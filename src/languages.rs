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
}

impl Language {
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
            Language::Cpp => &["function_definition"],
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
            Language::Cpp => &["declaration"],
        }
    }

    // kinds that represent a call expression
    pub fn call_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["call_expression"],
            Language::Python => &["call"],
            Language::JavaScript | Language::TypeScript | Language::Tsx => &["call_expression"],
            Language::Go => &["call_expression"],
            Language::Cpp => &["call_expression"],
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
        }
    }

    // kinds that declare one enum variant/enumerator - indexed as const-like
    // definitions so `references` can pair them with their usages
    pub fn variant_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["enum_variant"],
            Language::Cpp => &["enumerator"],
            Language::Python
            | Language::JavaScript
            | Language::TypeScript // ts enums are grammar-irregular - not covered yet
            | Language::Tsx 
            | Language::Go => &[],
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
        }
    }

    // node kinds that represent a comment
    pub fn comment_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["line_comment", "block_comment"],
            Language::Python | Language::JavaScript | Language::TypeScript | Language::Tsx
            | Language::Go => &["comment"],
            Language::Cpp => &["comment"],
        }
    }

    // Does this language declare types where the syntax tree can read them?
    pub fn is_typed(self) -> bool {
        matches!(
            self,
            Language::Rust | Language::Go | Language::Cpp | Language::TypeScript | Language::Tsx
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
            ],
            Language::TypeScript | Language::Tsx => &[
                ("class_declaration", "class"),
                ("abstract_class_declaration", "class"),
                ("interface_declaration", "interface"),
                ("type_alias_declaration", "alias"),
                ("enum_declaration", "enum"),
            ],
            // classes exist but carry no declared field/param types
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
        }
    }

    // node kinds holding a function's parameter list
    pub fn param_list_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Rust | Language::Python => &["parameters"],
            Language::JavaScript | Language::TypeScript | Language::Tsx => &["formal_parameters"],
            Language::Go | Language::Cpp => &["parameter_list"],
        }
    }

    // kinds that make a resource release automatic
    pub fn guard_kinds(self) -> &'static [&'static str] {
        match self {
            Language::Python => &["with_statement"],
            Language::Go => &["defer_statement"],
            // rust/c++ release through RAII, which has no distinct node kind.
            // I cant be bothered to try and analyse this right now
            _ => &[],
        }
    }

    // (acquire, release) call-name pairs
    pub fn resource_pairs(self) -> &'static [(&'static str, &'static str)] {
        match self {
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
            Language::Go => "keep under the inliner's cost budget",
            Language::Python => "no inliner - consider hoisting the call out of hot loops",
            Language::JavaScript | Language::TypeScript | Language::Tsx => {
                "monomorphic call site (JIT inlines these)"
            }
        }
    }

    // field name holding a function's return type (if the grammar has one)
    pub fn return_field(self) -> Option<&'static str> {
        match self {
            Language::Rust | Language::Python => Some("return_type"),
            Language::TypeScript | Language::Tsx => Some("return_type"),
            Language::Go => Some("result"),
            // C++ carries the return type in the `type` field of a
            // `function_definition` (pointer/ref markers live on the declarator).
            Language::Cpp => Some("type"),
            Language::JavaScript => None,
        }
    }
}
