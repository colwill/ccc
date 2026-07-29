# scan.rs.md (20260729-22-00-57) UTC
# source: src/scan.rs [rust]
# const
    - L13@SKIP_DIRS:&[&str]
    - L30@MAX_FILE_BYTES:u64
    - L47@Modified:ChangeKind
    - L49@Missing:ChangeKind
    - L51@Stale:ChangeKind
# funcs
    - L55:12@as_str:&'static str
    - L73:8@collect_files:Result<Vec<PathBuf>> // Discover supported source files under `root`
    - L113:8@build_caches:Vec<FileCache> // parse every discovered file into a `FileCache`, sorted by path
    - L121:4@disambiguate_cache_names // fixes bug where cache_name wasnt unique oops
    - L138:4@build_one:Option<FileCache>
    - L158:4@render_all:BTreeMap<String, String>
    - L168:8@scan:Result<ScanReport> // scan root and (re)write the `.ccc` directory
    - L195:8@check:Result<CheckReport> // verify .ccc outputs for CI
    - L246:4@clear_generated:Result<()>
# refs
    - build_caches@L114 calls L138:4@build_one:Option<FileCache>
    - build_caches@L116 calls L121:4@disambiguate_cache_names
    - scan@L169 calls L73:8@collect_files:Result<Vec<PathBuf>>
    - scan@L170 calls L113:8@build_caches:Vec<FileCache>
    - scan@L172 calls L158:4@render_all:BTreeMap<String, String>
    - scan@L176 calls L246:4@clear_generated:Result<()>
    - check@L196 calls L73:8@collect_files:Result<Vec<PathBuf>>
    - check@L197 calls L113:8@build_caches:Vec<FileCache>
    - check@L199 calls L158:4@render_all:BTreeMap<String, String>
# note
    - @L120 fixes bug where cache_name wasnt unique oops
