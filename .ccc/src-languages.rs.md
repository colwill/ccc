# languages.rs.md (20260703-15-47-40) UTC
# source: src/languages.rs [rust]
# const
# funcs
    - L17:12@from_path:Option<Language>
    - L30:12@as_str:&'static str
    - L42:12@ts_language:TsLanguage // tree-sitter grammar for this language
    - L54:12@func_kinds:&'static [&'static str] // kinds that represent a function/method definition
    - L70:12@const_kinds:&'static [&'static str] // kinds that represent a top-level constant/variable declaration
    - L82:12@call_kinds:&'static [&'static str] // kinds that represent a call expression
    - L92:12@comment_kinds:&'static [&'static str] // node kinds that represent a comment
    - L101:12@return_field:Option<&'static str> // field name holding a function's return type (if the grammar has one)
# refs
# note
