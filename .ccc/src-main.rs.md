# main.rs.md (20260701-13-08-47) UTC
# source: src/main.rs [rust]
# const
# funcs
    - L53:4@main:ExitCode
    - L63:4@run:Result<ExitCode>
    - L111:4@print_check_text
    - L125:4@print_check_json:Result<()> // Emit `{ root, up_to_date, files[], changes[] }` as one JSON line. `files` is
    - L155:4@rel_join:PathBuf // Join `rest` onto `root`, dropping a leading `./` so paths read cleanly
    - L167:4@path_str:String // Path as a forward-slash string (stable for CI output regardless of platform).
    - L171:4@run_tokenize:Result<()>
    - L190:4@canonical:PathBuf
# refs
    - main@L54 calls L63:4@run:Result<ExitCode>
    - run@L71 calls L190:4@canonical:PathBuf
    - run@L84 calls L171:4@run_tokenize:Result<()>
    - run@L92 calls L190:4@canonical:PathBuf
    - run@L94 calls L111:4@print_check_text
    - run@L95 calls L125:4@print_check_json:Result<()>
    - run@L104 calls L190:4@canonical:PathBuf
    - run@L105 calls L171:4@run_tokenize:Result<()>
    - print_check_json@L126 calls L155:4@rel_join:PathBuf
    - print_check_json@L141 calls L167:4@path_str:String
# note
