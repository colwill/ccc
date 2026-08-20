# main.rs.md (20260820-07-57-23) UTC
# source: src/main.rs [rust]
# modules
# imports
    - L1@anyhow (anyhow, Context, Result)
    - L2@clap (Parser, Subcommand, ValueEnum)
    - L3@codecache (CheckReport, Encoding, ChangesOptions, ChangesReport)
    - L4@std::path (Component, Path, PathBuf)
    - L5@std::process (ExitCode)
    - L548@std::os::unix::fs (PermissionsExt)
# const
    - L21@Text:OutputFormat
    - L23@Json:OutputFormat
    - L29@Scan:Command
    - L38@Check:Command
    - L45@Tokenize:Command
    - L52@Export:Command
    - L70@Changes:Command
    - L105@Serve:Command
    - L125@Insights:Command
    - L135@Install:Command
# funcs
    - L145:4@main:ExitCode
    - L155:4@run:Result<ExitCode>
    - L345:4@print_changes_text
    - L423:4@print_check_text
    - L437:4@print_check_json:Result<()> // Emit `{ root, up_to_date, files[], changes[] }` as one JSON line. `files` is
    - L467:4@rel_join:PathBuf // Join `rest` onto `root`, dropping a leading `./` so paths read cleanly
    - L479:4@path_str:String // Path as a forward-slash string (stable for CI output regardless of platform).
    - L485:4@html_title:String // title for a generated HTML report: the output file's stem
    - L492:4@run_tokenize:Result<()>
    - L516:4@run_install:Result<ExitCode> // Copy the running `ccc` binary into a directory on the user's PATH.
    - L565:4@default_bin_dir:Result<PathBuf> // `~/.local/bin` — the XDG user-local binary directory on Linux.
    - L572:4@expand_tilde:PathBuf // Expand a leading `~` (or `~/`) to `$HOME`; leave other paths untouched.
    - L582:4@same_file:bool // True if both paths resolve to the same existing file.
    - L590:4@dir_on_path:bool // True if `dir` is one of the entries in `$PATH`.
    - L600:4@canonical:PathBuf
# refs
    - main@L146 calls L155:4@run:Result<ExitCode>
    - run@L163 calls L600:4@canonical:PathBuf
    - run@L176 calls L492:4@run_tokenize:Result<()>
    - run@L184 calls L600:4@canonical:PathBuf
    - run@L186 calls L423:4@print_check_text
    - run@L187 calls L437:4@print_check_json:Result<()>
    - run@L201 calls L600:4@canonical:PathBuf
    - run@L204 calls L479:4@path_str:String
    - run@L240 calls L600:4@canonical:PathBuf
    - run@L241 calls L492:4@run_tokenize:Result<()>
    - run@L255 calls L600:4@canonical:PathBuf
    - run@L269 calls L485:4@html_title:String
    - run@L287 calls L479:4@path_str:String
    - run@L290 calls L485:4@html_title:String
    - run@L296 calls L345:4@print_changes_text
    - run@L321 calls L600:4@canonical:PathBuf
    - run@L325 calls L600:4@canonical:PathBuf
    - run@L341 calls L516:4@run_install:Result<ExitCode>
    - print_check_json@L438 calls L467:4@rel_join:PathBuf
    - print_check_json@L453 calls L479:4@path_str:String
    - run_install@L521 calls L572:4@expand_tilde:PathBuf
    - run_install@L522 calls L565:4@default_bin_dir:Result<PathBuf>
    - run_install@L529 calls L582:4@same_file:bool
    - run_install@L554 calls L590:4@dir_on_path:bool
# note
