//! tree-sitter based extraction of symbols from a single source file

use crate::languages::Language;
use crate::model::{
    Annotation, Boundary, CallSite, Const, Func, FuncMetrics, Import, LoopInfo, Note, Ref, ResourceOp, TypeDef,
};
use std::collections::HashMap;
use tree_sitter::{Node, Parser};

// `CallSite::caller` for a call that sits outside any function
pub(crate) const TOP_LEVEL: &str = "<top>";

pub struct Extracted {
    pub consts: Vec<Const>,
    pub funcs: Vec<Func>,
    pub refs: Vec<Ref>,
    pub notes: Vec<Note>,
    // every call site in loose (name + qualifier) form, for `changes`
    pub calls: Vec<CallSite>,
    // qualified constant-like usages that are not calls (enum variants,
    // module consts, scoped types), same loose form
    pub uses: Vec<CallSite>,
    // import/use/include statements in loose textual form
    pub imports: Vec<Import>,
    // named type definitions, for `changes`'s type-directed resolution
    pub types: Vec<TypeDef>,
    // module identities declared here (go `package`, c++ `namespace`, rust `mod`)
    pub modules: Vec<String>,
    // `ccc:serves` / `ccc:calls` boundary hints written in comments
    pub annotations: Vec<Annotation>,
}

enum CallKind {
    Free(String),
    Method { ty: String, name: String },
}

struct RawCall {
    caller: String,
    call_line: usize,
    kind: CallKind,
    // type body the call sits in
    in_type: Option<String>,
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
    // every call in loose form (superset of `calls`), kept for `changes`
    loose_calls: Vec<CallSite>,
    // qualified non-call usages, same loose form
    uses: Vec<CallSite>,
    // import/use/include statements
    imports: Vec<Import>,
    types: Vec<TypeDef>,
    modules: Vec<String>,
    annotations: Vec<Annotation>,
    // 1-based lines of `ccc:skip` directives, resolved after the walk
    skips: Vec<usize>,
    // variable name -> declared type, one frame per lexical scope. Lets a
    // method call be attributed to its receiver's type instead of guessed at
    // from the method name alone.
    type_env: Vec<HashMap<String, String>>,
    free_index: HashMap<String, (usize, usize, Option<String>)>,
    method_index: HashMap<(String, String), (usize, usize, Option<String>)>,
    scope_stack: Vec<Scope>,
    // > 0 while inside a test scope: a Rust `mod tests`-style container or a
    // BDD test-registration callback (`test("...", () => { ... })`)
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
            .unwrap_or_else(|| TOP_LEVEL.to_string())
    }

    // declared type of a variable, searching innermost scope outwards
    fn type_of(&self, var: &str) -> Option<String> {
        self.type_env.iter().rev().find_map(|f| f.get(var).cloned())
    }

    fn bind(&mut self, var: String, ty: String) {
        if let Some(frame) = self.type_env.last_mut() {
            if !ty.is_empty() && !var.is_empty() {
                frame.insert(var, ty);
            }
        }
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
        types: Vec::new(),
        modules: Vec::new(),
        type_env: vec![HashMap::new()],
        free_index: HashMap::new(),
        method_index: HashMap::new(),
        annotations: Vec::new(),
        skips: Vec::new(),
        scope_stack: Vec::new(),
        test_mod_depth: 0,
        import_depth: 0,
        enum_types: Vec::new(),
    };
    visit(tree.root_node(), &mut ctx);

    // Honour `ccc:skip` before anything is derived from the walk: a marker on
    // a function removes that function, any other placement withdraws the
    // whole file - which reports the same `None` as an unparseable file, so
    // every consumer already handles it.
    if !apply_skips(&mut ctx) {
        return None;
    }

    // resolve calls to same-file definitions
    let mut refs = Vec::new();
    for c in &ctx.calls {
        let resolved = match &c.kind {
            // An unqualified call inside a class body means that class's own
            // method in C++, C# and Zig
            CallKind::Free(name) => {
                let sibling = c
                    .in_type
                    .as_ref()
                    .filter(|_| lang.implicit_member_scope())
                    .and_then(|ty| ctx.method_index.get(&(ty.clone(), name.clone())));
                sibling
                    .or_else(|| ctx.free_index.get(name))
                    .map(|v| (name.clone(), v))
            }
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
    // after `funcs` is final: a directive is bound to a definition by position
    ctx.annotations.sort_by_key(|a| a.line);
    bind_annotations(&mut ctx.annotations, &ctx.funcs);

    Some(Extracted {
        consts: ctx.consts,
        funcs: ctx.funcs,
        refs,
        notes: ctx.notes,
        calls: ctx.loose_calls,
        types: ctx.types,
        modules: ctx.modules,
        uses: ctx.uses,
        imports: ctx.imports,
        annotations: ctx.annotations,
    })
}

fn visit(node: Node, ctx: &mut Ctx) {
    let kind = node.kind();
    let lang = ctx.lang;
    let mut pushed = 0usize;

    // rust unit tests conventionally live in an inline `mod tests`; track it so
    // call sites inside are flagged as test context for `changes`
    let test_mod = lang == Language::Rust
        && kind == "mod_item"
        && node
            .child_by_field_name("name")
            .map(|n| text(n, ctx.src).to_ascii_lowercase().contains("test"))
            .unwrap_or(false);
    // BDD suites name their tests with a string, not an identifier
    // (`test("charge", () => { ... })`) - the callback is the test body, so it
    // is a test scope whose "name" is that label.
    let bdd_label = bdd_test_label(node, ctx);
    if test_mod || bdd_label.is_some() {
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
    // type definitions and module identities are indexed independently of the
    // dispatch below, because several of their node kinds (a Rust `trait_item`,
    // a C++ `class_specifier`, a TS `class_declaration`) are also type scopes
    // and would otherwise be swallowed by that branch
    if let Some(kind) = lang.type_kinds().iter().find(|(k, _)| *k == kind).map(|(_, v)| *v) {
        if let Some(name) = type_def_name(node, ctx) {
            ctx.types.push(TypeDef {
                line: pos(node).0,
                name: oneline(text(name, ctx.src)),
                kind: kind.to_string(),
            });
        }
    }
    // A test scope is a module the language recognises but not one the project
    // is built out of
    if lang.module_kinds().contains(&kind) && ctx.test_mod_depth == 0 {
        // a go `package_clause` has no `name` field; its identifier child is it
        let name = node
            .child_by_field_name("name")
            .or_else(|| node.named_child(0))
            .map(|n| oneline(text(n, ctx.src)));
        // `declare module "pkg"` names an external package's types, not a
        // module this project defines - its name arrives quoted, which is the
        // tell
        let local = |n: &String| !n.is_empty() && !n.starts_with('"') && !n.starts_with('\'');
        if let Some(name) = name.filter(local) {
            if !ctx.modules.contains(&name) {
                ctx.modules.push(name);
            }
        }
    }
    bind_declaration(node, ctx);

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
        } else if let Some(label) = &bdd_label {
            // anonymous callback, but the suite gave it a name
            ctx.scope_stack.push(Scope::Func(label.clone()));
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
    } else if lang == Language::Odin && kind == "enum_declaration" {
        if !ctx.in_function() {
            extract_odin_enum(node, ctx);
        }
    } else if lang.call_kinds().contains(&kind) {
        if let Some(call) = classify_call(node, ctx) {
            ctx.calls.push(call);
        }
        // independently record the loose form (kept even when the precise
        // classifier declines) - `changes` matches these across services
        if let Some(site) = loose_call(node, ctx) {
            ctx.loose_calls.push(site);
        }
    } else if lang.comment_kinds().contains(&kind) {
        maybe_note(node, ctx);
        maybe_annotations(node, ctx);
        maybe_skips(node, ctx);
    } else if lang == Language::Rust && kind == "token_tree" {
        // macro bodies (`assert_eq!(charge(1), 31)`) are token trees, not
        // expressions - approximate the calls inside so `changes` sees them
        scan_macro_tokens(node, ctx);
    } else if lang.use_kinds().contains(&kind) {
        maybe_use(node, ctx);
    }

    // one type-environment frame per scope, so a `let x: T` in an inner block
    // cannot leak its binding out to the enclosing function
    if pushed > 0 {
        ctx.type_env.push(HashMap::new());
        if lang.func_kinds().contains(&kind) {
            bind_signature(node, ctx);
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, ctx);
    }
    if pushed > 0 {
        ctx.type_env.pop();
    }
    for _ in 0..pushed {
        ctx.scope_stack.pop();
    }
    if test_mod || bdd_label.is_some() {
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
        (
            Language::CSharp,
            "class_declaration" | "struct_declaration" | "interface_declaration"
            | "record_declaration",
        ) => Some((name_of("name")?, Some("this".to_string()))),
        // zig methods live in an anonymous struct bound to a name receiver is `self`
        (Language::Zig, "struct_declaration" | "union_declaration" | "opaque_declaration") => {
            let name = type_def_name(node, ctx)?;
            Some((oneline(text(name, ctx.src)), Some("self".to_string())))
        }
        _ => None,
    }
}

// `static int Charge(this Client c, int amt)` 
// the `this` before Client qualifies as an extension method
fn csharp_extension_receiver(node: Node, ctx: &Ctx) -> Option<(String, Option<String>)> {
    let list = first_child_of_kind(node, "parameter_list")?;
    let first = list.named_children(&mut list.walk()).find(|p| p.kind() == "parameter")?;
    let mut cursor = first.walk();
    let extends = first
        .children(&mut cursor)
        .any(|c| c.kind() == "modifier" && text(c, ctx.src).trim() == "this");
    if !extends {
        return None;
    }
    let ty = oneline(text(first.child_by_field_name("type")?, ctx.src));
    let recv = first
        .child_by_field_name("name")
        .map(|n| oneline(text(n, ctx.src)));
    Some((ty, recv))
}

// if it is a method the enclosing type scope or for go... the method's own receiver
fn func_owner(node: Node, ctx: &Ctx) -> Option<(String, Option<String>)> {
    if ctx.lang == Language::Go && node.kind() == "method_declaration" {
        return go_receiver(node, ctx.src);
    }
    if ctx.lang == Language::CSharp {
        if let Some(owner) = csharp_extension_receiver(node, ctx) {
            return Some(owner);
        }
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
        | Language::Cpp
        | Language::C
        | Language::Zig => !ctx.in_type(),
        Language::CSharp => true,
        Language::Rust | Language::Go | Language::Odin => true,
    }
}

// extract a function definition; returns `None` for anonymous functions we
// cannot name
fn extract_func(node: Node, ctx: &Ctx) -> Option<Func> {
    let (name, name_node) = func_name(node, ctx)?;
    let (line, col) = pos(name_node);
    let ret = func_return(node, ctx);
    let comment = preceding_comment(node, ctx);
    let metrics = func_metrics(node, &name, ctx);
    let owner = func_owner(node, ctx).map(|(t, _)| normalize_type(&t));
    let param_types = param_pairs(node, ctx)
        .into_iter()
        .filter_map(|(_, t)| t)
        .map(|t| normalize_type(&t))
        .filter(|t| !t.is_empty())
        .collect();
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
        owner,
        param_types,
        metrics,
    })
}

