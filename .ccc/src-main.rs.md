# main.rs.md (20260703-15-47-40) UTC
# source: src/main.rs [rust]
# const
# funcs
    - L62:4@main:ExitCode
    - L72:4@run:Result<ExitCode>
    - L121:4@print_check_text
    - L135:4@print_check_json:Result<()> // Emit `{ root, up_to_date, files[], changes[] }` as one JSON line. `files` is
    - L165:4@rel_join:PathBuf // Join `rest` onto `root`, dropping a leading `./` so paths read cleanly
    - L177:4@path_str:String // Path as a forward-slash string (stable for CI output regardless of platform).
    - L181:4@run_tokenize:Result<()>
    - L205:4@run_install:Result<ExitCode> // Copy the running `ccc` binary into a directory on the user's PATH.
    - L254:4@default_bin_dir:Result<PathBuf> // `~/.local/bin` — the XDG user-local binary directory on Linux.
    - L261:4@expand_tilde:PathBuf // Expand a leading `~` (or `~/`) to `$HOME`; leave other paths untouched.
    - L271:4@same_file:bool // True if both paths resolve to the same existing file.
    - L279:4@dir_on_path:bool // True if `dir` is one of the entries in `$PATH`.
    - L289:4@canonical:PathBuf
# refs
    - main@L63 calls L72:4@run:Result<ExitCode>
    - run@L80 calls L289:4@canonical:PathBuf
    - run@L93 calls L181:4@run_tokenize:Result<()>
    - run@L101 calls L289:4@canonical:PathBuf
    - run@L103 calls L121:4@print_check_text
    - run@L104 calls L135:4@print_check_json:Result<()>
    - run@L113 calls L289:4@canonical:PathBuf
    - run@L114 calls L181:4@run_tokenize:Result<()>
    - run@L117 calls L205:4@run_install:Result<ExitCode>
    - print_check_json@L136 calls L165:4@rel_join:PathBuf
    - print_check_json@L151 calls L177:4@path_str:String
    - run_install@L210 calls L261:4@expand_tilde:PathBuf
    - run_install@L211 calls L254:4@default_bin_dir:Result<PathBuf>
    - run_install@L218 calls L271:4@same_file:bool
    - run_install@L243 calls L279:4@dir_on_path:bool
# note
