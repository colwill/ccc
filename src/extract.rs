//! tree-sitter based extraction of symbols from a single source file

use crate::languages::Language;
use crate::model::{CallSite, Const, Func, Note, Ref};
use std::collections::HashMap;
use tree_sitter::{Node, Parser};

pub struct Extracted {
    pub consts: Vec<Const>,
    pub funcs: Vec<Func>,
    pub refs: Vec<Ref>,
    pub notes: Vec<Note>,
    // every call site in loose (name + qualifier) form, for `surf`
    pub calls: Vec<CallSite>,
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
    // `recv` used to recognise self-calls
    Type { name: String, recv: Option<String> },
}

struct Ctx<'a> {
    lang: Language,
    src: &'a str,
    consts: Vec<Const>,
    funcs: Vec<Func>,
    notes: Vec<Note>,
    calls: Vec<RawCall>,
    // every call in loose form (superset of `calls`), kept for `surf`
    loose_calls: Vec<CallSite>,
    free_index: HashMap<String, (usize, usize, Option<String>)>,
    method_index: HashMap<(String, String), (usize, usize, Option<String>)>,
    scope_stack: Vec<Scope>,
    // > 0 while inside a Rust `mod tests`-style container
    test_mod_depth: usize,
}

impl Ctx<'_> {
    // nearest enclosing function name for caller attribution
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

    // nearest enclosing type scope (its name and receiver token) used to
    // resolve self-calls to a method of that type
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

// parse `src` as `lang` and extract its symbols returns `None` if the source
// cannot be parsed at all
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
        loose_calls: Vec::new(),
        free_index: HashMap::new(),
        method_index: HashMap::new(),
        scope_stack: Vec::new(),
        test_mod_depth: 0,
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
    ctx.loose_calls
        .sort_by(|a, b| (a.line, &a.name).cmp(&(b.line, &b.name)));

    Some(Extracted {
        consts: ctx.consts,
        funcs: ctx.funcs,
        refs,
        notes: ctx.notes,
        calls: ctx.loose_calls,
    })
}

fn visit(node: Node, ctx: &mut Ctx) {
    let kind = node.kind();
    let lang = ctx.lang;
    let mut pushed = 0usize;

    // rust unit tests conventionally live in an inline `mod tests`; track it so
    // call sites inside are flagged as test context for `surf`
    let test_mod = lang == Language::Rust
        && kind == "mod_item"
        && node
            .child_by_field_name("name")
            .map(|n| text(n, ctx.src).to_ascii_lowercase().contains("test"))
            .unwrap_or(false);
    if test_mod {
        ctx.test_mod_depth += 1;
    }

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
            } else if lang == Language::Cpp {
                // out-of-line `Class::method` bodies aren't nested in the class,
                // so push its type scope to resolve `this->` calls within.
                if let Some((ty, recv)) = cpp_qualified_owner(node, ctx) {
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
        // independently record the loose form (kept even when the precise
        // classifier declines) - `surf` matches these across services
        if let Some(site) = loose_call(node, ctx) {
            ctx.loose_calls.push(site);
        }
    } else if lang.comment_kinds().contains(&kind) {
        maybe_note(node, ctx);
    } else if lang == Language::Rust && kind == "token_tree" {
        // macro bodies (`assert_eq!(charge(1), 31)`) are token trees, not
        // expressions - approximate the calls inside so `surf` sees them
        scan_macro_tokens(node, ctx);
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, ctx);
    }
    for _ in 0..pushed {
        ctx.scope_stack.pop();
    }
    if test_mod {
        ctx.test_mod_depth -= 1;
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
        (Language::Cpp, "class_specifier" | "struct_specifier") => {
            Some((name_of("name")?, Some("this".to_string())))
        }
        _ => None,
    }
}

// if it is a method the enclosing type scope or for go... the method's own receiver
fn func_owner(node: Node, ctx: &Ctx) -> Option<(String, Option<String>)> {
    if ctx.lang == Language::Go && node.kind() == "method_declaration" {
        return go_receiver(node, ctx.src);
    }
    // C++ methods are often defined out of line as `Class::method` rather than
    // nested inside the class body, so the owner comes from the qualified name.
    if ctx.lang == Language::Cpp {
        if let Some(owner) = cpp_qualified_owner(node, ctx) {
            return Some(owner);
        }
    }
    match ctx.scope_stack.last() {
        Some(Scope::Type { name, recv }) => Some((name.clone(), recv.clone())),
        _ => None,
    }
}