// Reduce a declared type to the bare name a definition can be looked up by:
// strips reference/pointer/mutability sigils, unwraps the common single-type
// containers, drops generic arguments, and keeps the last path segment.
// `&mut Option<billing::Client<'a>>` -> `Client`.
pub fn normalize_type(raw: &str) -> String {
    // containers whose single type argument is the type actually being used
    const WRAPPERS: &[&str] = &[
        "Option", "Result", "Vec", "Box", "Rc", "Arc", "RefCell", "Cell", "Mutex", "RwLock",
        "shared_ptr", "unique_ptr", "weak_ptr", "vector", "optional", "Promise", "Array",
        "ReadonlyArray", "Partial", "Readonly",
    ];
    let mut s = raw.trim().to_string();
    for _ in 0..8 {
        let before = s.clone();
        s = s
            .trim()
            .trim_start_matches([':', '&', '*', '(', ' '])
            .trim_end_matches([';', '?', ')', ' ', ','])
            .trim()
            .to_string();
        for kw in ["mut ", "const ", "readonly ", "dyn ", "impl ", "static ", "final "] {
            if let Some(rest) = s.strip_prefix(kw) {
                s = rest.trim().to_string();
            }
        }
        // `T[]` / `[]T` (go slices) are still uses of T
        s = s.trim_end_matches("[]").trim().to_string();
        if let Some(rest) = s.strip_prefix("[]") {
            s = rest.trim().to_string();
        }
        // unwrap a container down to its first type argument
        if let Some((head, rest)) = s.split_once('<') {
            let head_name = head.rsplit(["::", "."].as_slice()[0]).next().unwrap_or(head);
            if WRAPPERS.contains(&head_name.trim()) {
                let inner = rest.strip_suffix('>').unwrap_or(rest);
                // first argument, ignoring lifetimes
                let first = inner
                    .split(',')
                    .map(str::trim)
                    .find(|a| !a.starts_with('\'') && !a.is_empty())
                    .unwrap_or(inner);
                s = first.to_string();
            }
        }
        if s == before {
            break;
        }
    }
    // drop any remaining generic arguments, then keep the last path segment
    if let Some((head, _)) = s.split_once('<') {
        s = head.to_string();
    }
    for sep in ["::", ".", "->"] {
        if let Some((_, last)) = s.rsplit_once(sep) {
            s = last.to_string();
        }
    }
    s.trim().to_string()
}

// (parameter name, declared type) for each parameter of a definition. Either
// side may be absent: an unannotated JS parameter has no type, a C++
// `void f(int)` has no name.
fn param_pairs(node: Node, ctx: &Ctx) -> Vec<(Option<String>, Option<String>)> {
    let kinds = ctx.lang.param_list_kinds();
    let mut list = None;
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if kinds.contains(&n.kind()) {
            list = Some(n);
            break;
        }
        let mut c = n.walk();
        stack.extend(n.children(&mut c));
    }
    let Some(list) = list else { return Vec::new() };
    let mut out = Vec::new();
    let mut cursor = list.walk();
    for p in list.named_children(&mut cursor) {
        if p.kind().contains("comment") {
            continue;
        }
        let name = p
            .child_by_field_name("pattern")
            .or_else(|| p.child_by_field_name("name"))
            .or_else(|| p.child_by_field_name("declarator"))
            .map(|n| oneline(text(n, ctx.src)))
            // a bare identifier parameter (go `x` in `x, y int`, js `x`)
            .or_else(|| (p.kind() == "identifier").then(|| oneline(text(p, ctx.src))))
            // odin labels neither side: `spec: string` is an identifier child
            // and a type child
            .or_else(|| {
                (ctx.lang == Language::Odin)
                    .then(|| first_child_of_kind(p, "identifier"))
                    .flatten()
                    .map(|n| oneline(text(n, ctx.src)))
            });
        let ty = p
            .child_by_field_name("type")
            .map(|n| oneline(text(n, ctx.src)))
            // rust/ts self-parameters name their own type via the impl scope
            .or_else(|| {
                p.kind().contains("self").then(|| {
                    ctx.current_type().map(|(t, _)| t).unwrap_or_default()
                })
            })
            .or_else(|| {
                (ctx.lang == Language::Odin)
                    .then(|| first_child_of_kind(p, "type"))
                    .flatten()
                    .map(|n| oneline(text(n, ctx.src)))
            });
        let name = name.map(|n| {
            // `&self` / `mut x` / `*p` reduce to the bound identifier
            n.trim_start_matches(['&', '*', ' '])
                .trim_start_matches("mut ")
                .trim()
                .to_string()
        });
        out.push((name.filter(|n| !n.is_empty()), ty.filter(|t| !t.is_empty())));
    }
    out
}

// Bind a definition's parameters and receiver into the current scope, so calls
// in the body can be attributed to the right type.
fn bind_signature(node: Node, ctx: &mut Ctx) {
    for (name, ty) in param_pairs(node, ctx) {
        if let (Some(name), Some(ty)) = (name, ty) {
            let ty = normalize_type(&ty);
            ctx.bind(name, ty);
        }
    }
    // go carries the receiver outside the parameter list; rust/c++/ts reach
    // their own type through `self`/`this`
    if let Some((ty, recv)) = func_owner(node, ctx) {
        if let Some(recv) = recv {
            ctx.bind(recv, normalize_type(&ty));
        }
        ctx.bind("self".into(), normalize_type(&ty));
        ctx.bind("this".into(), normalize_type(&ty));
    }
}

// Bind a local variable declaration: `let c: Client`, `c := Client{}`,
// `const c = new Client()`, `Client c;`. Only declared or directly-constructed
// types are recorded - an inferred `let c = make_client()` stays unknown
// rather than being guessed at.
fn bind_declaration(node: Node, ctx: &mut Ctx) {
    let kind = node.kind();
    let interesting = matches!(
        kind,
        "let_declaration"               // rust
            | "short_var_declaration"   // go `x := ...`
            | "var_spec"                // go `var x T`
            | "const_spec"
            | "variable_declarator"     // js/ts
            | "declaration"             // c++
            | "field_declaration"
    );
    if !interesting {
        return;
    }
    let src = ctx.src;
    let name_of = |n: Node| oneline(text(n, src));

    // an explicit annotation always wins over an inferred initialiser
    let declared = node
        .child_by_field_name("type")
        .map(name_of)
        .filter(|t| !t.is_empty());
    // otherwise look at the initialiser for a direct construction
    let value = node
        .child_by_field_name("value")
        .or_else(|| node.child_by_field_name("declarator"))
        .map(name_of);
    let ty = declared.or_else(|| value.as_deref().and_then(constructed_type));
    let Some(ty) = ty else { return };
    let ty = normalize_type(&ty);
    if ty.is_empty() {
        return;
    }

    // the bound name(s): a pattern, a name, or a c++ declarator
    let mut names = Vec::new();
    for field in ["pattern", "name", "declarator", "left"] {
        if let Some(n) = node.child_by_field_name(field) {
            names.push(name_of(n));
        }
    }
    // go binds several names at once (`var a, b T`)
    if names.is_empty() {
        let mut cursor = node.walk();
        for c in node.named_children(&mut cursor) {
            if c.kind() == "identifier" {
                names.push(name_of(c));
            }
        }
    }
    for raw in names {
        for name in raw.split(',') {
            let name = name
                .trim()
                .trim_start_matches(['&', '*', ' '])
                .trim_start_matches("mut ")
                .trim();
            // a c++ declarator carries the initialiser: `p = new Client()`
            let name = name.split(['=', '(', '[', ':']).next().unwrap_or(name).trim();
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                ctx.bind(name.to_string(), ty.clone());
            }
        }
    }
}

// The type a constructor-ish initialiser produces: `Client::new(..)`,
// `Client { .. }`, `new Client(..)`, `&Client{}`, `Client()`. Returns `None`
// for anything that needs real inference to know.
fn constructed_type(value: &str) -> Option<String> {
    let v = value.trim().trim_start_matches(['&', '*', ' ']).trim();
    // `new Client(...)` (ts/js/c++)
    if let Some(rest) = v.strip_prefix("new ") {
        let name = rest.split(['(', '{', '<', ' ']).next()?.trim();
        return (!name.is_empty()).then(|| name.to_string());
    }
    // `Client::new(...)` / `Client::default()` - the type is the qualifier
    if let Some((head, tail)) = v.split_once("::") {
        let ctor = tail.split(['(', ':']).next().unwrap_or("");
        if matches!(ctor, "new" | "default" | "from" | "with_capacity" | "create") {
            let name = head.rsplit("::").next()?.trim();
            return (!name.is_empty()).then(|| name.to_string());
        }
    }
    // a bare type name as the whole initialiser: a unit struct (`let l = Ledger;`)
    let bare = v.trim_end_matches(';').trim();
    let unit_struct = !bare.is_empty()
        && bare.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && bare.chars().all(|c| c.is_alphanumeric() || c == '_')
        // an ALL_CAPS name is a constant, not a type
        && bare.chars().any(|c| c.is_ascii_lowercase());
    if unit_struct {
        return Some(bare.to_string());
    }
    // `Client { .. }` struct literal (rust/go)
    let head = v.split(['{', '(']).next()?.trim();
    let is_type_name = head
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase())
        && head.chars().all(|c| c.is_alphanumeric() || c == '_' || c == ':');
    if is_type_name && (v.contains('{') || v.contains('(')) {
        return Some(head.rsplit("::").next()?.to_string());
    }
    None
}

// Measure a function body: size, decision points, loop nesting, and
// acquire/release calls. One extra walk of the definition's subtree, so a
// closure nested in a function is counted into that function too - which is
// what "how big is this body" should mean.
fn func_metrics(node: Node, own_name: &str, ctx: &Ctx) -> FuncMetrics {
    let mut m = FuncMetrics {
        body_lines: node.end_position().row - node.start_position().row + 1,
        params: count_params(node, ctx),
        ..FuncMetrics::default()
    };
    walk_metrics(node, ctx, own_name, 0, 0, &mut m);
    m.loops.sort_by_key(|l| l.line);
    m.resources.sort_by_key(|r| r.line);
    m
}

fn count_params(node: Node, ctx: &Ctx) -> usize {
    let kinds = ctx.lang.param_list_kinds();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if kinds.contains(&n.kind()) {
            // `self`/`this` receivers are parameters of the call, not the API
            return n
                .named_children(&mut n.walk())
                .filter(|c| !c.kind().contains("self"))
                .count();
        }
        let mut cursor = n.walk();
        stack.extend(n.children(&mut cursor));
    }
    0
}

fn walk_metrics(
    node: Node,
    ctx: &Ctx,
    own_name: &str,
    loop_depth: usize,
    guard_depth: usize,
    m: &mut FuncMetrics,
) {
    let lang = ctx.lang;
    let kind = node.kind();
    m.nodes += 1;

    let is_loop = lang.loop_kinds().contains(&kind);
    let depth = if is_loop {
        let d = loop_depth + 1;
        m.loops.push(LoopInfo {
            line: pos(node).0,
            kind: loop_label(kind),
            depth: d,
            trip: literal_trip(node, ctx),
        });
        d
    } else {
        loop_depth
    };
    let guard = guard_depth + usize::from(lang.guard_kinds().contains(&kind));

    if lang.branch_kinds().contains(&kind) {
        m.branches += 1;
    }
    if let Some(op) = resource_op(node, ctx, guard > 0) {
        m.resources.push(op);
    }
    if lang.call_kinds().contains(&kind) && callee_name(node, ctx).as_deref() == Some(own_name) {
        m.recursive = true;
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_metrics(child, ctx, own_name, depth, guard, m);
    }
}

