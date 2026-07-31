//! walk a project, build caches, and write / verify `.ccc`

use crate::languages::Language;
use crate::model::{Counts, FileCache};
use crate::{extract, naming, render};
use anyhow::{Context, Result};
use ignore::WalkBuilder;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

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
    pub ccc_dir: PathBuf,
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
    let mut caches: Vec<FileCache> = files.iter().filter_map(|p| build_one(root, p)).collect();
    caches.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    disambiguate_cache_names(&mut caches);
    caches
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
    let lang = Language::from_path(path)?;
    let src = fs::read_to_string(path).ok()?;
    let ex = extract::extract(lang, &src)?;
    let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
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
    })
}

fn render_all(root: &Path, caches: &[FileCache], ts: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for c in caches {
        map.insert(c.cache_name.clone(), render::render_file(c, ts));
    }
    map.insert("CCC.md".to_string(), render::render_index(root, caches, ts));
    map
}

// scan root and (re)write the `.ccc` directory
pub fn scan(root: &Path) -> Result<ScanReport> {
    let files = collect_files(root)?;
    let caches = build_caches(root, &files);
    let ts = render::now_ts();
    let rendered = render_all(root, &caches, &ts);

    let ccc = root.join(".ccc");
    fs::create_dir_all(&ccc).with_context(|| format!("creating {}", ccc.display()))?;
    clear_generated(&ccc)?;
    crate::tokenize::clear(&ccc)?;
    for (name, content) in &rendered {
        let path = ccc.join(name);
        fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    }

    let mut totals = Counts::default();
    for c in &caches {
        totals.add(c.counts());
    }
    Ok(ScanReport {
        files: caches.len(),
        totals,
        ccc_dir: ccc,
    })
}

// verify .ccc outputs for CI
pub fn check(root: &Path) -> Result<CheckReport> {
    let files = collect_files(root)?;
    let caches = build_caches(root, &files);
    let ts = render::now_ts();
    let expected = render_all(root, &caches, &ts);

    let ccc = root.join(".ccc");
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
        for entry in fs::read_dir(&ccc)? {
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
