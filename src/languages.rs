//! language detection and per-language node-kind configuration used by tree-sitter

use std::path::Path;
use tree_sitter::Language as TsLanguage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Cpp,
    C,
    CSharp,
    Zig,
    Odin,
}

impl Language {
    pub const ALL: &'static [Language] = &[
        Language::Rust,
        Language::Python,
        Language::JavaScript,
        Language::TypeScript,
        Language::Tsx,
        Language::Go,
        Language::Cpp,
        Language::C,
        Language::CSharp,
        Language::Zig,
        Language::Odin,
    ];

    pub fn from_path(path: &Path) -> Option<Language> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "rs" => Language::Rust,
            "py" | "pyi" => Language::Python,
            "js" | "jsx" | "mjs" | "cjs" => Language::JavaScript,
            "ts" | "mts" | "cts" => Language::TypeScript,
            "tsx" => Language::Tsx,
            "go" => Language::Go,
            "cpp" | "cc" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h++" | "h" => Language::Cpp,
            // `.h` deliberately stays with C++ above instead of C
            "c" => Language::C,
            "cs" | "csx" => Language::CSharp,
            "zig" => Language::Zig,
            "odin" => Language::Odin,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::JavaScript => "javascript",
            Language::TypeScript => "typescript",
            Language::Tsx => "tsx",
            Language::Go => "go",
            Language::Cpp => "cpp",
            Language::C => "c",
            Language::CSharp => "csharp",
            Language::Zig => "zig",
            Language::Odin => "odin",
        }
    }

    // tree-sitter grammar for this language
    pub fn ts_language(self) -> TsLanguage {
        match self {
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::Python => tree_sitter_python::LANGUAGE.into(),
            Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Language::Go => tree_sitter_go::LANGUAGE.into(),
            Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Language::C => tree_sitter_c::LANGUAGE.into(),
            Language::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Language::Zig => tree_sitter_zig::LANGUAGE.into(),
            Language::Odin => tree_sitter_odin::LANGUAGE.into(),
        }
    }

    // kinds that represent a function/method definition
    pub fn func_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["function_item", "function_signature_item"],
            Language::Python => &["function_definition"],
            Language::JavaScript | Language::TypeScript | Language::Tsx => &[
                "function_declaration",
                "generator_function_declaration",
                "method_definition",
                "function_expression",
                "arrow_function",
            ],
            Language::Go => &["function_declaration", "method_declaration"],
            Language::Cpp | Language::C => &["function_definition"],
            Language::CSharp => &[
                "method_declaration",
                "constructor_declaration",
                "destructor_declaration",
                "operator_declaration",
                "local_function_statement",
            ],
            Language::Zig => &["function_declaration"],
            Language::Odin => &["procedure_declaration"],
        }
    }

    // kinds that represent a top-level constant/variable declaration
    pub fn const_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["const_item", "static_item"],
            Language::Python => &["assignment"],
            Language::JavaScript | Language::TypeScript | Language::Tsx => {
                &["lexical_declaration", "variable_declaration"]
            }
            Language::Go => &["const_spec", "var_spec"],
            Language::Cpp | Language::C => &["declaration"],
            // a C# constant has nowhere to live but inside a type
            Language::CSharp => &["field_declaration"],
            // zig binds everything - `const x = 1`, `const T = struct {}` - with
            // the same node; the type case is split back out in `extract`
            Language::Zig => &["variable_declaration"],
            Language::Odin => &["const_declaration"],
        }
    }

    // kinds that represent a call expression
    pub fn call_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["call_expression"],
            Language::Python => &["call"],
            Language::JavaScript | Language::TypeScript | Language::Tsx => &["call_expression"],
            Language::Go => &["call_expression"],
            Language::Cpp | Language::C | Language::Zig | Language::Odin => &["call_expression"],
            Language::CSharp => &["invocation_expression", "object_creation_expression"],
        }
    }

    // kinds that represent a qualified name usable as a value or type
    // (`Encoding::O200kBase`, `http.StatusOK`) - candidates for use capture
    pub fn use_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["scoped_identifier", "scoped_type_identifier"],
            Language::Python => &["attribute"],
            Language::JavaScript | Language::TypeScript | Language::Tsx => &["member_expression"],
            Language::Go => &["selector_expression"],
            Language::Cpp => &["qualified_identifier"],
            // C has no namespaces, so no qualified value names to capture
            Language::C => &[],
            Language::CSharp => &["member_access_expression"],
            Language::Zig => &["field_expression"],
            Language::Odin => &["member_expression"],
        }
    }

    // kinds that declare one enum variant/enumerator - indexed as const-like
    // definitions so `references` can pair them with their usages
    pub fn variant_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["enum_variant"],
            Language::Cpp | Language::C => &["enumerator"],
            Language::CSharp => &["enum_member_declaration"],
            Language::Python
            | Language::JavaScript
            | Language::TypeScript // ts enums are grammar-irregular - not covered yet
            | Language::Tsx 
            | Language::Go
            // zig shares `container_field` between enum variants and struct
            // fields, and odin's enum members are bare identifiers with no kind
            // of their own; keying on kind alone would file every struct field
            // as a constant. See LANGUAGES.md.
            | Language::Odin => &[],
            // told apart from a struct field by its parent, in `extract`
            Language::Zig => &["container_field"],
        }
    }

    // kinds whose subtree is an import/include; qualified names inside are
    // declarations of availability, not usages
    pub fn import_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["use_declaration"],
            Language::Python => &["import_statement", "import_from_statement"],
            Language::JavaScript | Language::TypeScript | Language::Tsx => &["import_statement"],
            Language::Go => &["import_declaration"],
            Language::Cpp => &["preproc_include", "using_declaration"],
            Language::C => &["preproc_include"],
            Language::CSharp => &["using_directive"],
            Language::Zig => &["variable_declaration"],
            Language::Odin => &["import_declaration"],
        }
    }

    // node kinds that represent a comment
    pub fn comment_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["line_comment", "block_comment"],
            Language::Python | Language::JavaScript | Language::TypeScript | Language::Tsx
            | Language::Go => &["comment"],
            Language::Cpp | Language::C | Language::CSharp | Language::Zig
            | Language::Odin => &["comment"],
        }
    }

    // does this language declare types where the syntax tree can read them?
    pub fn is_typed(self) -> bool {
        matches!(
            self,
            Language::Rust
                | Language::Go
                | Language::Cpp
                | Language::C
                | Language::CSharp
                | Language::Zig
                | Language::Odin
                | Language::TypeScript
                | Language::Tsx
        )
    }

    // languages whose code can call each other's functions directly
    pub fn family(self) -> &'static str {
        match self {
            Language::C | Language::Cpp => "c",
            Language::JavaScript | Language::TypeScript | Language::Tsx => "js",
            Language::Rust => "rust",
            Language::Python => "python",
            Language::Go => "go",
            Language::CSharp => "csharp",
            Language::Zig => "zig",
            Language::Odin => "odin",
        }
    }

    // does an unqualified name reach the rest of its directory
    pub fn package_scoped(self) -> bool {
        matches!(
            self,
            Language::Go | Language::CSharp | Language::C | Language::Cpp
        )
    }

    // kinds that define a named type
    pub fn type_kinds(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Language::Rust => &[
                ("struct_item", "struct"),
                ("enum_item", "enum"),
                ("trait_item", "trait"),
                ("union_item", "union"),
                ("type_item", "alias"),
            ],
            Language::Go => &[("type_spec", "struct")],
            Language::Cpp => &[
                ("class_specifier", "class"),
                ("struct_specifier", "struct"),
                ("enum_specifier", "enum"),
                ("union_specifier", "union"),
                ("alias_declaration", "alias"),
                // a `typedef` names a type just as much as `using` does
                ("type_definition", "alias"),
            ],
            Language::C => &[
                ("struct_specifier", "struct"),
                ("enum_specifier", "enum"),
                ("union_specifier", "union"),
                ("type_definition", "alias"),
            ],
            Language::CSharp => &[
                ("class_declaration", "class"),
                ("interface_declaration", "interface"),
                ("struct_declaration", "struct"),
                ("enum_declaration", "enum"),
                ("record_declaration", "record"),
                ("delegate_declaration", "alias"),
            ],
            // `extract` reads the name off the binding
            Language::Zig => &[
                ("struct_declaration", "struct"),
                ("enum_declaration", "enum"),
                ("union_declaration", "union"),
                ("opaque_declaration", "opaque"),
                ("error_set_declaration", "error"),
            ],
            Language::Odin => &[
                ("struct_declaration", "struct"),
                ("enum_declaration", "enum"),
                ("union_declaration", "union"),
                ("bit_field_declaration", "struct"),
            ],
            Language::TypeScript | Language::Tsx => &[
                ("class_declaration", "class"),
                ("abstract_class_declaration", "class"),
                ("interface_declaration", "interface"),
                ("type_alias_declaration", "alias"),
                ("enum_declaration", "enum"),
            ],
            // classes exist for some reason but carry no declared field/param types
            Language::JavaScript => &[("class_declaration", "class")],
            Language::Python => &[("class_definition", "class")],
        }
    }

    // kinds that declare a module identity a qualifier can name: a Go
    // `package`, a C++ `namespace`, a Rust inline `mod`
    pub fn module_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Go => &["package_clause"],
            Language::Cpp => &["namespace_definition"],
            Language::Rust => &["mod_item"],
            Language::CSharp => &["namespace_declaration", "file_scoped_namespace_declaration"],
            Language::Odin => &["package_declaration"],
            Language::TypeScript | Language::Tsx => &["internal_module", "module"],
            _ => &[],
        }
    }

    // kinds that repeat their body - loop-nesting depth and unroll candidates
    pub fn loop_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["for_expression", "while_expression", "loop_expression"],
            // comprehensions are loops that the reader does not always see
            Language::Python => &[
                "for_statement",
                "while_statement",
                "list_comprehension",
                "set_comprehension",
                "dictionary_comprehension",
                "generator_expression",
            ],
            Language::JavaScript | Language::TypeScript | Language::Tsx => &[
                "for_statement",
                "for_in_statement",
                "while_statement",
                "do_statement",
            ],
            Language::Go => &["for_statement"],
            Language::Cpp => &["for_statement", "for_range_loop", "while_statement", "do_statement"],
            // C has no range-for
            Language::C => &["for_statement", "while_statement", "do_statement"],
            Language::CSharp => &[
                "for_statement",
                "foreach_statement",
                "while_statement",
                "do_statement",
            ],
            Language::Zig => &["for_statement", "while_statement"],
            Language::Odin => &["for_statement"],
        }
    }

    // kinds that fork control flow
    pub fn branch_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["if_expression", "match_arm"],
            Language::Python => &["if_statement", "elif_clause", "except_clause", "case_clause"],
            Language::JavaScript | Language::TypeScript | Language::Tsx => &[
                "if_statement",
                "switch_case",
                "catch_clause",
                "ternary_expression",
            ],
            Language::Go => &[
                "if_statement",
                "expression_case",
                "type_case",
                "communication_case",
            ],
            Language::Cpp => &[
                "if_statement",
                "case_statement",
                "catch_clause",
                "conditional_expression",
            ],
            // C has no exceptions thankfully
            Language::C => &["if_statement", "case_statement", "conditional_expression"],
            Language::CSharp => &[
                "if_statement",
                "switch_section",
                "switch_expression_arm",
                "catch_clause",
                "conditional_expression",
            ],
            Language::Zig => &["if_statement", "if_expression", "switch_case"],
            Language::Odin => &["if_statement", "switch_case"],
        }
    }

    // node kinds holding a function's parameter list
    pub fn param_list_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust | Language::Python => &["parameters"],
            Language::JavaScript | Language::TypeScript | Language::Tsx => &["formal_parameters"],
            Language::Go | Language::Cpp | Language::C | Language::CSharp => {
                &["parameter_list"]
            }
            Language::Zig | Language::Odin => &["parameters"],
        }
    }

    // kinds that make a resource release automatic
    pub fn guard_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Python => &["with_statement"],
            Language::Go | Language::Odin => &["defer_statement"],
            // `errdefer` releases on the error path only, so counting it as a
            // guard is an approximation
            Language::Zig => &["defer_statement", "errdefer_statement"],
            // `using (var f = ...)` disposes on scope exit - the same promise
            // python's `with` makes
            Language::CSharp => &["using_statement"],
            // rust/c++ release through RAII, which has no distinct node kind.
            // I cant be bothered to try and analyse this right now
            _ => &[],
        }
    }

    // (acquire, release) call-name pairs
    pub fn resource_pairs(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Language::C => &[
                ("malloc", "free"),
                ("calloc", "free"),
                ("realloc", "free"),
                ("strdup", "free"),
                ("fopen", "fclose"),
                ("open", "close"),
                ("opendir", "closedir"),
                ("pthread_mutex_lock", "pthread_mutex_unlock"),
            ],
            Language::CSharp => &[
                ("OpenRead", "Dispose"),
                ("OpenWrite", "Dispose"),
                ("Open", "Dispose"),
                ("Connect", "Dispose"),
                ("Rent", "Return"),
                ("WaitOne", "Release"),
            ],
            Language::Zig => &[
                ("create", "destroy"),
                ("alloc", "free"),
                ("openFile", "close"),
                ("createFile", "close"),
                ("init", "deinit"),
                ("lock", "unlock"),
            ],
            Language::Odin => &[
                ("open", "close"),
                ("make", "delete"),
                ("new", "free"),
                ("init", "destroy"),
            ],
            Language::Cpp => &[
                ("malloc", "free"),
                ("calloc", "free"),
                ("realloc", "free"),
                ("strdup", "free"),
                ("new", "delete"),
                ("fopen", "fclose"),
                ("lock", "unlock"),
            ],
            Language::Rust => &[("leak", "from_raw"), ("forget", "from_raw"), ("into_raw", "from_raw")],
            Language::Go => &[
                ("Open", "Close"),
                ("OpenFile", "Close"),
                ("Dial", "Close"),
                ("Lock", "Unlock"),
                ("RLock", "RUnlock"),
                ("NewTicker", "Stop"),
                ("NewTimer", "Stop"),
            ],
            Language::Python => &[("open", "close"), ("connect", "close"), ("acquire", "release")],
            Language::JavaScript | Language::TypeScript | Language::Tsx => &[
                ("addEventListener", "removeEventListener"),
                ("setInterval", "clearInterval"),
                ("createObjectURL", "revokeObjectURL"),
            ],
        }
    }

    // how this language asks the compiler to inline, named in lint advice
    pub fn inline_hint(self) -> &'static str {
        match self {
            Language::Rust => "#[inline]",
            Language::Cpp => "inline / header definition",
            Language::C => "static inline in a header",
            Language::CSharp => "[MethodImpl(MethodImplOptions.AggressiveInlining)]",
            Language::Zig => "inline fn / comptime",
            Language::Odin => "#force_inline",
            Language::Go => "keep under the inliner's cost budget",
            Language::Python => "no inliner - consider hoisting the call out of hot loops",
            Language::JavaScript | Language::TypeScript | Language::Tsx => {
                "monomorphic call site (JIT inlines these)"
            }
        }
    }

    // Nodes that wrap a definition without being part of it
    pub fn doc_wrapper_kinds(self) -> &'static [&'static str] {
        match self {
            Language::JavaScript | Language::TypeScript | Language::Tsx => &[
                "export_statement",
                "lexical_declaration",
                "variable_declaration",
                "variable_declarator",
            ],
            Language::Python => &["decorated_definition"],
            _ => &[],
        }
    }

    // annotations a language allows between a doc comment and what it
    // documents
    pub fn annotation_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["attribute_item"],
            Language::Python => &["decorator"],
            Language::JavaScript | Language::TypeScript | Language::Tsx => &["decorator"],
            Language::CSharp => &["attribute_list"],
            Language::Cpp | Language::C => &["attributed_statement"],
            _ => &[],
        }
    }

    // does an unqualified call inside a type body resolve to that types own
    // methods
    pub fn implicit_member_scope(self) -> bool {
        matches!(self, Language::Cpp | Language::CSharp | Language::Zig)
    }

    // field name holding a function's return type (if the grammar has one)
    pub fn return_field(self) -> Option<&'static str> {
        match self {
            Language::Rust | Language::Python => Some("return_type"),
            Language::TypeScript | Language::Tsx => Some("return_type"),
            Language::Go => Some("result"),
            // C++ carries the return type in the `type` field of a
            // `function_definition` (pointer/ref markers live on the declarator).
            Language::Cpp | Language::C => Some("type"),
            Language::CSharp => Some("returns"),
            // zig puts the return type in `type` on the fn; odin's return sits
            // inside the `procedure` node, so `extract` reads it there
            Language::Zig => Some("type"),
            Language::Odin => None,
            Language::JavaScript => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    // every node kind named in the tables above must exist in the grammar it is
    // claimed for
    #[test]
    fn every_node_kind_exists_in_its_grammar() {
        let mut missing: Vec<String> = Vec::new();
        for &lang in Language::ALL {
            let ts = lang.ts_language();
            let known: BTreeSet<&str> = (0..ts.node_kind_count())
                .filter_map(|i| ts.node_kind_for_id(i as u16))
                .collect();
            let mut check = |label: &str, kinds: &[&str]| {
                for k in kinds {
                    if !known.contains(k) {
                        missing.push(format!("{}: {label} names `{k}`", lang.as_str()));
                    }
                }
            };
            check("func_kinds", lang.func_kinds());
            check("const_kinds", lang.const_kinds());
            check("call_kinds", lang.call_kinds());
            check("use_kinds", lang.use_kinds());
            check("variant_kinds", lang.variant_kinds());
            check("import_kinds", lang.import_kinds());
            check("comment_kinds", lang.comment_kinds());
            check("module_kinds", lang.module_kinds());
            check("loop_kinds", lang.loop_kinds());
            check("branch_kinds", lang.branch_kinds());
            check("param_list_kinds", lang.param_list_kinds());
            check("guard_kinds", lang.guard_kinds());
            check("doc_wrapper_kinds", lang.doc_wrapper_kinds());
            check("annotation_kinds", lang.annotation_kinds());
            let types: Vec<&str> = lang.type_kinds().iter().map(|(k, _)| *k).collect();
            check("type_kinds", &types);
        }
        assert!(missing.is_empty(), "node kinds no grammar has:\n{}", missing.join("\n"));
    }

    // every language has to be reachable from a path or it can be configured
    // in full and still never run
    #[test]
    fn every_language_is_reachable_by_extension() {
        let found: BTreeSet<&str> = ["a.rs", "a.py", "a.js", "a.ts", "a.tsx", "a.go", "a.cpp",
            "a.c", "a.cs", "a.zig", "a.odin"]
            .iter()
            .filter_map(|f| Language::from_path(Path::new(f)))
            .map(Language::as_str)
            .collect();
        for &lang in Language::ALL {
            assert!(found.contains(lang.as_str()), "{} has no extension", lang.as_str());
        }
        // a header stays with the permissive grammar; see LANGUAGES.md
        assert_eq!(Language::from_path(Path::new("a.h")), Some(Language::Cpp));
    }
}

