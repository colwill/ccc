# languages.rs.md (20260820-07-57-23) UTC
# source: src/languages.rs [rust]
# modules
# imports
    - L3@std::path (Path)
    - L4@tree_sitter (Language, TsLanguage)
    - L558@super
    - L559@std::collections (BTreeSet)
# const
    - L8@Rust:Language
    - L9@Python:Language
    - L10@JavaScript:Language
    - L11@TypeScript:Language
    - L12@Tsx:Language
    - L13@Go:Language
    - L14@Cpp:Language
    - L15@C:Language
    - L16@CSharp:Language
    - L17@Zig:Language
    - L18@Odin:Language
    - L22@ALL:&'static [Language]
# funcs
    - L36:12@from_path:Option<Language>
    - L55:12@as_str:&'static str
    - L72:12@ts_language:TsLanguage // tree-sitter grammar for this language
    - L89:12@func_kinds:&'static [&'static str] // kinds that represent a function/method definition
    - L115:12@const_kinds:&'static [&'static str] // kinds that represent a top-level constant/variable declaration
    - L134:12@call_kinds:&'static [&'static str] // kinds that represent a call expression
    - L147:12@use_kinds:&'static [&'static str] // kinds that represent a qualified name usable as a value or type
    - L164:12@variant_kinds:&'static [&'static str] // kinds that declare one enum variant/enumerator - indexed as const-like
    - L186:12@import_kinds:&'static [&'static str] // kinds whose subtree is an import/include; qualified names inside are
    - L201:12@comment_kinds:&'static [&'static str] // node kinds that represent a comment
    - L212:12@is_typed:bool // does this language declare types where the syntax tree can read them?
    - L228:12@family:&'static str // languages whose code can call each other's functions directly
    - L242:12@package_scoped:bool // does an unqualified name reach the rest of its directory
    - L250:12@type_kinds:&'static [(&'static str, &'static str)] // kinds that define a named type
    - L312:12@module_kinds:&'static [&'static str] // kinds that declare a module identity a qualifier can name: a Go
    - L325:12@loop_kinds:&'static [&'static str] // kinds that repeat their body - loop-nesting depth and unroll candidates
    - L359:12@branch_kinds:&'static [&'static str] // kinds that fork control flow
    - L396:12@param_list_kinds:&'static [&'static str] // node kinds holding a function's parameter list
    - L408:12@guard_kinds:&'static [&'static str] // kinds that make a resource release automatic
    - L425:12@resource_pairs:&'static [(&'static str, &'static str)] // (acquire, release) call-name pairs
    - L488:12@inline_hint:&'static str // how this language asks the compiler to inline, named in lint advice
    - L505:12@doc_wrapper_kinds:&'static [&'static str] // Nodes that wrap a definition without being part of it
    - L520:12@annotation_kinds:&'static [&'static str] // annotations a language allows between a doc comment and what it
    - L533:12@implicit_member_scope:bool // does an unqualified call inside a type body resolve to that types own
    - L538:12@return_field:Option<&'static str> // field name holding a function's return type (if the grammar has one)
    - L564:8@every_node_kind_exists_in_its_grammar // every node kind named in the tables above must exist in the grammar it is
    - L601:8@every_language_is_reachable_by_extension // every language has to be reachable from a path or it can be configured
# refs
# note
