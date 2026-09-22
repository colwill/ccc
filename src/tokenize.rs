//! Pre-encode the generated `.ccc` corpus into a persisted token stream using a
//! pretrained tiktoken vocabulary. Downstream consumers load raw token IDs
//! (`&[u32]`) directly from `tokens.bin` - no re-tokenization at load time.
//!
//! Layout written into the cache directory (`.ccc/` unless `--dir` moved it):
//! - `tokens.bin`  - little-endian `u32` token IDs for every cache file, concatenated
//! - `tokens.json` - index: encoding, layout, and per-file `(offset, len)` in tokens

use anyhow::{anyhow, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use tiktoken_rs::CoreBPE;

pub const TOKENS_BIN: &str = "tokens.bin";
pub const TOKENS_INDEX: &str = "tokens.json";
const INDEX_VERSION: u32 = 1;

// disclaimer embedded in `tokens.json` so the stream is never mistaken for a
// claude-ready or exact-count artifact
const NOTE: &str = "APPROXIMATE tiktoken IDs - NOT compatible with Claude/Anthropic models. \
Claude uses a different tokenizer, and its Messages API accepts text, not token IDs, so these \
IDs cannot be loaded into Claude and only roughly approximate its token counts (tiktoken \
undercounts Claude tokens, more so on code). Intended for a downstream model that shares this \
tiktoken vocabulary, or for rough size estimates. For exact Claude token counts, use \
Anthropic's /v1/messages/count_tokens endpoint.";

// pretrained tiktoken vocabulary
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    O200kBase,
    Cl100kBase,
}

impl Encoding {
    pub fn parse(s: &str) -> Option<Encoding> {
        match s {
            "o200k_base" | "o200k" => Some(Encoding::O200kBase),
            "cl100k_base" | "cl100k" => Some(Encoding::Cl100kBase),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Encoding::O200kBase => "o200k_base",
            Encoding::Cl100kBase => "cl100k_base",
        }
    }

    // load the (embedded) BPE ranks for this encoding
    pub fn load(self) -> Result<CoreBPE> {
        let bpe = match self {
            Encoding::O200kBase => tiktoken_rs::o200k_base(),
            Encoding::Cl100kBase => tiktoken_rs::cl100k_base(),
        };
        bpe.map_err(|e| anyhow!("loading {} vocab: {e}", self.name()))
    }
}

