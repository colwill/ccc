//! tree-sitter based extraction of symbols from a single source file

use crate::languages::Language;
use crate::model::{Const, Func, Note, Ref};
use std::collections::HashMap;
use tree_sitter::{Node, Parser};

pub struct Extracted {
    pub consts: Vec<Const>,
    pub funcs: Vec<Func>,
    pub refs: Vec<Ref>,
    pub notes: Vec<Note>,
}

enum CallKind {
    Free(String),
    Method { ty: String, name: String },
}

struct RawCall {
    caller: String,
    call_line: usize,
    kind: CallKind,
}

enum Scope {
    Func(String),
    /// `recv` used to recognise self-calls
    Type { name: String, recv: Option<String> },
}

struct Ctx<'a> {
    lang: Language,
    src: &'a str,
    consts: Vec<Const>,
    funcs: Vec<Func>,
    notes: Vec<Note>,
    calls: Vec<RawCall>,
    free_index: HashMap<String, (usize, usize, Option<String>)>,
    method_index: HashMap<(String, String), (usize, usize, Option<String>)>,
    scope_stack: Vec<Scope>,
}

impl Ctx<'_> {
    /// nearest enclosing function name for caller attribution
    fn caller(&self) -> String {
        self.scope_stack
            .iter()
            .rev()
            .find_map(|s| match s {
                Scope::Func(n) => Some(n.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "<top>".to_string())
    }

    /// nearest enclosing type scope (its name and receiver token) used to
    /// resolve self-calls to a method of that type
    fn current_type(&self) -> Option<(String, Option<String>)> {
        self.scope_stack.iter().rev().find_map(|s| match s {
            Scope::Type { name, recv } => Some((name.clone(), recv.clone())),
            _ => None,
        })
    }

    fn in_function(&self) -> bool {
        self.scope_stack.iter().any(|s| matches!(s, Scope::Func(_)))
    }

    fn in_type(&self) -> bool {
        self.scope_stack
            .iter()
            .any(|s| matches!(s, Scope::Type { .. }))
    }
}

/// parse `src` as `lang` and extract its symbols returns `None` if the source
/// cannot be parsed at all
pub fn extract(lang: Language, src: &str) -> Option<Extracted> {
    let mut parser = Parser::new();
    parser.set_language(&lang.ts_language()).ok()?;
    let tree = parser.parse(src, None)?;

    let mut ctx = Ctx {
        lang,
        src,
        consts: Vec::new(),
        funcs: Vec::new(),
        notes: Vec::new(),
        calls: Vec::new(),
        free_index: HashMap::new(),
        method_index: HashMap::new(),
        scope_stack: Vec::new(),
    };
    visit(tree.root_node(), &mut ctx);

    // resolve calls to same-file definitions
    let mut refs = Vec::new();
    for c in &ctx.calls {
        let resolved = match &c.kind {
            CallKind::Free(name) => ctx.free_index.get(name).map(|v| (name.clone(), v)),
            CallKind::Method { ty, name } => ctx
                .method_index
                .get(&(ty.clone(), name.clone()))
                .map(|v| (name.clone(), v)),
        };
        if let Some((target_name, (line, col, ret))) = resolved {
            refs.push(Ref {
                caller: c.caller.clone(),
                call_line: c.call_line,
                target_line: *line,
                target_col: *col,
                target_name,
                target_ret: ret.clone(),
            });
        }
    }
    // deterministic order + dedupe identical call sites
    refs.sort_by(|a, b| {
        (a.call_line, &a.caller, &a.target_name).cmp(&(b.call_line, &b.caller, &b.target_name))
    });
    refs.dedup_by(|a, b| {
        a.call_line == b.call_line && a.caller == b.caller && a.target_name == b.target_name
    });

    ctx.funcs.sort_by_key(|f| (f.line, f.col));
    ctx.consts.sort_by_key(|c| c.line);
    ctx.notes.sort_by_key(|n| n.line);

    Some(Extracted {
        consts: ctx.consts,
        funcs: ctx.funcs,
        refs,
        notes: ctx.notes,
    })
}

fn visit(node: Node, ctx: &mut Ctx) {
    let kind = node.kind();
    let lang = ctx.lang;
    let mut pushed = 0usize;

    if let Some((name, recv)) = type_scope(node, ctx) {
        ctx.scope_stack.push(Scope::Type { name, recv });
        pushed += 1;
    } else if lang.func_kinds().contains(&kind) {
        if let Some(func) = extract_func(node, ctx) {
            let owner = func_owner(node, ctx);
            let entry = (func.line, func.col, func.ret.clone());
            match &owner {
                Some((ty, _)) => {
                    ctx.method_index
                        .entry((ty.clone(), func.name.clone()))
                        .or_insert(entry);
                }
                None => {
                    ctx.free_index.entry(func.name.clone()).or_insert(entry);
                }
            }
            let name = func.name.clone();
            ctx.funcs.push(func);
            // go has to be special, methods carry their receiver on the definition rather than
            // nesting inside a type node ugh...
            if lang == Language::Go && kind == "method_declaration" {
                if let Some((ty, recv)) = owner {
                    ctx.scope_stack.push(Scope::Type { name: ty, recv });
                    pushed += 1;
                }
            }
            ctx.scope_stack.push(Scope::Func(name));
            pushed += 1;
        }
    } else if lang.const_kinds().contains(&kind) {
        if const_eligible(ctx) {
            extract_consts(node, ctx);
        }
    } else if lang.call_kinds().contains(&kind) {
        if let Some(call) = classify_call(node, ctx) {
            ctx.calls.push(call);
        }
    } else if lang.comment_kinds().contains(&kind) {
        maybe_note(node, ctx);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, ctx);
    }
    for _ in 0..pushed {
        ctx.scope_stack.pop();
    }
}