// rightmost name of a call's callee (`billing::charge(1)` -> `charge`)
fn callee_name(call: Node, ctx: &Ctx) -> Option<String> {
    let callee = callee_node(call)?;
    loose_name(callee, ctx).map(|(_, name)| name).filter(|n| !n.is_empty())
}

fn loop_label(kind: &str) -> String {
    if kind.contains("comprehension") || kind.contains("generator") {
        "comprehension".into()
    } else if kind.starts_with("do_") {
        "do".into()
    } else if kind.starts_with("while") {
        "while".into()
    } else if kind.starts_with("loop") {
        "loop".into()
    } else {
        "for".into()
    }
}

// A resource acquire/release call, matched on the callee's rightmost name.
// C++ `new`/`delete` are operators rather than calls, so they are matched on
// their own node kinds.
fn resource_op(node: Node, ctx: &Ctx, guarded: bool) -> Option<ResourceOp> {
    let pairs = ctx.lang.resource_pairs();
    let line = pos(node).0;
    if ctx.lang == Language::Cpp {
        match node.kind() {
            "new_expression" => {
                return Some(ResourceOp {
                    line,
                    name: "new".into(),
                    pair: "new",
                    acquire: true,
                    guarded,
                })
            }
            "delete_expression" => {
                return Some(ResourceOp {
                    line,
                    name: "delete".into(),
                    pair: "new",
                    acquire: false,
                    guarded,
                })
            }
            _ => {}
        }
    }
    if !ctx.lang.call_kinds().contains(&node.kind()) {
        return None;
    }
    let name = callee_name(node, ctx)?;
    for (acq, rel) in pairs {
        if name == *acq {
            return Some(ResourceOp { line, name, pair: acq, acquire: true, guarded });
        }
        if name == *rel {
            return Some(ResourceOp { line, name, pair: acq, acquire: false, guarded });
        }
    }
    None
}

// Trip count of a counted loop when every bound is an integer literal. Read
// from the loop header text rather than the grammar so one routine covers all
// six languages; anything it cannot prove returns `None`.
fn literal_trip(node: Node, ctx: &Ctx) -> Option<usize> {
    // everything before the body: `for i := 0; i < 8` etc. Taken from the
    // grammar because the delimiter differs per language (`{`, `:`, newline)
    let header = match node.child_by_field_name("body") {
        Some(b) => ctx.src.get(node.start_byte()..b.start_byte())?,
        None => text(node, ctx.src),
    };
    let int = |s: &str| s.trim().parse::<usize>().ok();

    // `for i in 0..8` / `0..=8`
    if let Some(range) = header.split(" in ").nth(1) {
        if let Some((lo, hi)) = range.split_once("..") {
            let (hi, inclusive) = match hi.strip_prefix('=') {
                Some(rest) => (rest, true),
                None => (hi, false),
            };
            let hi = hi.split(|c: char| !c.is_ascii_digit()).find(|s| !s.is_empty())?;
            let (lo, hi) = (int(lo)?, int(hi)?);
            return hi.checked_sub(lo).map(|n| n + usize::from(inclusive));
        }
        // `for i in range(8)` / `range(2, 8)`
        if let Some(args) = range.split_once("range(").map(|(_, a)| a) {
            let args = args.split(')').next()?;
            let nums: Vec<usize> = args.split(',').filter_map(int).collect();
            return match (nums.len(), args.matches(',').count()) {
                (1, 0) => Some(nums[0]),
                (2, 1) => nums[1].checked_sub(nums[0]),
                _ => None,
            };
        }
        return None;
    }

    // C-style `for (i = 0; i < 8; i++)` - init, condition, step
    let parts: Vec<&str> = header.split(';').collect();
    if parts.len() < 2 {
        return None;
    }
    let start = int(parts[0].rsplit(['=', ':']).next()?)?;
    let cond = parts[1];
    let (op_inclusive, bound) = if let Some((_, b)) = cond.split_once("<=") {
        (true, b)
    } else if let Some((_, b)) = cond.split_once('<') {
        (false, b)
    } else {
        return None;
    };
    let end = int(bound)?;
    end.checked_sub(start).map(|n| n + usize::from(op_inclusive))
}

fn func_name<'a>(node: Node<'a>, ctx: &Ctx) -> Option<(String, Node<'a>)> {
    // C and C++ have no `name` field: the name is buried in the declarator
    // chain, behind any pointer/array/parenthesised layers the return type put
    // there.
    if matches!(ctx.lang, Language::Cpp | Language::C) {
        return cpp_func_name(node, ctx);
    }
    if let Some(n) = node.child_by_field_name("name") {
        return Some((oneline(text(n, ctx.src)), n));
    }
    // odin declares by binding a name to a procedure literal
    if ctx.lang == Language::Odin {
        if let Some(n) = first_child_of_kind(node, "identifier") {
            return Some((oneline(text(n, ctx.src)), n));
        }
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

// jest/mocha/vitest-style registrars: `test("name", () => {...})`. Suffixed
// forms (`it.only`, `test.each`, `describe.skip`) match on the head segment.
pub(crate) const BDD_REGISTRARS: &[&str] = &[
    "test", "it", "describe", "suite", "context", "specify", "bench",
];

// Label for an anonymous function passed to a BDD test registrar, e.g.
// `test("charge", () => { ... })` -> `test("charge")`. These callbacks are the
// only "test functions" JS/TS suites have - without this they are anonymous and
// every call inside them is attributed to `<top>`.
fn bdd_test_label(node: Node, ctx: &Ctx) -> Option<String> {
    if !matches!(
        ctx.lang,
        Language::JavaScript | Language::TypeScript | Language::Tsx
    ) || !matches!(node.kind(), "arrow_function" | "function_expression")
    {
        return None;
    }
    // the callback must be an argument of a call, not a bound variable
    let args = node.parent()?;
    if args.kind() != "arguments" {
        return None;
    }
    let call = args.parent()?;
    if call.kind() != "call_expression" {
        return None;
    }
    let callee = text(call.child_by_field_name("function")?, ctx.src);
    let head = callee
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .find(|s| !s.is_empty())?;
    if !BDD_REGISTRARS.contains(&head) {
        return None;
    }
    // the title is the first string-ish argument
    let mut cursor = args.walk();
    let title = args.children(&mut cursor).find_map(|c| {
        matches!(c.kind(), "string" | "template_string")
            .then(|| oneline(text(c, ctx.src)))
            .map(|t| t.trim_matches(['"', '\'', '`']).to_string())
    })?;
    if title.is_empty() {
        return None;
    }
    Some(format!("{head}(\"{title}\")"))
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

// First direct child of a given kind
fn first_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).find(|c| c.kind() == kind);
    found
}

// The node carrying a type definition's name
fn type_def_name<'a>(node: Node<'a>, ctx: &Ctx) -> Option<Node<'a>> {
    if let Some(n) = node.child_by_field_name("name") {
        return Some(n);
    }
    match ctx.lang {
        // `typedef struct {...} Codec;` names the type on the typedef
        Language::C | Language::Cpp if node.kind() == "type_definition" => {
            if node
                .child_by_field_name("type")
                .and_then(|t| t.child_by_field_name("name"))
                .is_some()
            {
                return None;
            }
            node.child_by_field_name("declarator")
                .filter(|d| d.kind() == "type_identifier")
        }
        // `pub const Codec = struct { ... };` - the struct is anonymous and the
        // name belongs to the binding around it
        Language::Zig => {
            let parent = node.parent()?;
            if parent.kind() != "variable_declaration" {
                return None;
            }
            first_child_of_kind(parent, "identifier")
        }
        // `Codec :: struct { ... }` - an unlabelled identifier child
        Language::Odin => first_child_of_kind(node, "identifier"),
        _ => None,
    }
}