// per-file location within the token stream
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FileEntry {
    pub file: String,
    pub offset: usize,
    pub len: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TokenIndex {
    pub version: u32,
    pub encoding: String,
    #[serde(default)]
    pub approximate: bool,
    #[serde(default)]
    pub note: String,
    pub token_width: u8,  // bytes per token in `tokens.bin` (always 4)
    pub endianness: String, // byte order of each token (always "le")
    pub total_tokens: usize,
    pub files: Vec<FileEntry>,
}

pub struct TokenizeReport {
    pub files: usize,
    pub total_tokens: usize,
    pub bytes: usize,
    pub encoding: Encoding,
    pub bin_path: PathBuf,
}

// Encode a rendered map into `tokens.bin` + `tokens.json` under `ccc`, then
// verify the persisted stream decodes back to it.
//
// The corpus is passed in rather than read back off disk: the map is built in
// memory by whoever called this, and re-reading the markdown would encode a
// round trip through the filesystem instead of the thing that was mapped. It
// also means a token stream can be produced for a project that never writes
// the markdown at all.
pub fn tokenize(
    corpus: &BTreeMap<String, String>,
    ccc: &Path,
    enc: Encoding,
) -> Result<TokenizeReport> {
    ensure!(!corpus.is_empty(), "nothing to encode - the map is empty");
    let bpe = enc.load()?;
    let names = ordered_names(corpus);

    let mut stream: Vec<u32> = Vec::new();
    let mut entries: Vec<FileEntry> = Vec::new();
    for name in &names {
        let text = &corpus[name];
        let toks = bpe.encode_ordinary(text);
        entries.push(FileEntry {
            file: name.clone(),
            offset: stream.len(),
            len: toks.len(),
        });
        stream.extend_from_slice(&toks);
    }

    fs::create_dir_all(ccc).with_context(|| format!("creating {}", ccc.display()))?;

    // tokens.bin - little-endian u32 stream
    let mut bytes = Vec::with_capacity(stream.len() * 4);
    for t in &stream {
        bytes.extend_from_slice(&t.to_le_bytes());
    }
    let bin_path = ccc.join(TOKENS_BIN);
    fs::write(&bin_path, &bytes).with_context(|| format!("writing {}", bin_path.display()))?;

    // tokens.json index
    let index = TokenIndex {
        version: INDEX_VERSION,
        encoding: enc.name().to_string(),
        approximate: true,
        note: NOTE.to_string(),
        token_width: 4,
        endianness: "le".to_string(),
        total_tokens: stream.len(),
        files: entries,
    };
    let json = serde_json::to_string_pretty(&index)?;
    fs::write(ccc.join(TOKENS_INDEX), json)
        .with_context(|| format!("writing {}", ccc.join(TOKENS_INDEX).display()))?;

    // verify the persisted artifacts round-trip back to the exact corpus
    verify_roundtrip(ccc, enc, corpus, &names)?;

    Ok(TokenizeReport {
        files: index.files.len(),
        total_tokens: stream.len(),
        bytes: bytes.len(),
        encoding: enc,
        bin_path,
    })
}

// remove persisted token artifacts (used when regenerating the cache without
// re-tokenizing, so stale tokens never linger).
pub fn clear(ccc: &Path) -> Result<()> {
    for name in [TOKENS_BIN, TOKENS_INDEX] {
        let p = ccc.join(name);
        if p.exists() {
            fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
        }
    }
    Ok(())
}

// reload persisted tokens from disk and confirm they decode to the corpus
fn verify_roundtrip(
    ccc: &Path,
    enc: Encoding,
    corpus: &BTreeMap<String, String>,
    names: &[String],
) -> Result<()> {
    let cache = TokenCache::load(ccc)?;
    let bpe = enc.load()?;
    let mut expected = String::new();
    for name in names {
        expected.push_str(&corpus[name]);
    }
    let decoded = bpe
        .decode(cache.all())
        .map_err(|e| anyhow!("decoding token stream: {e}"))?;
    ensure!(decoded == expected, "token round-trip verification failed");
    Ok(())
}

// the corpus in stream order: the index first, then the rest by name, so the
// layout is stable and a reader can find `CCC.md` at offset zero
fn ordered_names(corpus: &BTreeMap<String, String>) -> Vec<String> {
    let mut names: Vec<String> = corpus.keys().cloned().collect();
    if let Some(pos) = names.iter().position(|n| n == "CCC.md") {
        let c = names.remove(pos);
        names.insert(0, c);
    }
    names
}

// loaded token cache: the raw `u32` stream plus its index
// constructed by reading `tokens.bin` / `tokens.json` with no BPE pass
pub struct TokenCache {
    pub encoding: Encoding,
    pub tokens: Vec<u32>,
    pub index: TokenIndex,
}

impl TokenCache {
    pub fn load(ccc: &Path) -> Result<TokenCache> {
        let idx_path = ccc.join(TOKENS_INDEX);
        let json = fs::read_to_string(&idx_path)
            .with_context(|| format!("reading {} (run `ccc tokenize`)", idx_path.display()))?;
        let index: TokenIndex = serde_json::from_str(&json)
            .with_context(|| format!("parsing {}", idx_path.display()))?;
        ensure!(
            index.version == INDEX_VERSION,
            "unsupported token index version {}",
            index.version
        );
        ensure!(
            index.token_width == 4 && index.endianness == "le",
            "unsupported token layout ({}x{})",
            index.token_width,
            index.endianness
        );
        let encoding = Encoding::parse(&index.encoding)
            .ok_or_else(|| anyhow!("unknown encoding {}", index.encoding))?;

        let bytes = fs::read(ccc.join(TOKENS_BIN))?;
        ensure!(
            bytes.len() == index.total_tokens * 4,
            "tokens.bin size {} does not match index ({} tokens)",
            bytes.len(),
            index.total_tokens
        );
        let tokens = bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();

        Ok(TokenCache {
            encoding,
            tokens,
            index,
        })
    }

    // raw token slice for one cache file (no re-tokenization)
    pub fn file(&self, name: &str) -> Option<&[u32]> {
        let e = self.index.files.iter().find(|e| e.file == name)?;
        self.tokens.get(e.offset..e.offset + e.len)
    }

    // entire concatenated token stream
    pub fn all(&self) -> &[u32] {
        &self.tokens
    }

    // decode token IDs back to text
    pub fn decode(&self, toks: &[u32]) -> Result<String> {
        self.encoding
            .load()?
            .decode(toks)
            .map_err(|e| anyhow!("decode: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("CCC.md".to_string(), "# CodeCaChe\n# files\n".to_string()),
            (
                "src-main.rs.md".to_string(),
                "# main.rs.md\n# funcs\n    - L1:4@main\n".to_string(),
            ),
        ])
    }

    #[test]
    fn tokenize_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ccc-tok-{}", std::process::id()));
        let ccc = dir.join(".ccc");
        // the destination need not exist, and the corpus need never be on disk
        let report = tokenize(&corpus(), &ccc, Encoding::O200kBase).unwrap();
        assert_eq!(report.files, 2);
        assert!(report.total_tokens > 0);
        assert_eq!(report.bytes, report.total_tokens * 4);

        let cache = TokenCache::load(&ccc).unwrap();
        // stream is labeled approximate / non-Claude
        assert!(cache.index.approximate);
        assert!(cache.index.note.contains("Claude"));
        // idx-first ordering
        assert_eq!(cache.index.files[0].file, "CCC.md");
        // per-file slice decodes back to the original file
        let toks = cache.file("src-main.rs.md").unwrap();
        let decoded = cache.decode(toks).unwrap();
        assert_eq!(decoded, "# main.rs.md\n# funcs\n    - L1:4@main\n");
        // no markdown was written, only the stream
        assert!(!ccc.join("CCC.md").exists());

        fs::remove_dir_all(&dir).ok();
    }
}
