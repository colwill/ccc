# tokenize.rs.md (20260729-17-57-11) UTC
# source: src/tokenize.rs [rust]
# const
    - L15@TOKENS_BIN:&str
    - L16@TOKENS_INDEX:&str
    - L17@INDEX_VERSION:u32
    - L21@NOTE:&str
# funcs
    - L36:12@parse:Option<Encoding>
    - L44:12@name:&'static str
    - L52:12@load:Result<CoreBPE> // load the (embedded) BPE ranks for this encoding
    - L93:8@tokenize:Result<TokenizeReport> // encode every `.md` cache file under `<root>/.ccc` into `tokens.bin` +
    - L155:8@clear:Result<()> // remove persisted token artifacts (used when regenerating the cache without
    - L166:4@verify_roundtrip:Result<()> // reload persisted tokens from disk and confirm they decode to the corpus
    - L181:4@list_markdown:Result<Vec<String>>
    - L207:12@load:Result<TokenCache>
    - L248:12@file:Option<&[u32]> // raw token slice for one cache file (no re-tokenization)
    - L254:12@all:&[u32] // entire concatenated token stream
    - L259:12@decode:Result<String> // decode token IDs back to text
    - L272:8@tokenize_load_roundtrip
# refs
    - tokenize@L102 calls L181:4@list_markdown:Result<Vec<String>>
    - tokenize@L142 calls L166:4@verify_roundtrip:Result<()>
    - tokenize_load_roundtrip@L283 calls L93:8@tokenize:Result<TokenizeReport>
# note
