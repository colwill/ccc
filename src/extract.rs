//! tree-sitter based extraction of symbols from a single source file

use crate::languages::Language;
use crate::model::{CallSite, Const, Func, Import, Note, Ref};
use std::collections::HashMap;
use tree_sitter::{Node, Parser};

pub struct Extracted {
    pub consts: Vec<Const>,
    pub funcs: Vec<Func>,
    pub refs: Vec<Ref>,
    pub notes: Vec<Note>,
    // every call site in loose (name + qualifier) form, for `surf`
    pub calls: Vec<CallSite>,
    // qualified constant-like usages that are not calls (enum variants,
    // module consts, scoped types), same loose form
    pub uses: Vec<CallSite>,
    // import/use/include statements in loose textual form
    pub imports: Vec<Import>,
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
    // qualified non-call usages, same loose form
    uses: Vec<CallSite>,
    // import/use/include statements
    imports: Vec<Import>,
    free_index: HashMap<String, (usize, usize, Option<String>)>,
    method_index: HashMap<(String, String), (usize, usize, Option<String>)>,
    scope_stack: Vec<Scope>,
    // > 0 while inside a Rust `mod tests`-style container
    test_mod_depth: usize,
    // > 0 while inside an import/use/include declaration
    import_depth: usize,
    // names of enclosing Python Enum-subclass definitions; members are
    // const-like definitions typed by the innermost one
    enum_types: Vec<String>,
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
        uses: Vec::new(),
        imports: Vec::new(),
        free_index: HashMap::new(),
        method_index: HashMap::new(),
        scope_stack: Vec::new(),
        test_mod_depth: 0,
        import_depth: 0,
        enum_types: Vec::new(),
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
    ctx.uses
        .sort_by(|a, b| (a.line, &a.name).cmp(&(b.line, &b.name)));
    ctx.imports.sort_by_key(|i| i.line);

