//! parses kept by content, so a file unchanged since an earlier run is read back rather than parsed again

use crate::extract::{self, Extracted};
use crate::languages::Language;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

// builds whose parses are kept side by side - the one in development, the editor's, a release
const KEEP_BUILDS: usize = 4;

// stamped each time a build opens its directory, so the sweep knows which ran last
const USED: &str = "last-used";

static ACTIVE: OnceLock<Option<ParseCache>> = OnceLock::new();

// one build's parses, each under the hash of what was parsed
pub struct ParseCache {
    dir: PathBuf,
}

// keep parses under the user's cache directory - the binary turns this on, a library caller parses afresh
pub fn enable() {
    let root = crate::voice::cache_dir().map(|d| d.join("parses"));
    let _ = ACTIVE.set(root.and_then(|r| ParseCache::open(&r)));
}

pub fn active() -> Option<&'static ParseCache> {
    ACTIVE.get().and_then(Option::as_ref)
}

impl ParseCache {
    // this build's directory under `root` - `None` when it cannot be made, and every file is parsed
    pub fn open(root: &Path) -> Option<ParseCache> {
        let dir = root.join(build_id()?);
        fs::create_dir_all(&dir).ok()?;
        sweep(root, &dir);
        Some(ParseCache { dir })
    }

    // what `extract::extract` makes of `src`, read back when kept - an entry that will not read is replaced
    pub fn extract(&self, lang: Language, src: &str) -> Option<Extracted> {
        let honour_skips = !extract::skips_ignored();
        let path = self.dir.join(key(lang, honour_skips, src));
        let kept = fs::read(&path).ok().and_then(|b| serde_json::from_slice::<Option<Extracted>>(&b).ok());
        if let Some(kept) = kept {
            return kept;
        }
        let parsed = extract::extract_with(lang, src, honour_skips);
        keep(&path, &parsed);
        parsed
    }
}

// the parse's address - everything `extract_with` reads and no path, so a copy or rename reuses it
fn key(lang: Language, honour_skips: bool, src: &str) -> String {
    let mut h = Sha256::new();
    h.update(lang.as_str());
    h.update([0, u8::from(honour_skips)]);
    h.update(src);
    format!("{}.json", hex(&h.finalize()))
}

// written beside its place and renamed in, so no reader sees half an entry
fn keep(path: &Path, parsed: &Option<Extracted>) {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let Ok(body) = serde_json::to_vec(parsed) else { return };
    let tmp = path.with_extension(format!("{}-{}.tmp", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
    if fs::write(&tmp, body).is_err() || fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

// the version plus the binary's size and mtime - a rebuild without a version bump must not read old parses
fn build_id() -> Option<String> {
    let meta = fs::metadata(std::env::current_exe().ok()?).ok()?;
    let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_nanos();
    let mut h = Sha256::new();
    h.update(meta.len().to_le_bytes());
    h.update(modified.to_le_bytes());
    Some(format!("{}-{}", env!("CARGO_PKG_VERSION"), &hex(&h.finalize())[..16]))
}

// stamp `current` as the latest build to run, and clear all but the most recently run builds
fn sweep(root: &Path, current: &Path) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let _ = fs::write(current.join(USED), now.to_string());
    let Ok(entries) = fs::read_dir(root) else { return };
    let mut builds: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .map(|p| (fs::metadata(p.join(USED)).and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH), p))
        .collect();
    builds.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, old) in builds.into_iter().skip(KEEP_BUILDS) {
        if old != current {
            let _ = fs::remove_dir_all(old);
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ccc-parses-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn json(ex: &Option<Extracted>) -> String {
        serde_json::to_string(ex).expect("encode")
    }

    const SRC: &str = "// TODO: tidy\nfn charge(c: u64) -> u64 {\n    for _ in 0..3 { helper(); }\n    if c > 1 { charge(c - 1) } else { c }\n}\nfn helper() { let f = File::open(\"x\"); }\n";
    const C_SRC: &str = "#include <stdio.h>\nvoid f(void) {\n    FILE *x = fopen(\"a\", \"r\");\n    while (x) { fclose(x); }\n}\n";

    // a parse read back is the parse that was kept - resource pairs and measurements included
    #[test]
    fn a_kept_parse_reads_back_as_the_parse_itself() {
        let dir = root("roundtrip");
        let cache = ParseCache::open(&dir).expect("open");
        for (lang, src) in [(Language::Rust, SRC), (Language::C, C_SRC)] {
            let fresh = extract::extract(lang, src);
            let first = cache.extract(lang, src);
            let again = cache.extract(lang, src);
            assert_eq!(json(&first), json(&fresh));
            assert_eq!(json(&again), json(&fresh));
        }
        let c = cache.extract(Language::C, C_SRC).expect("parse");
        assert_eq!(c.funcs[0].metrics.resources.len(), 2, "{:?}", c.funcs[0].metrics);
        assert_eq!(fs::read_dir(&cache.dir).expect("dir").count(), 3, "two entries beside the stamp");
        // the language is part of the address, so a file read as another one never gets this parse
        cache.extract(Language::Go, SRC);
        assert_eq!(fs::read_dir(&cache.dir).expect("dir").count(), 4);
        let _ = fs::remove_dir_all(&dir);
    }

    // an entry that will not read is parsed again and replaced, never trusted
    #[test]
    fn a_broken_entry_is_parsed_again() {
        let dir = root("broken");
        let cache = ParseCache::open(&dir).expect("open");
        let path = cache.dir.join(key(Language::Rust, !extract::skips_ignored(), SRC));
        fs::write(&path, b"{\"consts\": [").expect("write");
        let read = cache.extract(Language::Rust, SRC);
        assert_eq!(json(&read), json(&extract::extract(Language::Rust, SRC)));
        assert!(serde_json::from_slice::<Option<Extracted>>(&fs::read(&path).expect("read")).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    // old builds' parses are cleared, but never the running build's own
    #[test]
    fn only_the_most_recent_builds_are_kept() {
        let dir = root("sweep");
        for i in 0..KEEP_BUILDS + 2 {
            let old = dir.join(format!("0.0.{i}-old"));
            fs::create_dir_all(&old).expect("mkdir");
            let stamp = fs::File::create(old.join(USED)).expect("stamp");
            // long ago, so the running build is the latest however coarse the clock
            stamp.set_modified(UNIX_EPOCH + std::time::Duration::from_secs(i as u64 + 1)).expect("age");
        }
        let cache = ParseCache::open(&dir).expect("open");
        let left: Vec<PathBuf> = fs::read_dir(&dir).expect("dir").flatten().map(|e| e.path()).collect();
        assert_eq!(left.len(), KEEP_BUILDS);
        assert!(left.contains(&cache.dir));
        let _ = fs::remove_dir_all(&dir);
    }
}