fn type_scope(node: Node, ctx: &Ctx) -> Option<(String, Option<String>)> {
    let name_of = |field: &str| {
        node.child_by_field_name(field)
            .map(|n| oneline(text(n, ctx.src)))
    };
    match (ctx.lang, node.kind()) {
        (Language::Rust, "impl_item") => Some((name_of("type")?, Some("self".to_string()))),
        (Language::Rust, "trait_item") => Some((name_of("name")?, Some("self".to_string()))),
        (Language::Python, "class_definition") => {
            Some((name_of("name")?, Some("self".to_string())))
        }
        (
            Language::JavaScript | Language::TypeScript | Language::Tsx,
            "class_declaration" | "class" | "abstract_class_declaration",
        ) => Some((name_of("name")?, Some("this".to_string()))),
        _ => None,
    }
}

/// if it is a method the enclosing type scope or for go... the method's own receiver
fn func_owner(node: Node, ctx: &Ctx) -> Option<(String, Option<String>)> {
    if ctx.lang == Language::Go && node.kind() == "method_declaration" {
        return go_receiver(node, ctx.src);
    }
    match ctx.scope_stack.last() {
        Some(Scope::Type { name, recv }) => Some((name.clone(), recv.clone())),
        _ => None,
    }
}

fn go_receiver(node: Node, src: &str) -> Option<(String, Option<String>)> {
    let recv = node.child_by_field_name("receiver")?;
    let mut cursor = recv.walk();
    for child in recv.children(&mut cursor) {
        if child.kind() != "parameter_declaration" {
            continue;
        }
        let ty_node = child.child_by_field_name("type")?;
        let ty = oneline(text(ty_node, src));
        let ty = ty.trim_start_matches(['*', '&']).trim().to_string();
        let var = child
            .child_by_field_name("name")
            .map(|n| oneline(text(n, src)));
        return Some((ty, var));
    }
    None
}

fn const_eligible(ctx: &Ctx) -> bool {
    if ctx.in_function() {
        return false;
    }
    match ctx.lang {
        Language::Python | Language::JavaScript | Language::TypeScript | Language::Tsx => {
            !ctx.in_type()
        }
        Language::Rust | Language::Go => true,
    }
}

/// extract a function definition; returns `None` for anonymous functions we
/// cannot name
fn extract_func(node: Node, ctx: &Ctx) -> Option<Func> {
    let (name, name_node) = func_name(node, ctx)?;
    let (line, col) = pos(name_node);
    let ret = func_return(node, ctx);
    let comment = preceding_comment(node, ctx);
    Some(Func {
        line,
        col,
        name,
        ret,
        comment,
    })
}

fn func_name<'a>(node: Node<'a>, ctx: &Ctx) -> Option<(String, Node<'a>)> {
    if let Some(n) = node.child_by_field_name("name") {
        return Some((oneline(text(n, ctx.src)), n));
    }
    // lambda function expression bound to a variable declarator
    if matches!(node.kind(), "arrow_function" | "function_expression") {
        let parent = node.parent()?;
        if parent.kind() == "variable_declarator" {
            if let Some(n) = parent.child_by_field_name("name") {
                return Some((oneline(text(n, ctx.src)), n));
            }
        }
    }
    None
}

