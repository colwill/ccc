# scan.rs.md (20260820-07-57-23) UTC
# source: src/scan.rs [rust]
# modules
# imports
    - L3@crate::languages (Language)
    - L4@crate::model (Counts, FileCache)
    - L5@crate (extract, naming, render)
    - L6@anyhow (Context, Result)
    - L7@ignore (WalkBuilder)
    - L8@std::collections (BTreeMap, BTreeSet, HashMap)
    - L9@std (fs)
    - L10@std::path (Path, PathBuf)
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
    - L162:4@render_all:BTreeMap<String, String>
    - L172:8@scan:Result<ScanReport> // scan root and (re)write the `.ccc` directory
    - L199:8@check:Result<CheckReport> // verify .ccc outputs for CI
    - L250:4@clear_generated:Result<()>
# refs
    - build_caches@L114 calls L138:4@build_one:Option<FileCache>
    - build_caches@L116 calls L121:4@disambiguate_cache_names
    - scan@L173 calls L73:8@collect_files:Result<Vec<PathBuf>>
    - scan@L174 calls L113:8@build_caches:Vec<FileCache>
    - scan@L176 calls L162:4@render_all:BTreeMap<String, String>
    - scan@L180 calls L250:4@clear_generated:Result<()>
    - check@L200 calls L73:8@collect_files:Result<Vec<PathBuf>>
    - check@L201 calls L113:8@build_caches:Vec<FileCache>
    - check@L203 calls L162:4@render_all:BTreeMap<String, String>
# note
