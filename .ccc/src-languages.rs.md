# languages.rs.md (20260701-13-08-47) UTC
# source: src/languages.rs [rust]
# const
# funcs
    - L18:12@from_path:Option<Language>
    - L31:12@as_str:&'static str
    - L43:12@ts_language:TsLanguage // tree-sitter grammar for this language
    - L55:12@func_kinds:&'static [&'static str] // kinds that represent a function/method definition
    - L71:12@const_kinds:&'static [&'static str] // kinds that represent a top-level constant/variable declaration
    - L83:12@call_kinds:&'static [&'static str] // kinds that represent a call expression
    - L93:12@comment_kinds:&'static [&'static str] // node kinds that represent a comment
    - L102:12@return_field:Option<&'static str> // field name holding a function's return type (if the grammar has one)
# refs
    - from_path@L20 calls L31:12@as_str:&'static str
# note
