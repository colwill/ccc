# naming.rs.md (20260729-17-57-11) UTC
# source: src/naming.rs [rust]
# const
# funcs
    - L6:8@cache_name:String // name of the cache file inside `.ccc`, e.g. `src/extract.rs` -> `src-extract.rs.md`
    - L10:8@cache_name_disambiguated:String
    - L14:4@cache_name_inner:String
    - L43:4@short_hash:String // fnv-1a of the full relative path as 8 lowercase hex digits
    - L54:8@display_name:String // header display name `src/extract.rs` -> `extract.rs.md`
    - L66:4@sanitize:String
    - L84:8@nested_and_root_paths
    - L95:8@colliding_paths_disambiguate
# refs
    - cache_name@L7 calls L14:4@cache_name_inner:String
    - cache_name_disambiguated@L11 calls L14:4@cache_name_inner:String
    - cache_name_disambiguated@L11 calls L43:4@short_hash:String
    - cache_name_inner@L29 calls L66:4@sanitize:String
    - cache_name_inner@L34 calls L66:4@sanitize:String
    - colliding_paths_disambiguate@L99 calls L10:8@cache_name_disambiguated:String
    - colliding_paths_disambiguate@L100 calls L10:8@cache_name_disambiguated:String
# note
