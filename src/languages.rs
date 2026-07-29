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