fn func_return(node: Node, ctx: &Ctx) -> Option<String> {
    let field = ctx.lang.return_field()?;
    let n = node.child_by_field_name(field)?;
    let mut t = oneline(text(n, ctx.src));
    // typeScript return type node is a postfix `type_annotation` (`: T`)
    if let Some(stripped) = t.strip_prefix(':') {
        t = stripped.trim().to_string();
    }
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

fn extract_consts(node: Node, ctx: &mut Ctx) {
    match ctx.lang {
        Language::Rust => {
            if let Some(name) = node.child_by_field_name("name") {
                let ty = node
                    .child_by_field_name("type")
                    .map(|n| oneline(text(n, ctx.src)));
                ctx.consts.push(Const {
                    line: pos(name).0,
                    name: oneline(text(name, ctx.src)),
                    ty,
                });
            }
        }
        Language::Python => {
            if let Some(left) = node.child_by_field_name("left") {
                if left.kind() == "identifier" {
                    let name = oneline(text(left, ctx.src));
                    // Python has no real const; use the SHOUTY_SNEK_CASE
                    if !is_shouting_snek(&name) {
                        return;
                    }
                    let ty = node
                        .child_by_field_name("type")
                        .map(|n| oneline(text(n, ctx.src)));
                    ctx.consts.push(Const {
                        line: pos(left).0,
                        name,
                        ty,
                    });
                }
            }
        }
        Language::JavaScript | Language::TypeScript | Language::Tsx => {
            if node.kind() != "lexical_declaration" || !has_const_keyword(node) {
                return;
            }
            let mut cursor = node.walk();
            for decl in node.children(&mut cursor) {
                if decl.kind() != "variable_declarator" {
                    continue;
                }
                // function-valued declarators are captured as functions instead
                if let Some(v) = decl.child_by_field_name("value") {
                    if matches!(v.kind(), "arrow_function" | "function_expression") {
                        continue;
                    }
                }
                if let Some(name) = decl.child_by_field_name("name") {
                    if name.kind() != "identifier" {
                        continue;
                    }
                    let ty = decl.child_by_field_name("type").map(|n| {
                        let t = oneline(text(n, ctx.src));
                        t.strip_prefix(':')
                            .map(|s| s.trim().to_string())
                            .unwrap_or(t)
                    });
                    ctx.consts.push(Const {
                        line: pos(name).0,
                        name: oneline(text(name, ctx.src)),
                        ty,
                    });
                }
            }
        }
        Language::Go => {
            // const_spec / var_spec: identifiers before an optional type
            let ty = node
                .child_by_field_name("type")
                .map(|n| oneline(text(n, ctx.src)));
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "identifier" {
                    ctx.consts.push(Const {
                        line: pos(child).0,
                        name: oneline(text(child, ctx.src)),
                        ty: ty.clone(),
                    });
                }
            }
        }
    }
}


fn is_shouting_snek(name: &str) -> bool {
    name.chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && name.chars().any(|c| c.is_ascii_uppercase())
}

fn has_const_keyword(node: Node) -> bool {
    let mut cursor = node.walk();
    let is_const = node.children(&mut cursor).any(|c| c.kind() == "const");
    is_const
}

fn classify_call(node: Node, ctx: &Ctx) -> Option<RawCall> {
    let callee = node.child_by_field_name("function")?;
    let kind = resolve_callee(callee, ctx)?;
    Some(RawCall {
        caller: ctx.caller(),
        call_line: pos(callee).0,
        kind,
    })
}

