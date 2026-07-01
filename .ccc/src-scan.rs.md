# scan.rs.md (20260701-13-08-47) UTC
# source: src/scan.rs [rust]
# const
    - L13@SKIP_DIRS:&[&str]
    - L30@MAX_FILE_BYTES:u64
# funcs
    - L55:12@as_str:&'static str
    - L73:8@collect_files:Result<Vec<PathBuf>> // Discover supported source files under `root`
    - L113:8@build_caches:Vec<FileCache> // parse every discovered file into a `FileCache`, sorted by path
    - L122:4@build_one:Option<FileCache>
    - L139:4@render_all:BTreeMap<String, String>
    - L149:8@scan:Result<ScanReport> // scan root and (re)write the `.ccc` directory
    - L177:8@check:Result<CheckReport> // verify .ccc outputs for CI
    - L228:4@clear_generated:Result<()>
# refs
    - build_caches@L116 calls L122:4@build_one:Option<FileCache>
    - scan@L150 calls L73:8@collect_files:Result<Vec<PathBuf>>
    - scan@L151 calls L113:8@build_caches:Vec<FileCache>
    - scan@L153 calls L139:4@render_all:BTreeMap<String, String>
    - scan@L158 calls L228:4@clear_generated:Result<()>
    - check@L178 calls L73:8@collect_files:Result<Vec<PathBuf>>
    - check@L179 calls L113:8@build_caches:Vec<FileCache>
    - check@L181 calls L139:4@render_all:BTreeMap<String, String>
    - check@L221 calls L55:12@as_str:&'static str
# note
