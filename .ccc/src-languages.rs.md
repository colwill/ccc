# languages.rs.md (20260729-22-00-57) UTC
# source: src/languages.rs [rust]
# const
    - L8@Rust:Language
    - L9@Python:Language
    - L10@JavaScript:Language
    - L11@TypeScript:Language
    - L12@Tsx:Language
    - L13@Go:Language
    - L14@Cpp:Language
# funcs
    - L18:12@from_path:Option<Language>
    - L32:12@as_str:&'static str
    - L45:12@ts_language:TsLanguage // tree-sitter grammar for this language
    - L58:12@func_kinds:&'static [&'static str] // kinds that represent a function/method definition
    - L75:12@const_kinds:&'static [&'static str] // kinds that represent a top-level constant/variable declaration
    - L88:12@call_kinds:&'static [&'static str] // kinds that represent a call expression
    - L100:12@use_kinds:&'static [&'static str] // kinds that represent a qualified name usable as a value or type
    - L112:12@variant_kinds:&'static [&'static str] // kinds that declare one enum variant/enumerator - indexed as const-like
    - L126:12@import_kinds:&'static [&'static str] // kinds whose subtree is an import/include; qualified names inside are
    - L137:12@comment_kinds:&'static [&'static str] // node kinds that represent a comment
    - L147:12@return_field:Option<&'static str> // field name holding a function's return type (if the grammar has one)
# refs
# note