/// reduce a call target expression to a [`CallKind`]
fn resolve_callee(node: Node, ctx: &Ctx) -> Option<CallKind> {
    let src = ctx.src;
    match node.kind() {
        "identifier" | "type_identifier" => Some(CallKind::Free(oneline(text(node, src)))),
        // rs: `a.b()` / `self.b()`
        "field_expression" => {
            let obj = node.child_by_field_name("value")?;
            let name = oneline(text(node.child_by_field_name("field")?, src));
            self_method(obj, name, ctx)
        }
        // rs: `a::b()` / `Self::b()`
        "scoped_identifier" => {
            let name = oneline(text(node.child_by_field_name("name")?, src));
            let path = node
                .child_by_field_name("path")
                .map(|n| oneline(text(n, src)));
            if matches!(path.as_deref(), Some("Self" | "self")) {
                ctx.current_type()
                    .map(|(ty, _)| CallKind::Method { ty, name })
            } else {
                None
            }
        }
        // py: `a.b()` / `self.b()`
        "attribute" => {
            let obj = node.child_by_field_name("object")?;
            let name = oneline(text(node.child_by_field_name("attribute")?, src));
            self_method(obj, name, ctx)
        }
        // ts/js: `a.b()` / `this.b()`
        "member_expression" => {
            let obj = node.child_by_field_name("object")?;
            let name = oneline(text(node.child_by_field_name("property")?, src));
            self_method(obj, name, ctx)
        }
        // go: `a.b()` / `recv.b()`
        "selector_expression" => {
            let obj = node.child_by_field_name("operand")?;
            let name = oneline(text(node.child_by_field_name("field")?, src));
            self_method(obj, name, ctx)
        }
        // rs: `foo::<T>()`
        "generic_function" => node
            .child_by_field_name("function")
            .and_then(|f| resolve_callee(f, ctx)),
        _ => None,
    }
}

fn self_method(obj: Node, name: String, ctx: &Ctx) -> Option<CallKind> {
    let (ty, recv) = ctx.current_type()?;
    let recv = recv?;
    if oneline(text(obj, ctx.src)) == recv {
        Some(CallKind::Method { ty, name })
    } else {
        None
    }
}

const MARKERS: &[&str] = &["TODO", "FIXME", "XXX", "HACK", "BUG", "NOTE", "SAFETY"];

fn maybe_note(node: Node, ctx: &mut Ctx) {
    let raw = text(node, ctx.src);
    let body = strip_comment(raw);
    // Match markers on word boundaries so `notes` / `notation` don't trigger.
    let has_marker = body
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|word| MARKERS.contains(&word.to_ascii_uppercase().as_str()));
    if !has_marker {
        return;
    }
    let one = oneline(&body);
    if one.is_empty() {
        return;
    }
    ctx.notes.push(Note {
        line: pos(node).0,
        text: truncate(&one, 160),
    });
}

/// nearest comment immediately preceding a function definition, used as its
/// one-line inline/doc comment
fn preceding_comment(node: Node, ctx: &Ctx) -> Option<String> {
    let is_comment = |n: &Node| ctx.lang.comment_kinds().contains(&n.kind());

    let mut cur = node.prev_sibling()?;
    if !is_comment(&cur) {
        return None;
    }
    // must be directly above the definition (allow the line right before)
    if node
        .start_position()
        .row
        .saturating_sub(cur.end_position().row)
        > 1
    {
        return None;
    }
    // doc comments are often a run of single-line comments (`///` in Rust)
    // walk to the topmost adjacent one so we use the summary line
    while let Some(prev) = cur.prev_sibling() {
        if is_comment(&prev)
            && cur
                .start_position()
                .row
                .saturating_sub(prev.end_position().row)
                <= 1
        {
            cur = prev;
        } else {
            break;
        }
    }
    let body = strip_comment(text(cur, ctx.src));
    // use the first non-empty line (the conventional summary)
    let line = body.lines().find(|l| !l.trim().is_empty())?;
    let one = oneline(line);
    if one.is_empty() {
        None
    } else {
        Some(truncate(&one, 100))
    }
}

// small helpers funcs

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    &src[node.byte_range()]
}

/// 1-based (line, column) of a node's start
fn pos(node: Node) -> (usize, usize) {
    let p = node.start_position();
    (p.row + 1, p.column + 1)
}

