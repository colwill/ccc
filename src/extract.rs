//! tree-sitter based extraction of symbols from a single source file

use crate::languages::Language;
use crate::model::{Const, Func, Note, Ref};
use std::collections::HashMap;
use tree_sitter::{Node, Parser};

/// result of walking one file's syntax tree
pub struct Extracted {
    pub consts: Vec<Const>,
    pub funcs: Vec<Func>,
    pub refs: Vec<Ref>,
    pub notes: Vec<Note>,
}

/// call site discovered during the walk resolved against same-file
/// functions after the walk completes
struct RawCall {
    caller: String,
    call_line: usize,
    callee: String,
}

struct Ctx<'a> {
    lang: Language,
    src: &'a str,
    consts: Vec<Const>,
    funcs: Vec<Func>,
    notes: Vec<Note>,
    calls: Vec<RawCall>,
    /// name -> (line, col, return type) for resolving refs.
    func_index: HashMap<String, (usize, usize, Option<String>)>,
    /// stack of enclosing function names for caller attribution.
    caller_stack: Vec<String>,
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
        func_index: HashMap::new(),
        caller_stack: Vec::new(),
    };
    visit(tree.root_node(), &mut ctx);

    // resolve calls to same-file function definitions
    let mut refs = Vec::new();
    for c in &ctx.calls {
        if let Some((line, col, ret)) = ctx.func_index.get(&c.callee) {
            refs.push(Ref {
                caller: c.caller.clone(),
                call_line: c.call_line,
                target_line: *line,
                target_col: *col,
                target_name: c.callee.clone(),
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
    let mut pushed = false;

    if lang.func_kinds().contains(&kind) {
        if let Some(func) = extract_func(node, ctx) {
            let name = func.name.clone();
            ctx.func_index
                .entry(name.clone())
                .or_insert((func.line, func.col, func.ret.clone()));
            ctx.funcs.push(func);
            ctx.caller_stack.push(name);
            pushed = true;
        }
    } else if lang.const_kinds().contains(&kind) {
        // only treat as a module-level constant when not inside a function body
        if ctx.caller_stack.is_empty() {
            extract_consts(node, ctx);
        }
    } else if lang.call_kinds().contains(&kind) {
        if let Some((callee, line)) = extract_call(node, ctx) {
            let caller = ctx
                .caller_stack
                .last()
                .cloned()
                .unwrap_or_else(|| "<top>".to_string());
            ctx.calls.push(RawCall {
                caller,
                call_line: line,
                callee,
            });
        }
    } else if lang.comment_kinds().contains(&kind) {
        maybe_note(node, ctx);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, ctx);
    }
    if pushed {
        ctx.caller_stack.pop();
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
                    let ty = node
                        .child_by_field_name("type")
                        .map(|n| oneline(text(n, ctx.src)));
                    ctx.consts.push(Const {
                        line: pos(left).0,
                        name: oneline(text(left, ctx.src)),
                        ty,
                    });
                }
            }
        }
        Language::JavaScript | Language::TypeScript | Language::Tsx => {
            let mut cursor = node.walk();
            for decl in node.children(&mut cursor) {
                if decl.kind() != "variable_declarator" {
                    continue;
                }
                // function-valued declarators are captured as functions instead
                // this should be okay I guess until I find a better way to capture these
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
                        t.strip_prefix(':').map(|s| s.trim().to_string()).unwrap_or(t)
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

/// returns (callee simple name, call-site line) for a call node
fn extract_call(node: Node, ctx: &Ctx) -> Option<(String, usize)> {
    let callee = node.child_by_field_name("function")?;
    let name = simple_callee_name(callee, ctx.src)?;
    let line = pos(callee).0;
    Some((name, line))
}

/// reduce a call target expression to a bare function/method name
fn simple_callee_name(node: Node, src: &str) -> Option<String> {
    match node.kind() {
        "identifier" | "field_identifier" | "property_identifier" | "type_identifier" => {
            Some(oneline(text(node, src)))
        }
        // rs: `a.b()`
        "field_expression" => node
            .child_by_field_name("field")
            .map(|n| oneline(text(n, src))),
        // rs: `a::b()`
        "scoped_identifier" => node
            .child_by_field_name("name")
            .map(|n| oneline(text(n, src))),
        // py: `a.b()`
        "attribute" => node
            .child_by_field_name("attribute")
            .map(|n| oneline(text(n, src))),
        // ts: `a.b()`
        "member_expression" => node
            .child_by_field_name("property")
            .map(|n| oneline(text(n, src))),
        // go: `a.b()`
        "selector_expression" => node
            .child_by_field_name("field")
            .map(|n| oneline(text(n, src))),
        // rs: `foo::<T>()`
        "generic_function" => node
            .child_by_field_name("function")
            .and_then(|f| simple_callee_name(f, src)),
        _ => None,
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
    if node.start_position().row.saturating_sub(cur.end_position().row) > 1 {
        return None;
    }
    // doc comments are often a run of single-line comments (`///` in Rust)
    // walk to the topmost adjacent one so we use the summary line
    while let Some(prev) = cur.prev_sibling() {
        if is_comment(&prev) && cur.start_position().row.saturating_sub(prev.end_position().row) <= 1
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
        // hopefully this doesnt cause problems later as a lambda should be
        // const or in C++ terms a constexpr :S
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
}
