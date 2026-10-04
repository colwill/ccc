//! walk a project, build caches, and write / verify `.ccc`

use crate::languages::Language;
use crate::model::{Counts, FileCache};
use crate::{extract, naming, render};
use anyhow::{Context, Result};
use ignore::WalkBuilder;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

// don't scan these dirs even with `.gitignore`
const SKIP_DIRS: &[&str] = &[
    ".ccc",
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    "dist",
    "build",
    "out",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
];

// skip lage files
const MAX_FILE_BYTES: u64 = 2_000_000;

pub struct ScanReport {
    pub files: usize,
    pub totals: Counts,
    // The markdown as rendered, kept only when it was written
    pub rendered: BTreeMap<String, String>,
    // where the markdown was written
    pub out_dir: Option<PathBuf>,
}

pub struct CheckReport {
    pub up_to_date: bool,
    pub changes: Vec<Change>,
}

// How a committed cache file differs from what a fresh scan would produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    // Present, but its content (ignoring timestamps) differs.
    Modified,
    // A cache file a fresh scan would write is absent.
    Missing,
    // A committed cache file a fresh scan would no longer write.
    Stale,
}

impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Modified => "modified",
            ChangeKind::Missing => "missing",
            ChangeKind::Stale => "stale",
        }
    }
}

// A single out-of-date cache file reported by [`check`].
#[derive(Clone, Debug)]
pub struct Change {
    pub kind: ChangeKind,
    // Cache file name inside `.ccc`, e.g. `src-main.rs.md`.
    pub file: String,
}

// Discover supported source files under `root`
pub fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .parents(false)
        .git_global(false)
        .filter_entry(|e| {
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if !is_dir {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !SKIP_DIRS.contains(&name.as_ref())
        })
        .build();

    for dent in walker {
        let dent = match dent {
            Ok(d) => d,
            Err(_) => continue,
        };
        if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = dent.path();
        if Language::from_path(path).is_none() {
            continue;
        }
        if let Ok(meta) = dent.metadata() {
            if meta.len() > MAX_FILE_BYTES {
                continue;
            }
        }
        out.push(path.to_path_buf());
    }
    out.sort();
    Ok(out)
}

// parse every discovered file into a `FileCache`, sorted by path
pub fn build_caches(root: &Path, files: &[PathBuf]) -> Vec<FileCache> {
    build_caches_reusing(root, files, |_| None)
}

// The caches for `files`, each that `held` still has for a file - one that
// has not changed since - taken from there rather than parsed again.
pub fn build_caches_reusing(root: &Path, files: &[PathBuf], held: impl Fn(&PathBuf) -> Option<FileCache>) -> Vec<FileCache> {
    let mut caches = Vec::with_capacity(files.len());
    let mut fresh = Vec::new();
    for f in files {
        match held(f) {
            Some(mut c) => {
                // named afresh below, against whatever else is here now
                c.cache_name = naming::cache_name(&c.rel_path);
                caches.push(c);
            }
            None => fresh.push(f.clone()),
        }
    }
    caches.extend(parse_all(root, &fresh));
    caches.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    disambiguate_cache_names(&mut caches);
    caches
}

// the main thread's default - `visit` recurses once per syntax tree level
const PARSE_STACK_BYTES: usize = 8 << 20;

// every file parses on its own, so spread them over all cores; workers pull the
// next file off a shared counter so one large file never stalls a whole batch
fn parse_all(root: &Path, files: &[PathBuf]) -> Vec<FileCache> {
    let workers = thread::available_parallelism().map_or(1, |n| n.get()).min(files.len());
    let next = AtomicUsize::new(0);
    let drain = || {
        let mut out = Vec::new();
        while let Some(path) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
            out.extend(build_one(root, path));
        }
        out
    };
    thread::scope(|s| {
        // the calling thread drains too, so a helper that fails to spawn costs speed, not files
        let helpers: Vec<_> = (1..workers)
            .filter_map(|_| {
                thread::Builder::new()
                    .stack_size(PARSE_STACK_BYTES)
                    .spawn_scoped(s, drain)
                    .ok()
            })
            .collect();
        let mut caches = drain();
        for h in helpers {
            caches.extend(h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)));
        }
        caches
    })
}