/// collapse all runs of whitespace to single spaces and trim
fn oneline(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// strip common comment delimiters from a raw comment token
fn strip_comment(raw: &str) -> String {
    let mut s = raw.trim();
    for p in ["///", "//!", "//", "#!", "#", "/**", "/*", "*/"] {
        if let Some(rest) = s.strip_prefix(p) {
            s = rest;
            break;
        }
    }
    let s = s.trim_end_matches("*/").trim();
    // for block comments drop leading `*` on continuation lines
    s.lines()
        .map(|l| l.trim().trim_start_matches('*').trim())
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_extraction() {
        let src = "const MAX: usize = 10;\n\
                   /// Doc summary.\n\
                   /// second line.\n\
                   fn helper(x: i32) -> i32 { x + 1 }\n\
                   fn run() { let _ = helper(MAX as i32); }\n";
        let ex = extract(Language::Rust, src).unwrap();

        assert_eq!(ex.consts.len(), 1);
        assert_eq!(ex.consts[0].name, "MAX");
        assert_eq!(ex.consts[0].ty.as_deref(), Some("usize"));

        let helper = ex.funcs.iter().find(|f| f.name == "helper").unwrap();
        assert_eq!(helper.ret.as_deref(), Some("i32"));
        // The summary (first) line of a multi-line doc comment is used.
        assert_eq!(helper.comment.as_deref(), Some("Doc summary."));

        assert!(ex
            .refs
            .iter()
            .any(|r| r.caller == "run" && r.target_name == "helper"));
    }

    #[test]
    fn go_const_block_and_call() {
        let src = "package main\n\
                   const Version string = \"1.0\"\n\
                   func greet(n string) string { return n }\n\
                   func main() { greet(Version) }\n";
        let ex = extract(Language::Go, src).unwrap();
        assert!(ex.consts.iter().any(|c| c.name == "Version"));
        assert!(ex
            .refs
            .iter()
            .any(|r| r.caller == "main" && r.target_name == "greet"));
    }

    #[test]
    fn ts_arrow_is_a_func() {
        let src = "const add = (a: number, b: number): number => a + b;\n\
                   function main(): void { add(1, 2); }\n";
        let ex = extract(Language::TypeScript, src).unwrap();
        // the lambda is recorded as a func, not a const.
        assert!(ex.funcs.iter().any(|f| f.name == "add"));
        assert!(!ex.consts.iter().any(|c| c.name == "add"));
        assert!(ex.refs.iter().any(|r| r.target_name == "add"));
    }

    #[test]
    fn note_marker_word_boundary() {
        // "notes" must not trigger, but a real TODO must
        let src = "// these are notes about the code\n\
                   fn f() {}\n\
                   // TODO: fix this\n\
                   fn g() {}\n";
        let ex = extract(Language::Rust, src).unwrap();
        assert_eq!(ex.notes.len(), 1);
        assert!(ex.notes[0].text.contains("TODO"));
    }

    #[test]
    fn qualified_call_does_not_bind_to_local_name() {
        // A method call `x.parse()` must NOT resolve to the free `fn parse`
        // just because the names match; only the bare `parse()` should.
        let src = "fn parse() -> i32 { 0 }\n\
                   fn run(x: String) {\n\
                       let _ = x.parse();\n\
                       let _ = parse();\n\
                   }\n";
        let ex = extract(Language::Rust, src).unwrap();
        let hits: Vec<_> = ex
            .refs
            .iter()
            .filter(|r| r.caller == "run" && r.target_name == "parse")
            .collect();
        // exactly one edge: the bare `parse()`, not `x.parse()`
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].call_line, 4);
    }

    #[test]
    fn self_method_calls_resolve_by_type() {
        // `self.helper()` resolves to the method on the same type; a same-named
        // free function is a distinct target.
        let src = "struct S;\n\
                   impl S {\n\
                       fn helper(&self) -> i32 { 1 }\n\
                       fn run(&self) -> i32 { self.helper() }\n\
                   }\n\
                   fn helper() {}\n";
        let ex = extract(Language::Rust, src).unwrap();
        assert!(ex.refs.iter().any(|r| r.caller == "run"
            && r.target_name == "helper"
            && r.target_ret.as_deref() == Some("i32")));
    }

    #[test]
    fn python_only_shouting_snek_is_const() {
        // nb explicit `\n` + spaces so the class body indentation survives
        let src = "MAX_SIZE = 10\nratio = 1.5\nclass C:\n    ATTR = 3\n";
        let ex = extract(Language::Python, src).unwrap();
        assert!(ex.consts.iter().any(|c| c.name == "MAX_SIZE"));
        // lowercase module var filtered out
        assert!(!ex.consts.iter().any(|c| c.name == "ratio"));
        // class attribute is not a module const
        assert!(!ex.consts.iter().any(|c| c.name == "ATTR"));
    }

    #[test]
    fn js_let_is_not_a_const() {
        let src = "const KEEP = 1;\n\
                   let drop = 2;\n\
                   var also = 3;\n";
        let ex = extract(Language::JavaScript, src).unwrap();
        assert!(ex.consts.iter().any(|c| c.name == "KEEP"));
        assert!(!ex.consts.iter().any(|c| c.name == "drop"));
        assert!(!ex.consts.iter().any(|c| c.name == "also"));
    }
}