    Some(Extracted {
        consts: ctx.consts,
        funcs: ctx.funcs,
        refs,
        notes: ctx.notes,
        calls: ctx.loose_calls,
        uses: ctx.uses,
        imports: ctx.imports,
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
    let import = lang.import_kinds().contains(&kind);
    if import {
        if ctx.import_depth == 0 {
            extract_import(node, ctx);
        }
        ctx.import_depth += 1;
    }
    // `class Color(Enum):` members inside are variant declarations
    let py_enum = lang == Language::Python
        && kind == "class_definition"
        && node
            .child_by_field_name("superclasses")
            .map(|s| {
                text(s, ctx.src)
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .any(|seg| {
                        matches!(seg, "Enum" | "IntEnum" | "StrEnum" | "Flag" | "IntFlag")
                    })
            })
            .unwrap_or(false);
    if py_enum {
        let name = node
            .child_by_field_name("name")
            .map(|n| oneline(text(n, ctx.src)))
            .unwrap_or_default();
        ctx.enum_types.push(name);
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
        // python Enum members live in a class body, which const_eligible
        // rejects - admit them when directly inside an Enum subclass
        let py_enum_member =
            lang == Language::Python && !ctx.enum_types.is_empty() && !ctx.in_function();
        if const_eligible(ctx) || py_enum_member {
            extract_consts(node, ctx);
        }
    } else if lang.variant_kinds().contains(&kind) {
        if !ctx.in_function() {
            extract_variant(node, ctx);
        }
    } else if matches!(lang, Language::TypeScript | Language::Tsx) && kind == "enum_declaration" {
        if !ctx.in_function() {
            extract_ts_enum(node, ctx);
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
    } else if lang.use_kinds().contains(&kind) {
        maybe_use(node, ctx);
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
    if import {
        ctx.import_depth -= 1;
    }
    if py_enum {
        ctx.enum_types.pop();
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
                        .map(|n| oneline(text(n, ctx.src)))
                        // Enum members are typed by their owning class
                        .or_else(|| ctx.enum_types.last().cloned());
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

// index one enum variant/enumerator as a const-like definition
fn extract_variant(node: Node, ctx: &mut Ctx) {
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    // rust: enum_variant < enum_variant_list < enum_item(name);
    // cpp: enumerator < enumerator_list < enum_specifier(name)
    let ty = node
        .parent()
        .and_then(|list| list.parent())
        .and_then(|e| e.child_by_field_name("name"))
        .map(|n| oneline(text(n, ctx.src)));
    ctx.consts.push(Const {
        line: pos(name).0,
        name: oneline(text(name, ctx.src)),
        ty,
    });
}

// ts `enum Color { Red, Green = 2 }` - members become const-like definitions
// typed by the enum's name
fn extract_ts_enum(node: Node, ctx: &mut Ctx) {
    let ty = node
        .child_by_field_name("name")
        .map(|n| oneline(text(n, ctx.src)));
    let Some(body) = node.child_by_field_name("body") else {
        return;
    };
    let mut cursor = body.walk();
    for member in body.children(&mut cursor) {
        let name_node = match member.kind() {
            "property_identifier" => Some(member),
            "enum_assignment" => member.child_by_field_name("name"),
            _ => None,
        };
        if let Some(n) = name_node {
            ctx.consts.push(Const {
                line: pos(n).0,
                name: oneline(text(n, ctx.src)),
                ty: ty.clone(),
            });
        }
    }
}

// record a qualified non-call usage (`Encoding::O200kBase`, `http.StatusOK`)
// so `references`/`find` cover enum variants and consts, not just calls. Only
// constant-like names (leading uppercase / SHOUTING_SNEK) are kept - lowercase
// field and property accesses would drown the map in noise.
fn maybe_use(node: Node, ctx: &mut Ctx) {
    if ctx.import_depth > 0 {
        return;
    }
    if let Some(parent) = node.parent() {
        // the callee of a call is already recorded as a call site
        let is_callee = ctx.lang.call_kinds().contains(&parent.kind())
            && parent
                .child_by_field_name("function")
                .is_some_and(|f| f.id() == node.id());
        // nested paths (`a::b::C`) record only at the outermost node
        if is_callee || ctx.lang.use_kinds().contains(&parent.kind()) {
            return;
        }
    }
    let Some((qualifier, name)) = loose_name(node, ctx) else {
        return;
    };
    if qualifier.is_none() || name.is_empty() {
        return;
    }
    let constant_like = name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        || is_shouting_snek(&name);
    if !constant_like {
        return;
    }
    ctx.uses.push(CallSite {
        caller: ctx.caller(),
        line: pos(node).0,
        name,
        qualifier,
        test_ctx: ctx.test_mod_depth > 0,
    });
}

// record an import/use/include statement in loose textual form: a module path
// plus the names it binds locally. Parsed from the statement text rather than
// the grammar so one function covers every language; `dependencies` matches
// module segments and bound names against project file stems, so over-collecting
// identifiers is harmless while under-collecting loses type-only edges.
fn extract_import(node: Node, ctx: &mut Ctx) {
    let line = pos(node).0;
    let stmt = oneline(text(node, ctx.src));
    for (module, names) in parse_import(ctx.lang, &stmt) {
        ctx.imports.push(Import {
            line,
            module,
            names,
        });
    }
}

// split one import statement into (module, bound names) pairs.
fn parse_import(lang: Language, stmt: &str) -> Vec<(String, Vec<String>)> {
    let idents = |s: &str| -> Vec<String> {
        s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|w| !w.is_empty() && !matches!(*w, "as" | "self" | "type" | "crate" | "super"))
            .map(str::to_string)
            .collect()
    };
    // last `::`/`.`-separated segment of a path item, resolving `a as b` to
    // both the real name and the alias
    let item_names = |item: &str| -> Vec<String> {
        let item = item.trim();
        if item.is_empty() || item == "*" {
            return Vec::new();
        }
        let (path, alias) = match item.split_once(" as ") {
            Some((p, a)) => (p.trim(), Some(a.trim())),
            None => (item, None),
        };
        let mut names = idents(path).last().cloned().into_iter().collect::<Vec<_>>();
        if let Some(a) = alias {
            names.extend(idents(a));
        }
        names
    };
    match lang {
        Language::Rust => {
            let Some(s) = stmt
                .trim_start_matches("pub")
                .trim()
                .strip_prefix("use")
            else {
                return Vec::new();
            };
            let s = s.trim().trim_end_matches(';').trim();
            match s.split_once('{') {
                Some((prefix, rest)) => {
                    let module = prefix.trim().trim_end_matches("::").to_string();
                    let inner = rest.rsplit_once('}').map(|(i, _)| i).unwrap_or(rest);
                    // flatten nested groups to identifiers - module segments
                    // that leak in only ever match real module files
                    vec![(module, idents(inner))]
                }
                None => match s.strip_suffix("::*") {
                    Some(module) => vec![(module.to_string(), Vec::new())],
                    None => {
                        let module = s.rsplit_once("::").map(|(m, _)| m).unwrap_or("");
                        vec![(module.to_string(), item_names(s))]
                    }
                },
            }
        }
        Language::Python => {
            if let Some(s) = stmt.trim().strip_prefix("from ") {
                let Some((module, items)) = s.split_once(" import ") else {
                    return Vec::new();
                };
                let names = items
                    .trim()
                    .trim_matches(|c| c == '(' || c == ')')
                    .split(',')
                    .flat_map(item_names)
                    .collect();
                vec![(module.trim().to_string(), names)]
            } else if let Some(s) = stmt.trim().strip_prefix("import ") {
                s.split(',')
                    .filter(|i| !i.trim().is_empty())
                    .map(|i| {
                        let module = i
                            .trim()
                            .split_once(" as ")
                            .map(|(m, _)| m.trim())
                            .unwrap_or(i.trim());
                        (module.to_string(), item_names(i))
                    })
                    .collect()
            } else {
                Vec::new()
            }
        }
        Language::JavaScript | Language::TypeScript | Language::Tsx => {
            // module is the quoted source; bound names are everything between
            // `import` and `from` (default, namespace, and named bindings)
            let module = stmt
                .split(['\'', '"'])
                .nth(1)
                .unwrap_or("")
                .to_string();
            let bound = stmt
                .trim()
                .strip_prefix("import")
                .map(|s| s.split(" from ").next().unwrap_or(s))
                .unwrap_or("");
            vec![(module, idents(bound))]
        }
        Language::Go => {
            // one or more `alias "path/to/pkg"` specs; the bound name is the
            // alias when present, else the last path segment
            let mut out = Vec::new();
            let mut parts = stmt.split('"');
            let mut before = parts.next().unwrap_or("").to_string();
            while let (Some(path), Some(after)) = (parts.next(), parts.next()) {
                let alias = idents(&before).pop().filter(|a| a != "import");
                let names = alias
                    .map(|a| vec![a])
                    .unwrap_or_else(|| idents(path).last().cloned().into_iter().collect());
                out.push((path.to_string(), names));
                before = after.to_string();
            }
            out
        }
        Language::Cpp => {
            if let Some(s) = stmt.trim().strip_prefix("#include") {
                let module = s
                    .trim()
                    .trim_matches(|c| c == '"' || c == '<' || c == '>')
                    .to_string();
                vec![(module, Vec::new())]
            } else if let Some(s) = stmt.trim().strip_prefix("using") {
                let s = s.trim().trim_start_matches("namespace").trim();
                let s = s.trim_end_matches(';').trim();
                let module = s.rsplit_once("::").map(|(m, _)| m).unwrap_or(s);
                vec![(module.to_string(), item_names(s))]
            } else {
                Vec::new()
            }
        }
    }
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
        // rs `a::b` (value position) / `module::Type` (type position)
        "scoped_identifier" | "scoped_type_identifier" => named(
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
    fn rust_imports_are_captured() {
        let src = "use crate::model::{CallSite, Const};\n\
                   use crate::{extract, naming};\n\
                   use std::path::PathBuf;\n\
                   use crate::render as r;\n\
                   use crate::languages::*;\n";
        let ex = extract(Language::Rust, src).unwrap();
        let by_module: Vec<(&str, Vec<&str>)> = ex
            .imports
            .iter()
            .map(|i| {
                (
                    i.module.as_str(),
                    i.names.iter().map(String::as_str).collect(),
                )
            })
            .collect();
        assert_eq!(
            by_module,
            vec![
                ("crate::model", vec!["CallSite", "Const"]),
                ("crate", vec!["extract", "naming"]),
                ("std::path", vec!["PathBuf"]),
                ("crate", vec!["render", "r"]),
                ("crate::languages", vec![]),
            ]
        );
    }

    #[test]
    fn python_and_js_imports_are_captured() {
        let py = "import os.path\n\
                  from mypkg.util import shrink, grow as g\n";
        let ex = extract(Language::Python, py).unwrap();
        assert_eq!(ex.imports[0].module, "os.path");
        assert_eq!(ex.imports[0].names, vec!["path"]);
        assert_eq!(ex.imports[1].module, "mypkg.util");
        assert_eq!(ex.imports[1].names, vec!["shrink", "grow", "g"]);

        let js = "import def, { a, b as c } from './util/helper';\n";
        let ex = extract(Language::JavaScript, js).unwrap();
        assert_eq!(ex.imports[0].module, "./util/helper");
        assert_eq!(ex.imports[0].names, vec!["def", "a", "b", "c"]);
    }

    #[test]
    fn go_and_cpp_imports_are_captured() {
        let go = "package main\n\
                  import (\n\
                      \"fmt\"\n\
                      alias \"path/to/pkg\"\n\
                  )\n";
        let ex = extract(Language::Go, go).unwrap();
        assert_eq!(ex.imports[0].module, "fmt");
        assert_eq!(ex.imports[0].names, vec!["fmt"]);
        assert_eq!(ex.imports[1].module, "path/to/pkg");
        assert_eq!(ex.imports[1].names, vec!["alias"]);

        let cpp = "#include \"scan.h\"\n\
                   #include <vector>\n\
                   using ns::helper;\n";
        let ex = extract(Language::Cpp, cpp).unwrap();
        assert_eq!(ex.imports[0].module, "scan.h");
        assert!(ex.imports[0].names.is_empty());
        assert_eq!(ex.imports[1].module, "vector");
        assert_eq!(ex.imports[2].module, "ns");
        assert_eq!(ex.imports[2].names, vec!["helper"]);
    }

    #[test]
    fn rust_qualified_usages_are_captured() {
        let src = "use other::Thing;\n\
                   pub enum Encoding { O200kBase, Cl100kBase }\n\
                   fn parse(s: &str) -> Option<Encoding> {\n\
                       match s { \"o200k\" => Some(Encoding::O200kBase), _ => None }\n\
                   }\n\
                   fn label(e: Encoding) -> &'static str {\n\
                       match e { Encoding::O200kBase => \"o200k\", _ => \"other\" }\n\
                   }\n\
                   fn build() -> String { tiktoken::o200k_base() }\n";
        let ex = extract(Language::Rust, src).unwrap();

        // variant used as a value (call argument) and as a match pattern
        let uses: Vec<_> = ex.uses.iter().filter(|u| u.name == "O200kBase").collect();
        assert_eq!(uses.len(), 2);
        assert!(uses
            .iter()
            .all(|u| u.qualifier.as_deref() == Some("Encoding")));
        assert!(uses.iter().any(|u| u.caller == "parse"));
        assert!(uses.iter().any(|u| u.caller == "label"));
        // imports are not usages
        assert!(!ex.uses.iter().any(|u| u.name == "Thing"));
        // a qualified callee stays a call, and lowercase names are not uses
        assert!(!ex.uses.iter().any(|u| u.name == "o200k_base"));
        assert!(ex
            .calls
            .iter()
            .any(|c| c.name == "o200k_base" && c.qualifier.as_deref() == Some("tiktoken")));
        // the variant declarations are const-like definitions typed by enum
        assert!(ex
            .consts
            .iter()
            .any(|c| c.name == "O200kBase" && c.ty.as_deref() == Some("Encoding")));
        assert!(ex
            .consts
            .iter()
            .any(|c| c.name == "Cl100kBase" && c.ty.as_deref() == Some("Encoding")));
    }

    #[test]
    fn cpp_enumerators_are_const_defs() {
        let src = "enum class Color { Red, Green };\n";
        let ex = extract(Language::Cpp, src).unwrap();
        assert!(ex
            .consts
            .iter()
            .any(|c| c.name == "Red" && c.ty.as_deref() == Some("Color")));
    }

    #[test]
    fn ts_enum_members_and_usages() {
        let src = "enum Color { Red, Green = 2 }\n\
                   function paint(): Color { return Color.Red; }\n";
        let ex = extract(Language::TypeScript, src).unwrap();
        assert!(ex
            .consts
            .iter()
            .any(|c| c.name == "Red" && c.ty.as_deref() == Some("Color")));
        assert!(ex
            .consts
            .iter()
            .any(|c| c.name == "Green" && c.ty.as_deref() == Some("Color")));
        assert!(ex.uses.iter().any(|u| u.name == "Red"
            && u.qualifier.as_deref() == Some("Color")
            && u.caller == "paint"));
    }

    #[test]
    fn python_enum_class_members_are_const_defs() {
        let src = "from enum import Enum\n\
                   class Color(Enum):\n\
                   \x20   RED = 1\n\
                   \x20   GREEN = 2\n\
                   class Plain:\n\
                   \x20   SIZE = 4\n\
                   BORING = 3\n\
                   def paint():\n\
                   \x20   return Color.RED\n";
        let ex = extract(Language::Python, src).unwrap();
        // Enum members become const defs typed by their class
        assert!(ex
            .consts
            .iter()
            .any(|c| c.name == "RED" && c.ty.as_deref() == Some("Color")));
        // plain class attributes stay excluded, module consts unaffected
        assert!(!ex.consts.iter().any(|c| c.name == "SIZE"));
        assert!(ex.consts.iter().any(|c| c.name == "BORING"));
        // the usage pairs up via the attribute capture
        assert!(ex
            .uses
            .iter()
            .any(|u| u.name == "RED" && u.qualifier.as_deref() == Some("Color")));
    }

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