// declarator chain of a C++ `function_definition`, stepping through pointer /
// reference declarators to the innermost `function_declarator`
fn cpp_function_declarator(node: Node) -> Option<Node> {
    let mut cur = node.child_by_field_name("declarator")?;
    loop {
        match cur.kind() {
            "function_declarator" => return Some(cur),
            "pointer_declarator" | "reference_declarator" | "parenthesized_declarator" => {
                cur = cur.child_by_field_name("declarator")?;
            }
            _ => return None,
        }
    }
}

// For an out-of-line C++ definition `Class::method`, the owning type (`Class`)
// with a `this` receiver. Returns `None` for free functions and in-body methods.
fn cpp_qualified_owner(node: Node, ctx: &Ctx) -> Option<(String, Option<String>)> {
    let decl = cpp_function_declarator(node)?;
    let name = decl.child_by_field_name("declarator")?;
    if name.kind() == "qualified_identifier" {
        let scope = name.child_by_field_name("scope")?;
        return Some((oneline(text(scope, ctx.src)), Some("this".to_string())));
    }
    None
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
        // treat class/struct members as not module-level consts
        Language::Python
        | Language::JavaScript
        | Language::TypeScript
        | Language::Tsx
        | Language::Cpp => !ctx.in_type(),
        Language::Rust | Language::Go => true,
    }
}

// extract a function definition; returns `None` for anonymous functions we
// cannot name
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
        // whole-definition span (covers multi-line signatures and the body)
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        test_ctx: ctx.test_mod_depth > 0,
    })
}

fn func_name<'a>(node: Node<'a>, ctx: &Ctx) -> Option<(String, Node<'a>)> {
    // C++ has no `name` field: the name is buried in the declarator chain.
    if ctx.lang == Language::Cpp {
        return cpp_func_name(node, ctx);
    }
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

// C++ `function_definition` name: descend the declarator chain to the
// `function_declarator`, then read its name (identifier / field / qualified /
// operator / destructor).
fn cpp_func_name<'a>(node: Node<'a>, ctx: &Ctx) -> Option<(String, Node<'a>)> {
    let decl = cpp_function_declarator(node)?;
    cpp_declarator_name(decl.child_by_field_name("declarator")?, ctx)
}

// Resolve the various shapes a C++ declarator name can take to a display name
// and the node to anchor its line/column on.
fn cpp_declarator_name<'a>(node: Node<'a>, ctx: &Ctx) -> Option<(String, Node<'a>)> {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" | "operator_name"
        | "destructor_name" => Some((oneline(text(node, ctx.src)), node)),
        // `Class::method` -> use the trailing name; the scope is the owner
        "qualified_identifier" => {
            cpp_declarator_name(node.child_by_field_name("name")?, ctx)
        }
        // `foo<T>` templated definition
        "template_function" | "template_type" => {
            cpp_declarator_name(node.child_by_field_name("name")?, ctx)
        }
        _ => None,
    }
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
        Language::Cpp => {
            // only `const`/`constexpr`/... declarations count as consts; plain
            // `int x = 5;` and function prototypes are skipped.
            if !cpp_is_const_decl(node, ctx.src) {
                return;
            }
            let ty = node
                .child_by_field_name("type")
                .map(|n| oneline(text(n, ctx.src)));
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if let Some((name, name_node)) = cpp_const_declarator_name(child, ctx) {
                    ctx.consts.push(Const {
                        line: pos(name_node).0,
                        name,
                        ty: ty.clone(),
                    });
                }
            }
        }
    }
}

// True if a C++ `declaration` carries a `const`/`constexpr`/... qualifier.
fn cpp_is_const_decl(node: Node, src: &str) -> bool {
    let mut cursor = node.walk();
    let is_const = node.children(&mut cursor).any(|c| {
        matches!(
            text(c, src).trim(),
            "const" | "constexpr" | "constinit" | "consteval"
        )
    });
    is_const
}

// Name of a single C++ declarator within a `declaration`, if it is a plain
// (possibly initialised) variable. Skips function prototypes and other shapes.
fn cpp_const_declarator_name<'a>(child: Node<'a>, ctx: &Ctx) -> Option<(String, Node<'a>)> {
    let inner = match child.kind() {
        "init_declarator" => child.child_by_field_name("declarator")?,
        "identifier" => return Some((oneline(text(child, ctx.src)), child)),
        _ => return None,
    };
    cpp_plain_name(inner, ctx)
}

