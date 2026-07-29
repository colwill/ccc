# main.rs.md (20260729-17-57-11) UTC
# source: src/main.rs [rust]
# const
# funcs
    - L111:4@main:ExitCode
    - L121:4@run:Result<ExitCode>
    - L246:4@print_surf_text
    - L288:4@print_check_text
    - L302:4@print_check_json:Result<()> // Emit `{ root, up_to_date, files[], changes[] }` as one JSON line. `files` is
    - L332:4@rel_join:PathBuf // Join `rest` onto `root`, dropping a leading `./` so paths read cleanly
    - L344:4@path_str:String // Path as a forward-slash string (stable for CI output regardless of platform).
    - L350:4@html_title:String // title for a generated HTML report: the output file's stem
    - L357:4@run_tokenize:Result<()>
    - L381:4@run_install:Result<ExitCode> // Copy the running `ccc` binary into a directory on the user's PATH.
    - L430:4@default_bin_dir:Result<PathBuf> // `~/.local/bin` — the XDG user-local binary directory on Linux.
    - L437:4@expand_tilde:PathBuf // Expand a leading `~` (or `~/`) to `$HOME`; leave other paths untouched.
    - L447:4@same_file:bool // True if both paths resolve to the same existing file.
    - L455:4@dir_on_path:bool // True if `dir` is one of the entries in `$PATH`.
    - L465:4@canonical:PathBuf
# refs
    - main@L112 calls L121:4@run:Result<ExitCode>
    - run@L129 calls L465:4@canonical:PathBuf
    - run@L142 calls L357:4@run_tokenize:Result<()>
    - run@L150 calls L465:4@canonical:PathBuf
    - run@L152 calls L288:4@print_check_text
    - run@L153 calls L302:4@print_check_json:Result<()>
    - run@L162 calls L465:4@canonical:PathBuf
    - run@L163 calls L357:4@run_tokenize:Result<()>
    - run@L176 calls L465:4@canonical:PathBuf
    - run@L189 calls L350:4@html_title:String
    - run@L206 calls L344:4@path_str:String
    - run@L209 calls L350:4@html_title:String
    - run@L215 calls L246:4@print_surf_text
    - run@L239 calls L465:4@canonical:PathBuf
    - run@L242 calls L381:4@run_install:Result<ExitCode>
    - print_check_json@L303 calls L332:4@rel_join:PathBuf
    - print_check_json@L318 calls L344:4@path_str:String
    - run_install@L386 calls L437:4@expand_tilde:PathBuf
    - run_install@L387 calls L430:4@default_bin_dir:Result<PathBuf>
    - run_install@L394 calls L447:4@same_file:bool
    - run_install@L419 calls L455:4@dir_on_path:bool
# note