// fixes bug where cache_name wasnt unique oops
fn disambiguate_cache_names(caches: &mut [FileCache]) {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for c in caches.iter() {
        *counts.entry(c.cache_name.as_str()).or_default() += 1;
    }
    let collisions: BTreeSet<String> = counts
        .into_iter()
        .filter(|&(_, n)| n > 1)
        .map(|(name, _)| name.to_string())
        .collect();
    for c in caches.iter_mut() {
        if collisions.contains(&c.cache_name) {
            c.cache_name = naming::cache_name_disambiguated(&c.rel_path);
        }
    }
}

fn build_one(root: &Path, path: &Path) -> Option<FileCache> {
    Language::from_path(path)?;
    let src = fs::read_to_string(path).ok()?;
    read_one(path.strip_prefix(root).unwrap_or(path), &src)
}

// one file's text parsed into a `FileCache` - read from disk, or a text not
// written yet; none for a file in no language ccc reads, or one that will not parse
pub fn read_one(rel: &Path, src: &str) -> Option<FileCache> {
    let lang = Language::from_path(rel)?;
    let ex = extract::extract(lang, src)?;
    let rel = rel.to_path_buf();
    Some(FileCache {
        cache_name: naming::cache_name(&rel),
        display_name: naming::display_name(&rel),
        rel_path: rel,
        language: lang,
        lines: src.lines().count(),
        consts: ex.consts,
        funcs: ex.funcs,
        refs: ex.refs,
        notes: ex.notes,
        calls: ex.calls,
        uses: ex.uses,
        imports: ex.imports,
        types: ex.types,
        modules: ex.modules,
        annotations: ex.annotations,
        withdrawn: ex.withdrawn,
        constructs: ex.constructs,
    })
}

// Discovered files `ccc:skip` withdrew whole, as paths relative to `root`.
// They are missing from `caches` the same way a file that would not parse is,
// so the directive is what tells the two apart.
pub fn withdrawn_files(root: &Path, files: &[PathBuf], caches: &[FileCache]) -> Vec<String> {
    // under `--ignore-skip` the directive withdraws nothing, so a missing file
    // is one that would not parse
    if extract::skips_ignored() {
        return Vec::new();
    }
    let mapped: BTreeSet<&Path> = caches.iter().map(|c| c.rel_path.as_path()).collect();
    files
        .iter()
        .filter_map(|p| {
            let rel = p.strip_prefix(root).unwrap_or(p);
            if mapped.contains(rel) {
                return None;
            }
            let src = fs::read_to_string(p).ok()?;
            extract::has_skip_directive(&src)
                .then(|| rel.to_string_lossy().replace('\\', "/"))
        })
        .collect()
}

// the whole cache as `name -> markdown`, which is what gets written, compared
// against, or encoded - never re-derived from whatever is on disk
pub fn render_all(root: &Path, caches: &[FileCache], ts: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for c in caches {
        map.insert(c.cache_name.clone(), render::render_file(c, ts));
    }
    map.insert("CCC.md".to_string(), render::render_index(root, caches, ts));
    map
}

// Parse `root` into the map, and write the markdown only when `out` names
// somewhere to put it. Every other command builds this same map in memory and
// never reads the files, so writing them is a choice rather than a step.
pub fn scan(root: &Path, out: Option<&Path>) -> Result<ScanReport> {
    let files = collect_files(root)?;
    let caches = build_caches(root, &files);

    let (out_dir, rendered) = match out {
        Some(dir) => {
            let ts = render::now_ts();
            let rendered = render_all(root, &caches, &ts);
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            clear_generated(dir)?;
            crate::tokenize::clear(dir)?;
            for (name, content) in &rendered {
                let path = dir.join(name);
                fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
            }
            (Some(dir.to_path_buf()), rendered)
        }
        // rendering is only ever done to be written, so without a destination
        // it is not done at all
        None => (None, BTreeMap::new()),
    };

    let mut totals = Counts::default();
    for c in &caches {
        totals.add(c.counts());
    }
    Ok(ScanReport {
        files: caches.len(),
        totals,
        rendered,
        out_dir,
    })
}