// Step through pointer / reference / array declarators to a bare identifier.
// Returns `None` on a `function_declarator` (a prototype, not a constant).
fn cpp_plain_name<'a>(node: Node<'a>, ctx: &Ctx) -> Option<(String, Node<'a>)> {
    let mut cur = node;
    loop {
        match cur.kind() {
            "identifier" => return Some((oneline(text(cur, ctx.src)), cur)),
            "pointer_declarator" | "reference_declarator" | "array_declarator"
            | "init_declarator" => {
                cur = cur.child_by_field_name("declarator")?;
            }
            _ => return None,
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

// record any call in loose form: rightmost identifier as the name, whatever
// sits left of it as the qualifier. Unlike [`resolve_callee`] this never
// requires the receiver to be `self`-like - `surf` wants the superset.
fn loose_call(node: Node, ctx: &Ctx) -> Option<CallSite> {
    let callee = node.child_by_field_name("function")?;
    let (qualifier, name) = loose_name(callee, ctx)?;
    if name.is_empty() {
        return None;
    }
    Some(CallSite {
        caller: ctx.caller(),
        line: pos(callee).0,
        name,
        qualifier,
        test_ctx: ctx.test_mod_depth > 0,
    })
}

// Approximate calls inside a Rust macro token tree: an identifier directly
// followed by a parenthesized token tree reads as a call (`charge(1)`); the
// qualifier is the `path::` / `recv.` chain walked back from it. Nested token
// trees are handled by the recursive visit.
fn scan_macro_tokens(node: Node, ctx: &mut Ctx) {
    for i in 0..node.child_count() {
        let Some(id) = node.child(i) else { continue };
        if id.kind() != "identifier" {
            continue;
        }
        let Some(next) = node.child(i + 1) else { continue };
        if next.kind() != "token_tree" || !text(next, ctx.src).starts_with('(') {
            continue;
        }
        // walk back over `<ident> ::` / `<ident> .` pairs
        let mut parts: Vec<String> = Vec::new();
        let mut j = i;
        while j >= 2 {
            match (node.child(j - 2), node.child(j - 1)) {
                (Some(q), Some(sep))
                    if matches!(sep.kind(), "::" | ".") && q.kind() == "identifier" =>
                {
                    parts.push(text(q, ctx.src).to_string());
                    j -= 2;
                }
                _ => break,
            }
        }
        parts.reverse();
        let qualifier = if parts.is_empty() {
            None
        } else {
            Some(parts.join("::"))
        };
        ctx.loose_calls.push(CallSite {
            caller: ctx.caller(),
            line: pos(id).0,
            name: oneline(text(id, ctx.src)),
            qualifier,
            test_ctx: ctx.test_mod_depth > 0,
        });
    }
}

// split a callee expression into (qualifier, rightmost name)
fn loose_name(node: Node, ctx: &Ctx) -> Option<(Option<String>, String)> {
    let src = ctx.src;
    let named = |q: Option<Node>, n: Node| {
        Some((q.map(|q| oneline(text(q, src))), oneline(text(n, src))))
    };
    match node.kind() {
        "identifier" | "type_identifier" | "field_identifier" => {
            Some((None, oneline(text(node, src))))
        }
        // rs `a.b` / cpp `a.b` `this->b`
        "field_expression" => named(
            node.child_by_field_name("value")
                .or_else(|| node.child_by_field_name("argument")),
            node.child_by_field_name("field")?,
        ),
        // rs `a::b`
        "scoped_identifier" => named(
            node.child_by_field_name("path"),
            node.child_by_field_name("name")?,
        ),
        // cpp `geo::square`
        "qualified_identifier" => {
            let name = node.child_by_field_name("name")?;
            // nested qualifiers (`a::b::c`) - recurse on the name side
            if name.kind() == "qualified_identifier" {
                return loose_name(name, ctx);
            }
            named(node.child_by_field_name("scope"), name)
        }
        // py `a.b`
        "attribute" => named(
            node.child_by_field_name("object"),
            node.child_by_field_name("attribute")?,
        ),
        // js/ts `a.b`
        "member_expression" => named(
            node.child_by_field_name("object"),
            node.child_by_field_name("property")?,
        ),
        // go `a.b`
        "selector_expression" => named(
            node.child_by_field_name("operand"),
            node.child_by_field_name("field")?,
        ),
        // rs `foo::<T>` / cpp `foo<T>`
        "generic_function" | "template_function" => node
            .child_by_field_name("function")
            .or_else(|| node.child_by_field_name("name"))
            .and_then(|f| loose_name(f, ctx)),
        _ => None,
    }
}

// reduce a call target expression to a [`CallKind`]
fn resolve_callee(node: Node, ctx: &Ctx) -> Option<CallKind> {
    let src = ctx.src;
    match node.kind() {
        "identifier" | "type_identifier" => Some(CallKind::Free(oneline(text(node, src)))),
        // rs: `a.b()` / `self.b()`  |  cpp: `a.b()` / `this->b()` (`argument` field)
        "field_expression" => {
            let obj = node
                .child_by_field_name("value")
                .or_else(|| node.child_by_field_name("argument"))?;
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

// nearest comment immediately preceding a function definition, used as its
// one-line inline/doc comment
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

// 1-based (line, column) of a node's start
fn pos(node: Node) -> (usize, usize) {
    let p = node.start_position();
    (p.row + 1, p.column + 1)
}

// collapse all runs of whitespace to single spaces and trim
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

// strip common comment delimiters from a raw comment token
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
                   // Doc summary.\n\
                   // second line.\n\
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
    fn rust_macro_calls_and_test_mod_are_loose_calls() {
        let src = "pub fn charge(c: u64) -> u64 { c }\n\
                   #[cfg(test)]\n\
                   mod tests {\n\
                       #[test]\n\
                       fn works() { assert_eq!(billing::charge(1), 1); }\n\
                   }\n";
        let ex = extract(Language::Rust, src).unwrap();
        // the call inside assert_eq! is captured, with its path qualifier,
        // and flagged as test context via the enclosing `mod tests`
        let call = ex
            .calls
            .iter()
            .find(|c| c.name == "charge")
            .expect("macro-wrapped call captured");
        assert_eq!(call.qualifier.as_deref(), Some("billing"));
        assert!(call.test_ctx);
        // the test fn itself carries test_ctx; the top-level fn does not
        assert!(ex.funcs.iter().any(|f| f.name == "works" && f.test_ctx));
        assert!(ex.funcs.iter().any(|f| f.name == "charge" && !f.test_ctx));
    }

    #[test]
    fn cpp_extraction() {
        let src = "const double PI = 3.14159;\n\
                   constexpr int SIDES = 4;\n\
                   int plain = 7;\n\
                   // Square a number.\n\
                   int square(int x) { return x * x; }\n\
                   int run() { return square(SIDES); }\n";
        let ex = extract(Language::Cpp, src).unwrap();

        // const / constexpr are consts; a plain (non-const) int is not.
        assert!(ex.consts.iter().any(|c| c.name == "PI"
            && c.ty.as_deref() == Some("double")));
        assert!(ex.consts.iter().any(|c| c.name == "SIDES"));
        assert!(!ex.consts.iter().any(|c| c.name == "plain"));

        let square = ex.funcs.iter().find(|f| f.name == "square").unwrap();
        assert_eq!(square.ret.as_deref(), Some("int"));
        assert_eq!(square.comment.as_deref(), Some("Square a number."));

        assert!(ex
            .refs
            .iter()
            .any(|r| r.caller == "run" && r.target_name == "square"));
    }

    #[test]
    fn cpp_out_of_line_method_and_self_call() {
        // `Circle::area` is defined outside the class body; it must be named
        // `area`, attributed to `Circle`, and `this->scaled()` must resolve to
        // the same class's method.
        let src = "class Circle {\n\
                   public:\n\
                       double area();\n\
                       double twice() { return this->scaled(2.0); }\n\
                   private:\n\
                       double scaled(double k) { return k; }\n\
                   };\n\
                   double Circle::area() { return this->scaled(1.0); }\n";
        let ex = extract(Language::Cpp, src).unwrap();

        // out-of-line definition is named by its trailing identifier, not qualified
        assert!(ex.funcs.iter().any(|f| f.name == "area"));
        // in-body `this->scaled()` resolves to the private method
        assert!(ex
            .refs
            .iter()
            .any(|r| r.caller == "twice" && r.target_name == "scaled"));
        // `this->scaled()` in the OUT-OF-LINE body resolves via the qualified
        // owner scope pushed for `Circle::area`.
        assert!(ex
            .refs
            .iter()
            .any(|r| r.caller == "area" && r.target_name == "scaled"));
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
