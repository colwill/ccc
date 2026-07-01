# naming.rs.md (20260701-13-08-47) UTC
# source: src/naming.rs [rust]
# const
# funcs
    - L6:8@cache_name:String // name of the cache file inside `.ccc`, e.g. `src/extract.rs` -> `src-extract.rs.md`
    - L31:8@display_name:String // header display name, e.g. `src/extract.rs` -> `extract.rs.md`
    - L43:4@sanitize:String
    - L61:8@nested_and_root_paths
# refs
    - cache_name@L21 calls L43:4@sanitize:String
    - cache_name@L26 calls L43:4@sanitize:String
# note
