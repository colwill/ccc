# scan.rs.md (20260703-15-47-40) UTC
# source: src/scan.rs [rust]
# const
    - L13@SKIP_DIRS:&[&str]
    - L30@MAX_FILE_BYTES:u64
# funcs
    - L55:12@as_str:&'static str
    - L73:8@collect_files:Result<Vec<PathBuf>> // Discover supported source files under `root`
    - L113:8@build_caches:Vec<FileCache> // parse every discovered file into a `FileCache`, sorted by path
    - L121:4@disambiguate_cache_names // ensure every `cache_name` is unique wit distinct source paths
    - L138:4@build_one:Option<FileCache>
    - L155:4@render_all:BTreeMap<String, String>
    - L165:8@scan:Result<ScanReport> // scan root and (re)write the `.ccc` directory
    - L192:8@check:Result<CheckReport> // verify .ccc outputs for CI
    - L243:4@clear_generated:Result<()>
# refs
    - build_caches@L114 calls L138:4@build_one:Option<FileCache>
    - build_caches@L116 calls L121:4@disambiguate_cache_names
    - scan@L166 calls L73:8@collect_files:Result<Vec<PathBuf>>
    - scan@L167 calls L113:8@build_caches:Vec<FileCache>
    - scan@L169 calls L155:4@render_all:BTreeMap<String, String>
    - scan@L173 calls L243:4@clear_generated:Result<()>
    - check@L193 calls L73:8@collect_files:Result<Vec<PathBuf>>
    - check@L194 calls L113:8@build_caches:Vec<FileCache>
    - check@L196 calls L155:4@render_all:BTreeMap<String, String>
# note