fn func_return(node: Node, ctx: &Ctx) -> Option<String> {
    // odin wraps the signature in a `procedure` node
    if ctx.lang == Language::Odin {
        let proc = first_child_of_kind(node, "procedure")?;
        let t = first_child_of_kind(proc, "type")?;
        let t = oneline(text(t, ctx.src));
        return (!t.is_empty()).then_some(t);
    }
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
        Language::CSharp => {
            // a plain field is mutable state; `const` and `readonly` are the
            // two ways C# says "this will not change"
            let mut cursor = node.walk();
            let is_const = node
                .children(&mut cursor)
                .any(|c| matches!(text(c, ctx.src).trim(), "const" | "readonly"));
            if !is_const {
                return;
            }
            let Some(decl) = first_child_of_kind(node, "variable_declaration") else {
                return;
            };
            let ty = decl
                .child_by_field_name("type")
                .map(|n| oneline(text(n, ctx.src)));
            let mut cursor = decl.walk();
            for d in decl.children(&mut cursor) {
                if d.kind() != "variable_declarator" {
                    continue;
                }
                if let Some(name) = d.child_by_field_name("name") {
                    ctx.consts.push(Const {
                        line: pos(name).0,
                        name: oneline(text(name, ctx.src)),
                        ty: ty.clone(),
                    });
                }
            }
        }
        Language::Zig => {
            // `var` is mutable, so only `const` bindings are constants
            if first_child_of_kind(node, "const").is_none() {
                return;
            }
            // the same node shape also spells a type definition and an import;
            // both are recorded elsewhere and are not constants
            let mut cursor = node.walk();
            let bound_elsewhere = node.children(&mut cursor).any(|c| {
                ctx.lang.type_kinds().iter().any(|(k, _)| *k == c.kind())
                    || (c.kind() == "builtin_function"
                        && text(c, ctx.src).trim_start().starts_with("@import"))
            });
            if bound_elsewhere {
                return;
            }
            let Some(name) = first_child_of_kind(node, "identifier") else {
                return;
            };
            let ty = node
                .child_by_field_name("type")
                .map(|n| oneline(text(n, ctx.src)));
            ctx.consts.push(Const {
                line: pos(name).0,
                name: oneline(text(name, ctx.src)),
                ty,
            });
        }
        Language::Odin => {
            let Some(name) = first_child_of_kind(node, "identifier") else {
                return;
            };
            // `Handle :: distinct int` declares a type. It shares a node kind
            // with every other `::` binding, so the value is what tells them
            // apart.
            if first_child_of_kind(node, "distinct_type").is_some() {
                ctx.types.push(TypeDef {
                    line: pos(name).0,
                    name: oneline(text(name, ctx.src)),
                    kind: "alias".to_string(),
                });
                return;
            }
            let ty = first_child_of_kind(node, "type").map(|n| oneline(text(n, ctx.src)));
            ctx.consts.push(Const {
                line: pos(name).0,
                name: oneline(text(name, ctx.src)),
                ty,
            });
        }
        Language::Cpp | Language::C => {
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
    let callee = callee_node(node)?;
    let kind = resolve_callee(callee, ctx)?;
    Some(RawCall {
        caller: ctx.caller(),
        call_line: pos(callee).0,
        kind,
        in_type: ctx.current_type().map(|(ty, _)| ty),
    })
}

// record any call in loose form: rightmost identifier as the name, whatever
// sits left of it as the qualifier. Unlike [`resolve_callee`] this never
// requires the receiver to be `self`-like - `changes` wants the superset.
//
// The node naming what a call invokes. Every grammar here puts it on a
// `function` field except C#'s `new Client()`, where the thing being called is
// the type being constructed.
fn callee_node(call: Node) -> Option<Node> {
    call.child_by_field_name("function")
        .or_else(|| call.child_by_field_name("type"))
}

fn loose_call(node: Node, ctx: &Ctx) -> Option<CallSite> {
    let callee = callee_node(node)?;
    let (qualifier, name) = loose_name(callee, ctx)?;
    if name.is_empty() {
        return None;
    }
    // Odin nests a qualified call inside the member access rather than the
    // other way round
    let qualifier = qualifier.or_else(|| {
        if ctx.lang != Language::Odin {
            return None;
        }
        let parent = node.parent()?;
        if parent.kind() != "member_expression" {
            return None;
        }
        let q = parent.named_child(0)?;
        (q.id() != node.id()).then(|| oneline(text(q, ctx.src)))
    });
    Some(CallSite {
        caller: ctx.caller(),
        line: pos(callee).0,
        name,
        recv_type: receiver_type(callee, qualifier.as_deref(), ctx),
        qualifier,
        test_ctx: ctx.test_mod_depth > 0,
    })
}

// Declared type of a method call's receiver. `c.charge()` looks `c` up in the
// scope's type environment; `self.charge()` uses the enclosing type; a
// qualifier that is itself a known type name (`Client::new`) is that type.
// Everything else is `None` - an unknown receiver must not be guessed.
fn receiver_type(callee: Node, qualifier: Option<&str>, ctx: &Ctx) -> Option<String> {
    let q = qualifier?;
    if !matches!(
        callee.kind(),
        "field_expression" | "member_expression" | "selector_expression" | "attribute"
            | "scoped_identifier" | "qualified_identifier" | "member_access_expression"
    ) {
        return None;
    }
    let base = q
        .trim_start_matches(['&', '*', '(', ' '])
        .split(["->", "::", "."].as_slice()[0])
        .next()
        .unwrap_or(q)
        .trim();
    // `self.x` / `this.x`
    if matches!(base, "self" | "this" | "Self") {
        return ctx.current_type().map(|(t, _)| normalize_type(&t));
    }
    // a bound local, parameter or receiver
    if let Some(t) = ctx.type_of(base) {
        return Some(t);
    }
    // the qualifier is written as the type itself (`Client::new`, `Client.of`)
    let last = q.rsplit(["::", "."].as_slice()[0]).next().unwrap_or(q).trim();
    let looks_like_type = last.chars().next().is_some_and(|c| c.is_ascii_uppercase());
    if looks_like_type && ctx.types.iter().any(|t| t.name == last) {
        return Some(last.to_string());
    }
    None
}

// index one enum variant/enumerator as a const-like definition
fn extract_variant(node: Node, ctx: &mut Ctx) {
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    // zig spells an enum variant and a struct field the same way; only the
    // parent tells them apart
    if ctx.lang == Language::Zig {
        let Some(parent) = node.parent() else { return };
        if parent.kind() != "enum_declaration" {
            return;
        }
        let ty = type_def_name(parent, ctx).map(|n| oneline(text(n, ctx.src)));
        ctx.consts.push(Const {
            line: pos(name).0,
            name: oneline(text(name, ctx.src)),
            ty,
        });
        return;
    }
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

// odin `Mode :: enum { Fast, Small }` - the members are bare identifiers with
// no node kind of their own, so they are read straight off the declaration
fn extract_odin_enum(node: Node, ctx: &mut Ctx) {
    let ty = type_def_name(node, ctx).map(|n| oneline(text(n, ctx.src)));
    let mut cursor = node.walk();
    let members: Vec<Node> = node
        .children(&mut cursor)
        .filter(|c| c.kind() == "identifier")
        .collect();
    // the first identifier is the enum's own name, not a member
    for m in members.into_iter().skip(1) {
        ctx.consts.push(Const {
            line: pos(m).0,
            name: oneline(text(m, ctx.src)),
            ty: ty.clone(),
        });
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
        // a use is a value/type reference, not a call through a receiver
        recv_type: None,
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
    let reexport = is_reexport(ctx.lang, &stmt);
    for (module, names) in parse_import(ctx.lang, &stmt) {
        ctx.imports.push(Import {
            line,
            module,
            names,
            reexport,
        });
    }
}

// does the current statement (stmt) hand the names it binds onward
fn is_reexport(lang: Language, stmt: &str) -> bool {
    match lang {
        Language::Rust => stmt
            .trim_start()
            .strip_prefix("pub")
            .is_some_and(|rest| rest.starts_with(['(', ' ', '\t'])),
        _ => false,
    }
}

// split one import statement into (module, bound names) pairs.
// ccc:calls
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
            // `pub(crate) use` / `pub(super) use` are re-exports too, and used
            // to be dropped whole: stripping the bare `pub` left `(crate) use`,
            // which is not a `use` prefix, so the statement produced no import
            // and the names it binds were invisible to every lookup.
            let s = stmt.trim_start().strip_prefix("pub").unwrap_or(stmt).trim();
            let s = match s.strip_prefix('(') {
                Some(rest) => rest.split_once(')').map_or(s, |(_, tail)| tail).trim(),
                None => s,
            };
            let Some(s) = s.strip_prefix("use") else {
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
        Language::C => {
            let s = stmt.trim().strip_prefix("#include").unwrap_or("").trim();
            let module = s.trim_matches(|c| c == '"' || c == '<' || c == '>').to_string();
            if module.is_empty() {
                Vec::new()
            } else {
                vec![(module, Vec::new())]
            }
        }
        Language::CSharp => {
            // `using System.Text;`, `using static A.B;`, `using X = A.B;`
            let s = stmt.trim().trim_end_matches(';').trim();
            let Some(s) = s.strip_prefix("using") else {
                return Vec::new();
            };
            let s = s.trim().strip_prefix("static").unwrap_or(s).trim();
            match s.split_once('=') {
                // an alias binds the name on the left to the path on the right
                Some((alias, path)) => {
                    vec![(path.trim().to_string(), idents(alias))]
                }
                // a plain `using` opens a namespace without binding a name; the
                // module is what a qualifier can then reach through
                None => vec![(s.to_string(), Vec::new())],
            }
        }
        Language::Zig => {
            // only `@import(...)` bindings are imports - every other `const`
            // arrives here too, because they share a node kind
            let Some((bound, rest)) = stmt.split_once('=') else {
                return Vec::new();
            };
            if !rest.contains("@import") {
                return Vec::new();
            }
            let module = rest.split('"').nth(1).unwrap_or("").to_string();
            if module.is_empty() {
                return Vec::new();
            }
            // `const std = @import("std")` binds `std`; drop the keywords
            let names: Vec<String> = idents(bound)
                .into_iter()
                .filter(|w| !matches!(w.as_str(), "pub" | "const" | "var"))
                .collect();
            vec![(module, names)]
        }
        Language::Odin => {
            // `import "core:fmt"` / `import os "core:os"`
            let module = stmt.split('"').nth(1).unwrap_or("").to_string();
            if module.is_empty() {
                return Vec::new();
            }
            let before = stmt.split('"').next().unwrap_or("");
            let alias = idents(before).into_iter().find(|w| w != "import");
            // with no alias the package is bound under its last path segment:
            // `core:fmt` -> `fmt`
            let names = alias.map(|a| vec![a]).unwrap_or_else(|| {
                module
                    .rsplit([':', '/'])
                    .next()
                    .filter(|s| !s.is_empty())
                    .map(|s| vec![s.to_string()])
                    .unwrap_or_default()
            });
            vec![(module, names)]
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
            // macro token trees are approximated; no receiver typing there
            recv_type: None,
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
        // rs `a.b` / cpp `a.b` `this->b` / zig `a.b` (`object`/`member`)
        "field_expression" => named(
            node.child_by_field_name("value")
                .or_else(|| node.child_by_field_name("argument"))
                .or_else(|| node.child_by_field_name("object")),
            node.child_by_field_name("field")
                .or_else(|| node.child_by_field_name("member"))?,
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
        // c# `a.b`
        "member_access_expression" => named(
            node.child_by_field_name("expression"),
            node.child_by_field_name("name")?,
        ),
        // odin `a.b` labels neither side, and nests a qualified call the other
        // way up: `member_expression(a, call_expression(b(...)))`
        "member_expression" if ctx.lang == Language::Odin => {
            let mut cursor = node.walk();
            let kids: Vec<Node> = node.named_children(&mut cursor).collect();
            let (&q, &tail) = (kids.first()?, kids.last()?);
            let name = match tail.kind() {
                "call_expression" => tail.child_by_field_name("function")?,
                _ => tail,
            };
            named(Some(q), name)
        }
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
                .or_else(|| node.child_by_field_name("argument"))
                .or_else(|| node.child_by_field_name("object"))?;
            let field = node
                .child_by_field_name("field")
                .or_else(|| node.child_by_field_name("member"))?;
            self_method(obj, oneline(text(field, src)), ctx)
        }
        // c#: `a.b()` / `this.b()`
        "member_access_expression" => {
            let obj = node.child_by_field_name("expression")?;
            let name = oneline(text(node.child_by_field_name("name")?, src));
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

// counts only where an author would actually write one: as a whole
// word that opens the comment or follows whitespace, and that is punctuated
// with a colon
fn has_marker(body: &str) -> bool {
    let is_marker = |start: usize, end: usize| {
        if !MARKERS.contains(&body[start..end].to_ascii_uppercase().as_str()) {
            return false;
        }
        let opens = match body[..start].chars().next_back() {
            Some(c) => c.is_whitespace(),
            None => true,
        };
        // step over an owner group, `TODO(alice):`, before looking for the colon
        let tail = &body[end..];
        let tail = match tail.strip_prefix('(') {
            Some(rest) => match rest.find(')') {
                Some(close) => &rest[close + 1..],
                None => return false,
            },
            None => tail,
        };
        opens && tail.starts_with(':')
    };
    let mut word: Option<usize> = None;
    for (i, c) in body.char_indices() {
        match (c.is_ascii_alphanumeric(), word) {
            (true, None) => word = Some(i),
            (false, Some(start)) => {
                if is_marker(start, i) {
                    return true;
                }
                word = None;
            }
            _ => {}
        }
    }
    word.is_some_and(|start| is_marker(start, body.len()))
}

fn maybe_note(node: Node, ctx: &mut Ctx) {
    let raw = text(node, ctx.src);
    let body = strip_comment(raw);
    if !has_marker(&body) {
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

// Transports we name in prose. Anything else an author writes is still
// accepted - it just travels as part of the key rather than as a transport.
const TRANSPORTS: &[&str] = &[
    "grpc", "rest", "http", "https", "graphql", "queue", "event", "webhook", "ffi", "cli", "soap",
    "websocket", "ws", "sql", "rpc",
];

const ANNOTATION_PREFIX: &str = "ccc:";

// `ccc:serves grpc billing.v1.Charge` and `ccc:calls grpc billing.v1.Charge`,
// written in whatever comment syntax the language uses. One spelling for every
// language: `strip_comment` has already removed the delimiters by the time we
// see the body, so nothing here is language-specific.
//
// A comment can carry more than one directive when it is a block, so every
// line is examined.
fn maybe_annotations(node: Node, ctx: &mut Ctx) {
    let raw = text(node, ctx.src);
    if !raw.contains(ANNOTATION_PREFIX) {
        return;
    }
    let start = pos(node).0;
    let body = strip_comment(raw);
    for (offset, line) in body.lines().enumerate() {
        let Some(parsed) = parse_annotation(line) else {
            continue;
        };
        let (boundary, transport, key) = parsed;
        ctx.annotations.push(Annotation {
            line: start + offset,
            boundary,
            transport,
            key,
            // rewritten once the file's functions are known; a directive above
            // a definition sits outside it, so the enclosing scope is wrong
            function: String::new(),
        });
    }
}

// One directive, already stripped of its comment delimiters.
fn parse_annotation(line: &str) -> Option<(Boundary, String, String)> {
    let at = line.find(ANNOTATION_PREFIX)?;
    // must open the comment or follow whitespace, so `// see ccc:serves` and a
    // URL like `http://x/ccc:serves` are not directives
    if !line[..at]
        .chars()
        .next_back()
        .map_or(true, |c| c.is_whitespace())
    {
        return None;
    }
    let rest = &line[at + ANNOTATION_PREFIX.len()..];
    let mut words = rest.split_whitespace();
    let boundary = match words.next()?.to_ascii_lowercase().as_str() {
        "serves" | "provides" | "handles" => Boundary::Serves,
        "calls" | "consumes" | "uses" => Boundary::Calls,
        _ => return None,
    };

    let tail = rest
        .split_once(char::is_whitespace)
        .map(|(_, t)| t.trim())
        .unwrap_or_default();
    if tail.is_empty() {
        return None;
    }
    // A leading known transport is metadata; anything else is all key, so
    // `ccc:calls billing.v1.Charge` works without naming a transport.
    let (transport, key) = match tail.split_once(char::is_whitespace) {
        Some((head, rest)) if TRANSPORTS.contains(&head.to_ascii_lowercase().as_str()) => {
            (head.to_ascii_lowercase(), rest.trim())
        }
        _ if TRANSPORTS.contains(&tail.to_ascii_lowercase().as_str()) => {
            // a transport and nothing else names no key, so there is nothing
            // for the far end to match on
            return None;
        }
        _ => ("unspecified".to_string(), tail),
    };
    if key.is_empty() {
        return None;
    }
    Some((boundary, transport, truncate(key, 160)))
}

// `ccc:skip`, in whatever comment syntax the language uses: the delimiters
// are already gone by the time the body is examined, so `// ccc:skip`,
// `# ccc:skip`, `/* ccc:skip */` and `-- ccc:skip` all read the same. Trailing
// prose is allowed - `// ccc:skip generated` - so an author can say why.
fn maybe_skips(node: Node, ctx: &mut Ctx) {
    let raw = text(node, ctx.src);
    if !raw.contains(ANNOTATION_PREFIX) {
        return;
    }
    let start = pos(node).0;
    let body = strip_comment(raw);
    for (offset, line) in body.lines().enumerate() {
        if is_skip_directive(line) {
            ctx.skips.push(start + offset);
        }
    }
}

// Same guard as `parse_annotation`: the marker must open the comment or follow
// whitespace, so prose like `see ccc:skip` in a URL is not a directive.
fn is_skip_directive(line: &str) -> bool {
    let Some(at) = line.find(ANNOTATION_PREFIX) else {
        return false;
    };
    if !line[..at]
        .chars()
        .next_back()
        .map_or(true, |c| c.is_whitespace())
    {
        return false;
    }
    let rest = &line[at + ANNOTATION_PREFIX.len()..];
    rest.split_whitespace()
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("skip"))
}

// Resolve every `ccc:skip` the walk collected. Returns `false` when the whole
// file is withdrawn.
//
// Placement decides the scope, and the file is the default: at the very top
// of the file the marker withdraws the file, even when a definition opens it.
// Inside a function, or directly above one - attribute and decorator lines
// may sit between, a blank line may not - it withdraws just that function.
// Anywhere else it is a file-level statement and withdraws the file.
fn apply_skips(ctx: &mut Ctx) -> bool {
    if ctx.skips.is_empty() {
        return true;
    }
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for &line in &ctx.skips {
        let at_top = ctx
            .src
            .lines()
            .take(line.saturating_sub(1))
            .all(|l| l.trim().is_empty());
        if at_top {
            return false;
        }
        if let Some(f) = ctx
            .funcs
            .iter()
            .filter(|f| line >= f.start_line && line <= f.end_line)
            .max_by_key(|f| f.start_line)
        {
            spans.push((f.start_line, f.end_line));
            continue;
        }
        let below = ctx
            .funcs
            .iter()
            .filter(|f| f.start_line > line && f.start_line - line <= MAX_ANNOTATION_GAP)
            .min_by_key(|f| f.start_line)
            // "directly above": every line between the marker and the
            // definition is non-blank, so a detached comment stays file-wide
            .filter(|f| {
                ctx.src
                    .lines()
                    .skip(line)
                    .take(f.start_line - line - 1)
                    .all(|l| !l.trim().is_empty())
            });
        match below {
            Some(f) => spans.push((f.start_line, f.end_line)),
            None => return false,
        }
    }

    // Drop the skipped functions and everything the walk saw inside them,
    // including their entries in the call-target indices: a skipped function
    // is neither a definition nor a place calls resolve to.
    let hit = |l: usize| spans.iter().any(|&(s, e)| l >= s && l <= e);
    ctx.funcs.retain(|f| !hit(f.start_line));
    ctx.consts.retain(|c| !hit(c.line));
    ctx.notes.retain(|n| !hit(n.line));
    ctx.calls.retain(|c| !hit(c.call_line));
    ctx.loose_calls.retain(|c| !hit(c.line));
    ctx.uses.retain(|u| !hit(u.line));
    ctx.imports.retain(|i| !hit(i.line));
    ctx.annotations.retain(|a| !hit(a.line));
    ctx.free_index.retain(|_, &mut (l, _, _)| !hit(l));
    ctx.method_index.retain(|_, &mut (l, _, _)| !hit(l));
    true
}

// Bind each directive to a function.
//
// A directive inside a function body belongs to that function - that is where
// `ccc:calls` naturally sits, on the call it describes. A directive above a
// definition belongs to the definition below it, which is where `ccc:serves`
// naturally sits, in the doc block of a handler. `MAX_ANNOTATION_GAP` keeps a
// file-header comment from being claimed by the first function far below it.
const MAX_ANNOTATION_GAP: usize = 10;

fn bind_annotations(annotations: &mut [Annotation], funcs: &[Func]) {
    for ann in annotations.iter_mut() {
        // innermost enclosing function, if any
        let enclosing = funcs
            .iter()
            .filter(|f| ann.line >= f.start_line && ann.line <= f.end_line)
            .max_by_key(|f| f.start_line);
        if let Some(f) = enclosing {
            ann.function = f.name.clone();
            continue;
        }
        // otherwise the nearest definition below, allowing for the attribute
        // or decorator lines a language may put in between
        let below = funcs
            .iter()
            .filter(|f| f.start_line > ann.line && f.start_line - ann.line <= MAX_ANNOTATION_GAP)
            .min_by_key(|f| f.start_line);
        ann.function = match below {
            Some(f) => f.name.clone(),
            None => TOP_LEVEL.to_string(),
        };
    }
}

// nearest comment immediately preceding a function definition, used as its
// one-line inline/doc comment
fn preceding_comment(node: Node, ctx: &Ctx) -> Option<String> {
    let is_comment = |n: &Node| ctx.lang.comment_kinds().contains(&n.kind());

    // Climb out of anything wrapping the definition - a TS `export`, a python
    // decorated definition - because the comment is a sibling of the wrapper,
    // not of the definition inside it.
    let mut anchor = node;
    while let Some(parent) = anchor.parent() {
        if !ctx.lang.doc_wrapper_kinds().contains(&parent.kind()) {
            break;
        }
        anchor = parent;
    }
    // Then step back over any annotations written between the two: a Rust
    // `#[inline]`, a python `@staticmethod`, a C# `[Obsolete]`.
    let mut cur = anchor.prev_sibling()?;
    while ctx.lang.annotation_kinds().contains(&cur.kind()) {
        cur = cur.prev_sibling()?;
    }
    if !is_comment(&cur) {
        return None;
    }
    // must be directly above the definition, or above the annotations leading
    // to it (allow the line right before)
    if anchor
        .start_position()
        .row
        .saturating_sub(cur.end_position().row)
        > 1
        && !ctx.lang.annotation_kinds().is_empty()
        && cur.end_position().row + 1 < anchor.start_position().row
    {
        // the gap is only acceptable when annotations fill it
        let filled = {
            let mut n = cur.next_sibling();
            let mut ok = true;
            while let Some(x) = n {
                if x.id() == anchor.id() {
                    break;
                }
                if !ctx.lang.annotation_kinds().contains(&x.kind()) {
                    ok = false;
                    break;
                }
                n = x.next_sibling();
            }
            ok
        };
        if !filled {
            return None;
        }
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

    fn annotations_of(lang: Language, src: &str) -> Vec<(usize, &'static str, String, String, String)> {
        extract(lang, src)
            .expect("parse")
            .annotations
            .into_iter()
            .map(|a| (a.line, a.boundary.label(), a.transport, a.key, a.function))
            .collect()
    }

    // The whole point of the comment form: one spelling, every language, no
    // build-time dependency anywhere.
    #[test]
    fn boundary_hints_parse_in_every_language() {
        for (lang, src, want_fn) in [
            (
                Language::Rust,
                "// ccc:serves grpc billing.v1.Charge\npub fn charge(n: u64) -> u64 { n }\n",
                "charge",
            ),
            (
                Language::Go,
                "package p\n\n// ccc:serves grpc billing.v1.Charge\nfunc Charge(n int) int { return n }\n",
                "Charge",
            ),
            (
                Language::Python,
                "# ccc:serves grpc billing.v1.Charge\ndef charge(n):\n    return n\n",
                "charge",
            ),
            (
                Language::TypeScript,
                "// ccc:serves grpc billing.v1.Charge\nexport function charge(n: number) { return n; }\n",
                "charge",
            ),
            (
                Language::CSharp,
                "class C {\n  // ccc:serves grpc billing.v1.Charge\n  int Charge(int n) { return n; }\n}\n",
                "Charge",
            ),
        ] {
            let got = annotations_of(lang, src);
            assert_eq!(got.len(), 1, "{lang:?}: {got:?}");
            let (_, boundary, transport, key, function) = &got[0];
            assert_eq!(*boundary, "serves", "{lang:?}");
            assert_eq!(transport, "grpc", "{lang:?}");
            assert_eq!(key, "billing.v1.Charge", "{lang:?}");
            assert_eq!(function, want_fn, "{lang:?}");
        }
    }

    // A directive above a definition belongs to it; one inside a body belongs
    // to the function it sits in, which is where a call is described.
    #[test]
    fn a_hint_binds_above_a_definition_and_inside_a_body() {
        let got = annotations_of(
            Language::Rust,
            "// ccc:serves grpc a.Serve\npub fn outer() {\n    // ccc:calls grpc b.Call\n    inner();\n}\n",
        );
        assert_eq!(got[0].1, "serves");
        assert_eq!(got[0].4, "outer", "a hint above a definition names it");
        assert_eq!(got[1].1, "calls");
        assert_eq!(got[1].4, "outer", "a hint in a body names its function");
    }

    // Decorators and attributes sit between the comment and the definition,
    // and must not break the binding.
    #[test]
    fn a_hint_survives_decorators_and_attributes() {
        let py = annotations_of(
            Language::Python,
            "# ccc:serves rest GET /health\n@app.route(\"/health\")\n@cached\ndef health():\n    return 1\n",
        );
        assert_eq!(py[0].4, "health");
        assert_eq!(py[0].3, "GET /health", "the whole remainder is the key");

        let rs = annotations_of(
            Language::Rust,
            "/// Refund money.\n/// ccc:serves rest POST /refund\n#[inline]\npub fn refund(x: u64) -> u64 { x }\n",
        );
        assert_eq!(rs[0].4, "refund", "a directive below a doc summary still binds");
    }

    // The transport is optional; without one there is still a key to match on.
    #[test]
    fn a_hint_without_a_transport_is_all_key() {
        let got = annotations_of(Language::Rust, "// ccc:calls billing.v1.Charge\nfn f() {}\n");
        assert_eq!(got[0].2, "unspecified");
        assert_eq!(got[0].3, "billing.v1.Charge");
    }

    // A transport and nothing else names no key, so there is nothing for the
    // far end to match - better to ignore it than to invent an empty edge.
    #[test]
    fn a_hint_naming_only_a_transport_is_not_a_directive() {
        assert!(annotations_of(Language::Rust, "// ccc:calls grpc\nfn f() {}\n").is_empty());
    }

    // `ccc:` has to be written as a directive, not merely contained.
    #[test]
    fn prose_and_urls_are_not_directives() {
        for src in [
            "// see http://example.com/ccc:serves for details\nfn f() {}\n",
            "// notccc:serves grpc a.B\nfn f() {}\n",
            "// ccc:whatever grpc a.B\nfn f() {}\n",
        ] {
            assert!(annotations_of(Language::Rust, src).is_empty(), "{src}");
        }
    }

    // A file-header hint has no definition to attach to and must not be
    // dragged onto whatever function happens to appear much later.
    #[test]
    fn a_far_away_hint_stays_at_file_level() {
        let mut src = String::from("// ccc:serves grpc a.B\n");
        for _ in 0..20 {
            src.push_str("//\n");
        }
        src.push_str("fn much_later() {}\n");
        let got = annotations_of(Language::Rust, &src);
        assert_eq!(got[0].4, TOP_LEVEL);
    }

    struct LangFixture {
        lang: Language,
        src: &'static str,
        func: &'static str,
        ret: Option<&'static str>,
        doc: &'static str,
        caller: &'static str,
        konst: &'static str,
        ty: &'static str,
        module: Option<&'static str>,
        import: &'static str,
        complexity: usize,
    }

    const LANG_FIXTURES: &[LangFixture] = &[
        LangFixture {
            lang: Language::C,
            src: "#include <stdio.h>\n\
                  \n\
                  static const int LIMIT = 4;\n\
                  \n\
                  typedef struct Codec { int level; } Codec;\n\
                  \n\
                  // Parse the level.\n\
                  static int parse_level(const char* spec) {\n\
                  \x20   for (int i = 0; i < LIMIT; i++) {\n\
                  \x20       if (spec[i] == ' ') { return 1; }\n\
                  \x20   }\n\
                  \x20   return 0;\n\
                  }\n\
                  \n\
                  int drive(const char* spec) { return parse_level(spec); }\n",
            func: "parse_level",
            ret: Some("int"),
            doc: "Parse the level.",
            caller: "drive",
            konst: "LIMIT",
            ty: "Codec",
            module: None,
            import: "stdio.h",
            complexity: 3,
        },
        LangFixture {
            lang: Language::CSharp,
            src: "using System.Text;\n\
                  \n\
                  namespace Billing\n\
                  {\n\
                  \x20   public class Client\n\
                  \x20   {\n\
                  \x20       public const int Limit = 4;\n\
                  \n\
                  \x20       // Parse the level.\n\
                  \x20       public int ParseLevel(string spec)\n\
                  \x20       {\n\
                  \x20           for (int i = 0; i < Limit; i++)\n\
                  \x20           {\n\
                  \x20               if (spec.Length > 0) { return 1; }\n\
                  \x20           }\n\
                  \x20           return 0;\n\
                  \x20       }\n\
                  \n\
                  \x20       public int Drive(string spec) { return ParseLevel(spec); }\n\
                  \x20   }\n\
                  }\n",
            func: "ParseLevel",
            ret: Some("int"),
            doc: "Parse the level.",
            caller: "Drive",
            konst: "Limit",
            ty: "Client",
            module: Some("Billing"),
            import: "System.Text",
            complexity: 3,
        },
        LangFixture {
            lang: Language::Zig,
            src: "const std = @import(\"std\");\n\
                  \n\
                  pub const LIMIT: usize = 4;\n\
                  \n\
                  pub const Codec = struct { level: u8 };\n\
                  \n\
                  /// Parse the level.\n\
                  pub fn parseLevel(spec: []const u8) u8 {\n\
                  \x20   for (spec) |ch| {\n\
                  \x20       if (ch == ' ') return 1;\n\
                  \x20   }\n\
                  \x20   return 0;\n\
                  }\n\
                  \n\
                  pub fn drive(spec: []const u8) u8 {\n\
                  \x20   return parseLevel(spec);\n\
                  }\n",
            func: "parseLevel",
            ret: Some("u8"),
            doc: "Parse the level.",
            caller: "drive",
            konst: "LIMIT",
            ty: "Codec",
            module: None,
            import: "std",
            complexity: 3,
        },
        LangFixture {
            lang: Language::Odin,
            src: "package codec\n\
                  \n\
                  import \"core:fmt\"\n\
                  \n\
                  LIMIT :: 4\n\
                  \n\
                  Codec :: struct { level: u8 }\n\
                  \n\
                  // Parse the level.\n\
                  parse_level :: proc(spec: string) -> u8 {\n\
                  \x20   for i := 0; i < LIMIT; i += 1 {\n\
                  \x20       if spec[i] == ' ' {\n\
                  \x20           return 1\n\
                  \x20       }\n\
                  \x20   }\n\
                  \x20   return 0\n\
                  }\n\
                  \n\
                  drive :: proc(spec: string) -> u8 {\n\
                  \x20   return parse_level(spec)\n\
                  }\n",
            func: "parse_level",
            ret: Some("u8"),
            doc: "Parse the level.",
            caller: "drive",
            konst: "LIMIT",
            ty: "Codec",
            module: Some("codec"),
            import: "core:fmt",
            complexity: 3,
        },
    ];

    #[test]
    fn new_languages_extract_the_same_shapes() {
        for f in LANG_FIXTURES {
            let name = f.lang.as_str();
            let ex = extract(f.lang, f.src).unwrap_or_else(|| panic!("{name}: parse failed"));

            let func = ex
                .funcs
                .iter()
                .find(|x| x.name == f.func)
                .unwrap_or_else(|| panic!("{name}: no `{}` in {:?}", f.func,
                    ex.funcs.iter().map(|x| &x.name).collect::<Vec<_>>()));
            assert_eq!(func.ret.as_deref(), f.ret, "{name}: return type");
            assert_eq!(func.comment.as_deref(), Some(f.doc), "{name}: doc comment");
            assert_eq!(func.metrics.complexity(), f.complexity, "{name}: complexity");
            assert_eq!(func.metrics.max_loop_depth(), 1, "{name}: loop depth");
            assert_eq!(func.metrics.params, 1, "{name}: parameter count");

            assert!(
                ex.funcs.iter().any(|x| x.name == f.caller),
                "{name}: no caller `{}`", f.caller
            );
            assert!(
                ex.refs.iter().any(|r| r.caller == f.caller && r.target_name == f.func),
                "{name}: `{}` -> `{}` unresolved", f.caller, f.func
            );
            assert!(
                ex.consts.iter().any(|c| c.name == f.konst),
                "{name}: no const `{}` in {:?}", f.konst,
                ex.consts.iter().map(|c| &c.name).collect::<Vec<_>>()
            );
            assert!(
                ex.types.iter().any(|t| t.name == f.ty),
                "{name}: no type `{}` in {:?}", f.ty,
                ex.types.iter().map(|t| &t.name).collect::<Vec<_>>()
            );
            assert!(
                ex.imports.iter().any(|i| i.module == f.import),
                "{name}: no import `{}` in {:?}", f.import,
                ex.imports.iter().map(|i| &i.module).collect::<Vec<_>>()
            );
            match f.module {
                Some(m) => assert!(ex.modules.iter().any(|x| x == m), "{name}: no module `{m}`"),
                None => assert!(ex.modules.is_empty(), "{name}: unexpected modules"),
            }
        }
    }

    // A binding that is really a type or an import must not also be filed as a
    // constant. Zig spells all three with `variable_declaration`, so this is
    // the one language where the three can collide.
    #[test]
    fn zig_type_and_import_bindings_are_not_constants() {
        let src = "const std = @import(\"std\");\n\
                   pub const Codec = struct { level: u8 };\n\
                   pub const LIMIT: usize = 4;\n\
                   var counter: usize = 0;\n";
        let ex = extract(Language::Zig, src).unwrap();
        let names: Vec<&str> = ex.consts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["LIMIT"], "only the value binding is a constant");
        assert!(ex.types.iter().any(|t| t.name == "Codec"));
        assert!(ex.imports.iter().any(|i| i.module == "std"));
    }

    // `defer` and `using` both promise the release happens when exiting scope but
    // function slightly differently
    #[test]
    fn defer_guards_the_release_and_using_guards_the_acquire() {
        let deferred: &[(Language, &str)] = &[
            (
                Language::Zig,
                "pub fn run() void {\n\
                 \x20   const f = openFile();\n\
                 \x20   defer f.close();\n\
                 }\n",
            ),
            (
                Language::Odin,
                "package p\n\
                 run :: proc() {\n\
                 \x20   h := open(\"x\")\n\
                 \x20   defer close(h)\n\
                 }\n",
            ),
        ];
        for (lang, src) in deferred {
            let ex = extract(*lang, src).unwrap();
            let f = &ex.funcs[0];
            assert!(
                f.metrics.resources.iter().any(|r| r.acquire),
                "{}: no acquire recorded", lang.as_str()
            );
            let release = f
                .metrics
                .resources
                .iter()
                .find(|r| !r.acquire)
                .unwrap_or_else(|| panic!("{}: no release recorded", lang.as_str()));
            assert!(release.guarded, "{}: release not seen as deferred", lang.as_str());
        }

        // C# `using` wraps the acquisition itself
        let cs = "class C { void Run(string p) { using (var f = File.OpenRead(p)) { } } }\n";
        let ex = extract(Language::CSharp, cs).unwrap();
        let f = ex.funcs.iter().find(|x| x.name == "Run").unwrap();
        let acquire = f.metrics.resources.iter().find(|r| r.acquire).expect("no acquire");
        assert!(acquire.guarded, "csharp: `using` did not guard the acquire");
    }


    #[test]
    fn a_typedef_names_a_type_exactly_once() {
        // an anonymous typedef has nowhere but the typedef to carry its name
        let ex = extract(Language::Cpp, "typedef struct { int x; } Anon;\n").unwrap();
        assert!(ex.types.iter().any(|t| t.name == "Anon"), "{:?}", ex.types);
        // a named one is already recorded by the struct, and must not be
        // recorded a second time by the typedef around it
        let ex = extract(Language::C, "typedef struct Codec { int x; } Codec;\n").unwrap();
        assert_eq!(ex.types.iter().filter(|t| t.name == "Codec").count(), 1, "{:?}", ex.types);
    }

    #[test]
    fn enum_members_are_indexed_without_swallowing_struct_fields() {
        let zig = "pub const Mode = enum {\n\
                   \x20   fast,\n\
                   \x20   small,\n\
                   };\n\
                   pub const Codec = struct {\n\
                   \x20   level: u8,\n\
                   \x20   name: u8,\n\
                   };\n";
        let ex = extract(Language::Zig, zig).unwrap();
        let consts: Vec<(&str, Option<&str>)> =
            ex.consts.iter().map(|c| (c.name.as_str(), c.ty.as_deref())).collect();
        // zig spells a variant and a struct field the same way
        assert_eq!(consts, vec![("fast", Some("Mode")), ("small", Some("Mode"))]);

        let odin = "package p\n\
                    \n\
                    Mode :: enum {\n\
                    \x20   Fast,\n\
                    \x20   Small,\n\
                    }\n\
                    \n\
                    Handle :: distinct int\n";
        let ex = extract(Language::Odin, odin).unwrap();
        let consts: Vec<(&str, Option<&str>)> =
            ex.consts.iter().map(|c| (c.name.as_str(), c.ty.as_deref())).collect();
        assert_eq!(consts, vec![("Fast", Some("Mode")), ("Small", Some("Mode"))]);
        // `distinct` declares a type - it shares a node kind with every other
        // `::` binding, so it is easy to file as a value by mistake
        assert!(
            ex.types.iter().any(|t| t.name == "Handle" && t.kind == "alias"),
            "{:?}", ex.types
        );
    }

    #[test]
    fn csharp_methods_are_owned_and_an_extension_binds_the_type_it_extends() {
        let src = "public class Client { public int Id; }\n\
                   public static class Ext {\n\
                   \x20   public static int Charge(this Client c, int amt) { return amt; }\n\
                   }\n\
                   public class Box<T> {\n\
                   \x20   public T Get<U>(U key) { return default(T); }\n\
                   }\n";
        let ex = extract(Language::CSharp, src).unwrap();
        let charge = ex.funcs.iter().find(|f| f.name == "Charge").unwrap();
        // declared inside `Ext`, but it extends `Client` - and `Client` is the
        // type a call through a receiver will name
        assert_eq!(charge.owner.as_deref(), Some("Client"));
        // a generic method is an ordinary method with an ordinary return type
        let get = ex.funcs.iter().find(|f| f.name == "Get").unwrap();
        assert_eq!(get.owner.as_deref(), Some("Box"));
        assert_eq!(get.ret.as_deref(), Some("T"));
    }

    #[test]
    fn an_unqualified_call_means_a_sibling_method_only_where_the_language_says_so() {
        // C++, C# and Zig look in the enclosing type before the file
        let cases: &[(Language, &str)] = &[
            (
                Language::Cpp,
                "struct C {\n\
                 \x20   int settle(int a) { return a; }\n\
                 \x20   int charge(int a) { return settle(a); }\n\
                 };\n",
            ),
            (
                Language::CSharp,
                "class C {\n\
                 \x20   int Settle(int a) { return a; }\n\
                 \x20   int Charge(int a) { return Settle(a); }\n\
                 }\n",
            ),
        ];
        for (lang, src) in cases {
            let ex = extract(*lang, src).unwrap();
            assert!(
                ex.refs.iter().any(|r| r.target_name.eq_ignore_ascii_case("settle")),
                "{}: a sibling method call resolved to nothing", lang.as_str()
            );
        }
        // Rust needs `self.` or `Self::`, so a bare name is the free function
        // even when an identically named method is in scope
        let rust = "fn settle(a: u8) -> u8 { a }\n\
                    struct C;\n\
                    impl C {\n\
                    \x20   fn settle(&self, a: u8) -> u8 { a + 1 }\n\
                    \x20   fn charge(&self, a: u8) -> u8 { settle(a) }\n\
                    }\n";
        let ex = extract(Language::Rust, rust).unwrap();
        let r = ex.refs.iter().find(|r| r.caller == "charge").expect("no ref from `charge`");
        assert_eq!(r.target_line, 1, "rust must reach the free `settle`, not the method");
    }

    // A doc comment is not always the definition's immediate predecessor.
    // Attributes sit between the two, and `export` / decorators / a `const`
    // binding wrap the definition so the comment belongs to the wrapper
    #[test]
    fn a_doc_comment_survives_attributes_and_wrappers() {
        struct Case {
            lang: Language,
            src: &'static str,
            func: &'static str,
            doc: &'static str,
        }
        const CASES: &[Case] = &[
            Case {
                lang: Language::Rust,
                src: "/// Exported to C.\n#[no_mangle]\npub extern \"C\" fn init() -> u32 { 1 }\n",
                func: "init",
                doc: "Exported to C.",
            },
            Case {
                lang: Language::Rust,
                src: "/// Two attributes deep.\n#[inline]\n#[must_use]\npub fn go() -> u32 { 1 }\n",
                func: "go",
                doc: "Two attributes deep.",
            },
            Case {
                lang: Language::Python,
                src: "# Decorated.\n@staticmethod\ndef run():\n    return 1\n",
                func: "run",
                doc: "Decorated.",
            },
            Case {
                lang: Language::TypeScript,
                src: "// Exported.\nexport function run(): number { return 1; }\n",
                func: "run",
                doc: "Exported.",
            },
            Case {
                lang: Language::TypeScript,
                src: "/** Bound to a name. */\nexport const run = () => 1;\n",
                func: "run",
                doc: "Bound to a name.",
            },
            Case {
                lang: Language::JavaScript,
                src: "// Plain arrow const.\nconst run = () => 1;\n",
                func: "run",
                doc: "Plain arrow const.",
            },
            Case {
                lang: Language::CSharp,
                src: "class C {\n  // Attributed.\n  [Obsolete]\n  public int Run() { return 1; }\n}\n",
                func: "Run",
                doc: "Attributed.",
            },
        ];
        for c in CASES {
            let ex = extract(c.lang, c.src).unwrap();
            let f = ex
                .funcs
                .iter()
                .find(|f| f.name == c.func)
                .unwrap_or_else(|| panic!("{}: no `{}`", c.lang.as_str(), c.func));
            assert_eq!(f.comment.as_deref(), Some(c.doc), "{}", c.lang.as_str());
        }
    }

    // tthe relaxation must not reach backwards past unrelated code: a comment
    // that documents something else is not this definition's summary
    #[test]
    fn a_distant_or_unrelated_comment_is_not_adopted() {
        let src = "// Documents the constant.\nconst LIMIT: u32 = 4;\n\npub fn undocumented() -> u32 { LIMIT }\n";
        let ex = extract(Language::Rust, src).unwrap();
        let f = ex.funcs.iter().find(|f| f.name == "undocumented").unwrap();
        assert_eq!(f.comment, None, "a comment on the previous item is not a doc");
    }

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
    fn a_crate_root_is_module_declarations_and_re_exports_not_an_empty_file() {
        let src = "//! crate docs\n\
                   pub mod scan;\n\
                   pub mod serve;\n\
                   mod internal;\n\
                   pub use scan::{scan, ScanReport};\n\
                   pub(crate) use serve::helper;\n\
                   pub use internal::*;\n\
                   use std::path::PathBuf;\n";
        let ex = extract(Language::Rust, src).unwrap();
        assert_eq!(ex.modules, vec!["scan", "serve", "internal"]);

        let flags: Vec<(&str, bool)> = ex
            .imports
            .iter()
            .map(|i| (i.module.as_str(), i.reexport))
            .collect();
        assert_eq!(
            flags,
            vec![
                ("scan", true),
                ("serve", true),
                ("internal", true),
                ("std::path", false),
            ],
            "`pub use` republishes, a plain `use` only consumes"
        );

        // a glob binds no names but is still a re-export of internal module
        let glob = ex.imports.iter().find(|i| i.module == "internal").unwrap();
        assert!(glob.names.is_empty() && glob.reexport);
    }

    #[test]
    fn an_inline_test_module_is_not_declared_structure() {
        // `mod tests` is a module the grammar sees and the project is not built
        // from - counting it would put a 1 on nearly every Rust file
        let src = "pub mod real;\n\
                   pub fn charge(c: u64) -> u64 { c }\n\
                   #[cfg(test)]\n\
                   mod tests {\n\
                       mod nested_helpers {}\n\
                       #[test]\n\
                       fn t() { assert_eq!(charge(1), 1); }\n\
                   }\n";
        let ex = extract(Language::Rust, src).unwrap();
        assert_eq!(ex.modules, vec!["real"]);
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

    // jest/vitest tests are anonymous callbacks; their string label becomes the
    // caller so `changes` can report which test exercises a function
    #[test]
    fn bdd_callbacks_name_and_flag_their_scope() {
        let src = "import { charge } from \"./pay\";\n\
                   describe(\"pay\", () => {\n\
                     it(\"charges a fee\", () => { expect(charge(1)).toBe(31); });\n\
                   });\n\
                   const total = charge(2);\n\
                   run(() => { charge(3); });\n";
        let ex = extract(Language::JavaScript, src).unwrap();
        let site = |line: usize| {
            ex.calls
                .iter()
                .find(|c| c.name == "charge" && c.line == line)
                .unwrap_or_else(|| panic!("no charge call on line {line}"))
        };
        let inner = site(3);
        assert_eq!(inner.caller, "it(\"charges a fee\")");
        assert!(inner.test_ctx);
        // outside any suite, nothing changes
        assert_eq!(site(5).caller, TOP_LEVEL);
        assert!(!site(5).test_ctx);
        // a non-test callback stays anonymous
        assert_eq!(site(6).caller, TOP_LEVEL);
        assert!(!site(6).test_ctx);
    }

    #[test]
    fn func_metrics_count_shape_and_literal_trips() {
        let rust = "fn work(a: usize, b: usize) -> usize {\n\
                    \x20   let mut t = 0;\n\
                    \x20   for i in 0..4 { for j in 0..=b { t += i * j; } }\n\
                    \x20   if t > 3 { t -= 1; }\n\
                    \x20   t\n\
                    }\n";
        let f = &extract(Language::Rust, rust).unwrap().funcs[0];
        assert_eq!(f.metrics.params, 2);
        assert_eq!(f.metrics.branches, 1);
        assert_eq!(f.metrics.loops.len(), 2);
        assert_eq!(f.metrics.max_loop_depth(), 2);
        // `0..4` is countable; `0..=b` depends on a value
        assert_eq!(f.metrics.loops[0].trip, Some(4));
        assert_eq!(f.metrics.loops[1].trip, None);
        assert_eq!(f.metrics.complexity(), 4);
        assert!(!f.metrics.recursive);

        // C-style headers and python ranges resolve the same way
        let go = "package m\nfunc loop() { for i := 0; i <= 7; i++ { use(i) } }\n";
        let g = &extract(Language::Go, go).unwrap().funcs[0];
        assert_eq!(g.metrics.loops[0].trip, Some(8));
        let py = "def loop():\n    for i in range(2, 6):\n        use(i)\n";
        let p = &extract(Language::Python, py).unwrap().funcs[0];
        assert_eq!(p.metrics.loops[0].trip, Some(4));

        // recursion is detected by self-name
        let rec = "fn fact(n: u64) -> u64 { if n < 2 { 1 } else { n * fact(n - 1) } }\n";
        assert!(extract(Language::Rust, rec).unwrap().funcs[0].metrics.recursive);
    }

    #[test]
    fn resource_ops_pair_by_language() {
        let c = "void f() { char* p = (char*)malloc(10); use(p); }\n\
                 void g() { char* p = (char*)malloc(10); free(p); }\n";
        let ex = extract(Language::Cpp, c).unwrap();
        let leaky = ex.funcs.iter().find(|f| f.name == "f").unwrap();
        assert_eq!(leaky.metrics.resources.len(), 1);
        assert!(leaky.metrics.resources[0].acquire);
        let paired = ex.funcs.iter().find(|f| f.name == "g").unwrap();
        assert_eq!(paired.metrics.resources.len(), 2);
        assert!(!paired.metrics.resources[1].acquire);

        // python `with` marks the acquire as automatically released
        let py = "def read(p):\n    with open(p) as fh:\n        return fh.read()\n";
        let f = &extract(Language::Python, py).unwrap().funcs[0];
        assert!(f.metrics.resources[0].guarded);
    }

    // the type layer `changes` resolves calls through: definitions, method
    // owners, parameter types, module identities, and receiver types
    #[test]
    fn typed_languages_expose_types_owners_and_receivers() {
        let rust = "pub struct Client { id: u64 }\n\
                    pub trait Pay { fn pay(&self); }\n\
                    impl Client {\n\
                    \x20   pub fn charge(&self, cents: u64) -> u64 { cents }\n\
                    }\n\
                    fn run(c: &Client) -> u64 {\n\
                    \x20   let other = Client::new();\n\
                    \x20   other.charge(1) + c.charge(2)\n\
                    }\n";
        let ex = extract(Language::Rust, rust).unwrap();
        let kinds: Vec<(&str, &str)> = ex
            .types
            .iter()
            .map(|t| (t.name.as_str(), t.kind.as_str()))
            .collect();
        assert_eq!(kinds, vec![("Client", "struct"), ("Pay", "trait")]);
        let charge = ex.funcs.iter().find(|f| f.name == "charge").unwrap();
        assert_eq!(charge.owner.as_deref(), Some("Client"));
        assert_eq!(charge.param_types, vec!["Client", "u64"]);
        // both receivers resolve: one from a parameter, one from a constructor
        let calls: Vec<(&str, Option<&str>)> = ex
            .calls
            .iter()
            .filter(|c| c.name == "charge")
            .map(|c| (c.name.as_str(), c.recv_type.as_deref()))
            .collect();
        assert_eq!(calls, vec![("charge", Some("Client")), ("charge", Some("Client"))]);

        let go = "package billing\n\
                  type Ledger struct { n int }\n\
                  func (l *Ledger) Charge(c int) int { return c }\n\
                  func run() int {\n\
                  \tvar led Ledger\n\
                  \treturn led.Charge(1)\n\
                  }\n";
        let ex = extract(Language::Go, go).unwrap();
        assert_eq!(ex.modules, vec!["billing"]);
        assert!(ex.types.iter().any(|t| t.name == "Ledger"));
        let m = ex.funcs.iter().find(|f| f.name == "Charge").unwrap();
        assert_eq!(m.owner.as_deref(), Some("Ledger"));
        let call = ex.calls.iter().find(|c| c.name == "Charge").unwrap();
        assert_eq!(call.recv_type.as_deref(), Some("Ledger"));

        let ts = "export class Gateway { send(n: number): void {} }\n\
                  export interface Wire { id: string }\n\
                  function go(): void {\n\
                  \x20 const g = new Gateway();\n\
                  \x20 g.send(1);\n\
                  }\n";
        let ex = extract(Language::TypeScript, ts).unwrap();
        assert!(ex.types.iter().any(|t| t.name == "Gateway" && t.kind == "class"));
        assert!(ex.types.iter().any(|t| t.name == "Wire" && t.kind == "interface"));
        let call = ex.calls.iter().find(|c| c.name == "send").unwrap();
        assert_eq!(call.recv_type.as_deref(), Some("Gateway"));

        let cpp = "namespace billing {\n\
                   class Account { public: double debit(double a); };\n\
                   double run() { Account acct; return acct.debit(1.0); }\n\
                   }\n";
        let ex = extract(Language::Cpp, cpp).unwrap();
        assert_eq!(ex.modules, vec!["billing"]);
        assert!(ex.types.iter().any(|t| t.name == "Account"));
        let call = ex.calls.iter().find(|c| c.name == "debit").unwrap();
        assert_eq!(call.recv_type.as_deref(), Some("Account"));
    }

    #[test]
    fn type_names_normalise_to_their_definition() {
        for (raw, want) in [
            ("&mut Option<billing::Client>", "Client"),
            ("Vec<Invoice>", "Invoice"),
            ("std::shared_ptr<Account>", "Account"),
            ("Promise<Gateway>", "Gateway"),
            ("[]Ledger", "Ledger"),
            ("Ledger[]", "Ledger"),
            (": Wire", "Wire"),
            ("*const Client", "Client"),
            ("u64", "u64"),
        ] {
            assert_eq!(normalize_type(raw), want, "normalising {raw}");
        }
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
    fn note_marker_requires_a_colon() {
        // a marker word loose in prose is not an annotation, however it is
        // cased; an owner group between the marker and its colon still is.
        let src = "// a real TODO must be punctuated\n\
                   fn a() {}\n\
                   // the coverage note into the markdown\n\
                   fn b() {}\n\
                   // fixes bug where cache_name wasnt unique\n\
                   fn c() {}\n\
                   // TODO(alice): wire this up\n\
                   fn d() {}\n\
                   // FIXME: broken\n\
                   fn e() {}\n";
        let ex = extract(Language::Rust, src).unwrap();
        let texts: Vec<&str> = ex.notes.iter().map(|n| n.text.as_str()).collect();
        assert_eq!(texts.len(), 2, "got {texts:?}");
        assert!(texts[0].contains("TODO(alice)"));
        assert!(texts[1].contains("FIXME"));
    }

    #[test]
    fn note_marker_must_follow_whitespace() {
        // a marker quoted or bracketed is prose about markers, not an
        // annotation; one opening its comment or following a space is real.
        let src = "// a free-form marker (TODO/FIXME/NOTE/...)\n\
                   fn a() {}\n\
                   // \"NOTE\" must not trigger\n\
                   fn b() {}\n\
                   /* SAFETY: the pointer is non-null */\n\
                   fn c() {}\n\
                   // trailing HACK: still counts\n\
                   fn d() {}\n";
        let ex = extract(Language::Rust, src).unwrap();
        let texts: Vec<&str> = ex.notes.iter().map(|n| n.text.as_str()).collect();
        assert_eq!(texts.len(), 2, "got {texts:?}");
        assert!(texts[0].contains("SAFETY"));
        assert!(texts[1].contains("HACK"));
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
    fn ts_module_identity_comes_from_namespace_blocks_only() {
        // the two TS forms that actually name a scope
        let ns = extract(
            Language::TypeScript,
            "namespace Billing { export function a(): number { return 1; } }\n",
        )
        .unwrap();
        assert_eq!(ns.modules, vec!["Billing"]);
        let legacy = extract(
            Language::TypeScript,
            "module Legacy { export function b(): number { return 2; } }\n",
        )
        .unwrap();
        assert_eq!(legacy.modules, vec!["Legacy"]);
        // `declare module "ext"` describes someone else's package
        let ambient = extract(
            Language::TypeScript,
            "declare module \"ext\" { export function e(): void; }\n",
        )
        .unwrap();
        assert!(ambient.modules.is_empty(), "{:?}", ambient.modules);
        // and the two forms that carry no name at all: in ES-module and CommonJS
        // code every file is already a module, so neither groups anything
        for src in [
            "function c() { return 3; }\nmodule.exports = { c };\n",
            "export function d(): number { return 4; }\n",
        ] {
            let ex = extract(Language::TypeScript, src).unwrap();
            assert!(ex.modules.is_empty(), "{src} -> {:?}", ex.modules);
        }
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

    // `ccc:skip` at the very top withdraws the file, in every comment syntax.
    #[test]
    fn skip_at_the_top_withdraws_the_file_in_every_language() {
        for (lang, src) in [
            (Language::Rust, "// ccc:skip\npub fn a() {}\npub fn b() {}\n"),
            (Language::Go, "// ccc:skip\npackage p\n\nfunc A() {}\n"),
            (Language::Python, "# ccc:skip\ndef a():\n    return 1\n"),
            (Language::TypeScript, "// ccc:skip generated\nexport function a() {}\n"),
            (Language::JavaScript, "/* ccc:skip */\nfunction a() {}\n"),
        ] {
            assert!(extract(lang, src).is_none(), "{lang:?}");
        }
    }

    // Directly above a definition it withdraws only that definition: the
    // function, its calls, and its slot as a call target.
    #[test]
    fn skip_above_a_definition_withdraws_only_it() {
        let src = "pub fn kept() {\n    helper();\n}\n\n\
                   // ccc:skip\npub fn dropped() {\n    helper();\n}\n\n\
                   pub fn helper() {}\n";
        let ex = extract(Language::Rust, src).expect("parse");
        let names: Vec<&str> = ex.funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["kept", "helper"]);
        assert!(ex.refs.iter().any(|r| r.caller == "kept"));
        assert!(!ex.refs.iter().any(|r| r.caller == "dropped"));
        assert!(ex.calls.iter().all(|c| c.caller != "dropped"));
    }

    // A skipped function is not a call target either: calls to it stay loose
    // instead of resolving to a definition the analysis no longer has.
    #[test]
    fn calls_to_a_skipped_function_do_not_resolve() {
        let src = "pub fn caller() {\n    hidden();\n}\n\n\
                   // ccc:skip\npub fn hidden() {}\n";
        let ex = extract(Language::Rust, src).expect("parse");
        assert!(!ex.refs.iter().any(|r| r.target_name == "hidden"));
        // the loose form survives - `changes` may still match it elsewhere
        assert!(ex.calls.iter().any(|c| c.name == "hidden"));
    }

    // Attribute and decorator lines may sit between the marker and the
    // definition; a blank line detaches it and makes it file-wide.
    #[test]
    fn skip_reaches_through_attributes_but_not_blank_lines() {
        let through = "pub fn kept() {}\n\n\
                       // ccc:skip\n#[inline]\npub fn dropped() {}\n";
        let ex = extract(Language::Rust, through).expect("parse");
        let names: Vec<&str> = ex.funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["kept"]);

        let detached = "pub fn kept() {}\n\n// ccc:skip\n\npub fn other() {}\n";
        assert!(extract(Language::Rust, detached).is_none());
    }

    // Inside a body the marker withdraws the function it sits in.
    #[test]
    fn skip_inside_a_body_withdraws_that_function() {
        let src = "pub fn kept() {}\n\n\
                   pub fn dropped() {\n    // ccc:skip\n    let _ = 1;\n}\n";
        let ex = extract(Language::Rust, src).expect("parse");
        let names: Vec<&str> = ex.funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["kept"]);
    }

    // Not a directive: prose around the marker, or a different word after the
    // prefix.
    #[test]
    fn skip_lookalikes_are_not_directives() {
        for src in [
            "// see http://x/ccc:skip\npub fn a() {}\n",
            "// ccc:skipped\npub fn a() {}\n",
        ] {
            let ex = extract(Language::Rust, src).expect("parse");
            assert!(ex.funcs.iter().any(|f| f.name == "a"), "{src}");
        }
    }

    // Python decorators sit between the marker and the `def` and must not
    // detach it.
    #[test]
    fn skip_reaches_through_python_decorators() {
        let src = "def kept():\n    return 1\n\n\
                   # ccc:skip\n@staticmethod\ndef dropped():\n    return 2\n";
        let ex = extract(Language::Python, src).expect("parse");
        let names: Vec<&str> = ex.funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["kept"]);
    }
}
