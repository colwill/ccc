# languages.rs.md (20260729-17-50-32) UTC
# source: src/languages.rs [rust]
# const
# funcs
    - L18:12@from_path:Option<Language>
    - L32:12@as_str:&'static str
    - L45:12@ts_language:TsLanguage // tree-sitter grammar for this language
    - L58:12@func_kinds:&'static [&'static str] // kinds that represent a function/method definition
    - L75:12@const_kinds:&'static [&'static str] // kinds that represent a top-level constant/variable declaration
    - L88:12@call_kinds:&'static [&'static str] // kinds that represent a call expression
    - L99:12@comment_kinds:&'static [&'static str] // node kinds that represent a comment
    - L109:12@return_field:Option<&'static str> // field name holding a function's return type (if the grammar has one)
# refs
# note