// Verify a written cache for CI. Deprecated alongside the `ccc check` command:
// it answers "does the copy on disk match the source", and since `scan` only
// writes a copy when `--dir` asks for one, most projects no longer have a copy
// to be stale. The map every other entry point reads is built from source each
// time and cannot go out of date.
#[deprecated(
    since = "1.4.3",
    note = "a written cache is now opt-in; regenerate with `scan(root, Some(dir))` and diff it, \
            or read the in-memory map that every other entry point builds"
)]
pub fn check(root: &Path, ccc: &Path) -> Result<CheckReport> {
    let files = collect_files(root)?;
    let caches = build_caches(root, &files);
    let ts = render::now_ts();
    let expected = render_all(root, &caches, &ts);

    let mut changes = Vec::new();

    for (name, content) in &expected {
        match fs::read_to_string(ccc.join(name)) {
            Ok(actual) => {
                if render::strip_timestamps(&actual) != render::strip_timestamps(content) {
                    changes.push(Change {
                        kind: ChangeKind::Modified,
                        file: name.clone(),
                    });
                }
            }
            Err(_) => changes.push(Change {
                kind: ChangeKind::Missing,
                file: name.clone(),
            }),
        }
    }

    if ccc.is_dir() { // clean stale
        let mut existing = BTreeSet::new();
        for entry in fs::read_dir(ccc)? {
            let name = entry?.file_name().to_string_lossy().to_string();
            if name.ends_with(".md") {
                existing.insert(name);
            }
        }
        for name in existing {
            if !expected.contains_key(&name) {
                changes.push(Change {
                    kind: ChangeKind::Stale,
                    file: name,
                });
            }
        }
    }

    changes.sort_by(|a, b| (&a.file, a.kind.as_str()).cmp(&(&b.file, b.kind.as_str())));
    Ok(CheckReport {
        up_to_date: changes.is_empty(),
        changes,
    })
}

fn clear_generated(ccc: &Path) -> Result<()> {
    for entry in fs::read_dir(ccc)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension().map(|e| e == "md").unwrap_or(false) {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ccc-scan-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).expect("mkdir");
        fs::write(dir.join("src/lib.rs"), "pub fn charge(c: u64) -> u64 { c }\n").expect("write");
        dir
    }

    fn md_names(dir: &Path) -> BTreeSet<String> {
        fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.ends_with(".md"))
                    .collect()
            })
            .unwrap_or_default()
    }

    // The map is the product; the markdown is a copy of it somebody asked for.
    // A scan that writes nothing has to leave the tree exactly as it found it -
    // not an empty `.ccc`, not a directory at all.
    #[test]
    fn a_scan_without_a_destination_writes_nothing() {
        let dir = fixture("memory");
        let report = scan(&dir, None).expect("scan");
        assert_eq!(report.files, 1);
        assert_eq!(report.totals.funcs, 1);
        assert!(report.out_dir.is_none());
        assert!(!dir.join(".ccc").exists(), "a bare scan created .ccc");
        let _ = fs::remove_dir_all(&dir);
    }

    // `check` is deprecated, not gone - it still has to work for the pipelines
    // that call it, so it is still covered
    #[allow(deprecated)]
    #[test]
    fn a_destination_gets_the_markdown_and_check_reads_it_back() {
        let dir = fixture("written");
        for out in [dir.join(".ccc"), dir.join("docs/map")] {
            let report = scan(&dir, Some(&out)).expect("scan");
            assert_eq!(report.out_dir.as_deref(), Some(out.as_path()));
            let names = md_names(&out);
            assert!(names.contains("CCC.md"), "{names:?}");
            assert!(names.contains("src-lib.rs.md"), "{names:?}");
            // and the cache just written is by definition up to date
            assert!(check(&dir, &out).expect("check").up_to_date);
        }
        // a destination that was never written reads as wholly missing rather
        // than as up to date
        let report = check(&dir, &dir.join("nowhere")).expect("check");
        assert!(!report.up_to_date);
        assert!(report.changes.iter().all(|c| c.kind == ChangeKind::Missing));
        let _ = fs::remove_dir_all(&dir);
    }

    // rewriting a destination clears what a previous scan left, so a deleted
    // source file does not linger as a cache entry nothing maps to
    #[test]
    fn rewriting_a_destination_clears_the_last_one() {
        let dir = fixture("stale");
        let out = dir.join(".ccc");
        fs::write(dir.join("src/gone.rs"), "pub fn gone() {}\n").expect("write");
        scan(&dir, Some(&out)).expect("scan");
        assert!(md_names(&out).contains("src-gone.rs.md"));

        fs::remove_file(dir.join("src/gone.rs")).expect("rm");
        scan(&dir, Some(&out)).expect("scan");
        assert!(!md_names(&out).contains("src-gone.rs.md"));
        let _ = fs::remove_dir_all(&dir);
    }
}
