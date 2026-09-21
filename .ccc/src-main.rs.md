# main.rs.md (20260921-12-11-30) UTC
# source: src/main.rs [rust]
# modules
# imports
    - L1@anyhow (anyhow, Context, Result)
    - L2@clap (Parser, Subcommand, ValueEnum)
    - L3@codecache (CheckReport, Encoding, ChangesOptions, ChangesReport)
    - L4@std::path (Component, Path, PathBuf)
    - L5@std::process (ExitCode)
    - L575@codecache::sast (Severity)
    - L923@std::os::unix::fs (PermissionsExt)
# const
    - L21@Text:OutputFormat
    - L23@Json:OutputFormat
    - L29@Scan:Command
    - L38@Check:Command
    - L45@Tokenize:Command
    - L52@Export:Command
    - L70@Changes:Command
    - L118@Deps:Command
    - L136@Prompts:Command
    - L155@Serve:Command
    - L177@Insights:Command
    - L187@Sast:Command
    - L200@Audit:Command
    - L213@Install:Command
# funcs
    - L223:4@main:ExitCode
    - L554:4@gate_introduced:bool // `--fail-introduced` shared by `changes` and `deps` when the
    - L574:4@print_sast_text
    - L608:4@print_audit_text
    - L666:4@print_prompts_text
    - L714:4@print_changes_text
    - L798:4@print_check_text
    - L812:4@print_check_json:Result<()> // Emit `{ root, up_to_date, files[], changes[] }` as one JSON line. `files` is
    - L842:4@rel_join:PathBuf // Join `rest` onto `root`, dropping a leading `./` so paths read cleanly
    - L854:4@path_str:String // Path as a forward-slash string (stable for CI output regardless of platform).
    - L860:4@html_title:String // title for a generated HTML report: the output file's stem
    - L867:4@run_tokenize:Result<()>
    - L891:4@run_install:Result<ExitCode> // Copy the running `ccc` binary into a directory on the user's PATH.
    - L940:4@default_bin_dir:Result<PathBuf> // `~/.local/bin` — the XDG user-local binary directory on Linux.
    - L947:4@expand_tilde:PathBuf // Expand a leading `~` (or `~/`) to `$HOME`; leave other paths untouched.
    - L957:4@same_file:bool // True if both paths resolve to the same existing file.
    - L965:4@dir_on_path:bool // True if `dir` is one of the entries in `$PATH`.
    - L975:4@canonical:PathBuf
# refs
    - print_check_json@L813 calls L842:4@rel_join:PathBuf
    - print_check_json@L828 calls L854:4@path_str:String
    - run_install@L896 calls L947:4@expand_tilde:PathBuf
    - run_install@L897 calls L940:4@default_bin_dir:Result<PathBuf>
    - run_install@L904 calls L957:4@same_file:bool
    - run_install@L929 calls L965:4@dir_on_path:bool
# note
