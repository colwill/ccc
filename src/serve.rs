//! `ccc serve` local REST/MCP endpoints for AI agents.
//!
//! On startup the whole project is parsed into an in-memory map (the same
//! model `.ccc` is rendered from); every query answers from memory. A watcher
//! thread polls a walk fingerprint (path + mtime + size) and swaps a freshly
//! parsed map in whenever source changes - `/refresh` forces it immediately.

use crate::model::{FileCache, Counts};
use crate::{insights, render, scan};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

const MCP_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
const MCP_LATEST: &str = "2025-06-18";

const FIND_CAP: usize = 200;
const REFS_CAP: usize = 500;
const EDGE_SYMBOL_CAP: usize = 20;

pub struct ServeOptions {
    pub addr: String,
    pub port: u16,
    pub watch: Option<std::time::Duration>,
    // serve the human-facing `/insights` UI alongside the agent endpoints
    pub html: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        ServeOptions {
            addr: "127.0.0.1".into(),
            port: 6767,
            watch: Some(std::time::Duration::from_secs(2)),
            html: false,
        }
    }
}

struct MapState {
    root: PathBuf,
    root_label: String,
    ts: String,
    caches: Vec<FileCache>,
    watch_secs: Option<u64>,
    // `/insights` is opt-in (`--html`): it is a human UI, not an agent endpoint
    html: bool,
    // Six MCP tools are views onto one analysis pass. Computing it per call
    // would repeat the whole graph build for each; this keeps one result per
    // (map generation, base ref). Behind a Mutex so it can be filled while
    // holding only a read lock on the map.
    analysis: Mutex<Option<Analysis>>,
}

struct Analysis {
    // the map generation this was computed from
    ts: String,
    base: Option<String>,
    report: Arc<Value>,
}

impl MapState {
    fn build(root: &Path) -> Result<MapState> {
        let files = scan::collect_files(root)?;
        let caches = scan::build_caches(root, &files);
        let root_label = root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(".")
            .to_string();
        Ok(MapState {
            root: root.to_path_buf(),
            root_label,
            ts: render::now_ts(),
            caches,
            watch_secs: None,
            html: false,
            analysis: Mutex::new(None),
        })
    }

    fn rescan(&mut self) -> Result<(usize, usize)> {
        let before = self.caches.len();
        let files = scan::collect_files(&self.root)?;
        self.caches = scan::build_caches(&self.root, &files);
        self.ts = render::now_ts();
        self.invalidate();
        Ok((before, self.caches.len()))
    }

    // swap in a fresh map (built outside lock by watcher)
    fn swap_in(&mut self, caches: Vec<FileCache>) {
        self.caches = caches;
        self.ts = render::now_ts();
        self.invalidate();
    }

    // `ts` alone would do it, but it has one-second resolution - two rescans
    // inside the same second would otherwise serve a stale analysis.
    fn invalidate(&self) {
        if let Ok(mut slot) = self.analysis.lock() {
            *slot = None;
        }
    }

    // The insights analysis for this map, computed at most once per
    // (generation, base). `base` is the git ref the change set diffs against.
    fn analysis(&self, base: Option<&str>) -> Arc<Value> {
        let want = base.map(str::to_string);
        let mut slot = match self.analysis.lock() {
            Ok(s) => s,
            // a panic in another thread must not take the endpoint down; just
            // pay for a fresh pass
            Err(p) => p.into_inner(),
        };
        if let Some(a) = slot.as_ref() {
            if a.ts == self.ts && a.base == want {
                return Arc::clone(&a.report);
            }
        }
        let report = Arc::new(insights::insights(
            &self.caches,
            &self.root,
            &self.root_label,
            &self.ts,
            base,
        ));
        *slot = Some(Analysis {
            ts: self.ts.clone(),
            base: want,
            report: Arc::clone(&report),
        });
        report
    }

    fn path_of(&self, cache: &FileCache) -> String {
        cache.rel_path.to_string_lossy().replace('\\', "/")
    }

    // find a file by relative path, cache name, or unique path suffix
    fn find_file(&self, key: &str) -> Result<&FileCache, String> {
        let norm = key.trim().trim_start_matches("./");
        if let Some(c) = self
            .caches
            .iter()
            .find(|c| self.path_of(c) == norm || c.cache_name == norm)
        {
            return Ok(c);
        }
        let matches: Vec<&FileCache> = self
            .caches
            .iter()
            .filter(|c| self.path_of(c).ends_with(norm))
            .collect();
        match matches.len() {
            1 => Ok(matches[0]),
            0 => {
                let mut close: Vec<String> = self
                    .caches
                    .iter()
                    .map(|c| self.path_of(c))
                    .filter(|p| {
                        let n = norm.to_ascii_lowercase();
                        p.to_ascii_lowercase().contains(&n)
                    })
                    .take(10)
                    .collect();
                close.sort();
                if close.is_empty() {
                    Err(format!("no file matching '{key}' in the map"))
                } else {
                    Err(format!(
                        "no file matching '{key}'; close: {}",
                        close.join(", ")
                    ))
                }
            }
            _ => Err(format!(
                "'{key}' is ambiguous: {}",
                matches
                    .iter()
                    .map(|c| self.path_of(c))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    // symbol name -> indexes of files defining it (as a function)
    fn def_files(&self) -> BTreeMap<&str, BTreeSet<usize>> {
        let mut out: BTreeMap<&str, BTreeSet<usize>> = BTreeMap::new();
        for (i, c) in self.caches.iter().enumerate() {
            for f in &c.funcs {
                out.entry(f.name.as_str()).or_default().insert(i);
            }
        }
        out
    }
}


// comps every mapped file with its mtime (unix nanos) and size.
type Fingerprint = BTreeMap<PathBuf, (u128, u64)>;

fn fingerprint(root: &Path) -> Result<Fingerprint> {
    let mut fp = Fingerprint::new();
    for f in scan::collect_files(root)? {
        if let Ok(md) = std::fs::metadata(&f) {
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            fp.insert(f, (mtime, md.len()));
        }
    }
    Ok(fp)
}

fn fingerprint_delta(a: &Fingerprint, b: &Fingerprint) -> usize {
    let changed = a
        .iter()
        .filter(|(path, meta)| b.get(*path) != Some(meta))
        .count();
    let added = b.keys().filter(|p| !a.contains_key(*p)).count();
    changed + added
}

fn check_and_rebuild(
    root: &Path,
    last: &Fingerprint,
) -> Result<Option<(Fingerprint, Vec<FileCache>, usize)>> {
    let now = fingerprint(root)?;
    if now == *last {
        return Ok(None);
    }
    let delta = fingerprint_delta(last, &now);
    let files: Vec<PathBuf> = now.keys().cloned().collect();
    let caches = scan::build_caches(root, &files);
    Ok(Some((now, caches, delta)))
}

fn spawn_watcher(state: Arc<RwLock<MapState>>, root: PathBuf, interval: std::time::Duration) {
    std::thread::spawn(move || {
        let mut last = fingerprint(&root).unwrap_or_default();
        let mut warned = false;
        loop {
            std::thread::sleep(interval);
            match check_and_rebuild(&root, &last) {
                Ok(Some((fp, caches, delta))) => {
                    let n = caches.len();
                    let ts = {
                        let mut map = state.write().expect("map lock poisoned");
                        map.swap_in(caches);
                        map.ts.clone()
                    };
                    last = fp;
                    warned = false;
                    println!("map refreshed: {n} files ({delta} changed) at {ts}");
                }
                Ok(None) => {}
                Err(e) => {
                    // transient FS trouble; keep watching, complain once
                    if !warned {
                        eprintln!("watcher: {e:#} (will keep polling)");
                        warned = true;
                    }
                }
            }
        }
    });
}

fn q_index(map: &MapState) -> Value {
    let mut totals = Counts::default();
    let files: Vec<Value> = map
        .caches
        .iter()
        .map(|c| {
            let n = c.counts();
            totals.add(n);
            json!({
                "path": map.path_of(c),
                "language": c.language.as_str(),
                "funcs": n.funcs,
                "consts": n.consts,
                "refs": n.refs,
                "notes": n.notes,
            })
        })
        .collect();
    json!({
        "root": map.root_label,
        "generated": map.ts,
        "totals": {
            "files": map.caches.len(),
            "funcs": totals.funcs,
            "consts": totals.consts,
            "refs": totals.refs,
            "notes": totals.notes,
        },
        "files": files,
    })
}

fn q_find(map: &MapState, query: &str, kind: &str) -> Result<Value, String> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return Err("empty query".into());
    }
    if !matches!(kind, "any" | "func" | "const" | "note" | "call") {
        return Err(format!("kind '{kind}' not one of any|func|const|note|call"));
    }
    let (want_qualifier, want_name) = split_qualified(query.trim());
    let want_qualifier = want_qualifier.map(|s| s.to_ascii_lowercase());
    let want_name = want_name.to_ascii_lowercase();
    // qualified queries (`serde_json::to_string`) resolve against call sites;
    // under `any` unqualified calls stay out so definitions aren't drowned.
    // uses (enum variants, consts) are constant-like and rare, so they are
    // searched under `any` even unqualified
    let search_calls =
        kind == "call" || (kind == "any" && want_qualifier.is_some() && !want_name.is_empty());
    let search_uses = matches!(kind, "any" | "call") && !want_name.is_empty();
    let mut results = Vec::new();
    for c in &map.caches {
        let path = map.path_of(c);
        if matches!(kind, "any" | "func") {
            for f in &c.funcs {
                if f.name.to_ascii_lowercase().contains(&q) {
                    results.push(json!({
                        "kind": "func", "file": path, "line": f.line, "col": f.col,
                        "name": f.name, "ret": f.ret, "doc": f.comment,
                        "span": [f.start_line, f.end_line],
                    }));
                }
            }
        }
        if matches!(kind, "any" | "const") {
            for k in &c.consts {
                if k.name.to_ascii_lowercase().contains(&q) {
                    results.push(json!({
                        "kind": "const", "file": path, "line": k.line,
                        "name": k.name, "type": k.ty,
                    }));
                }
            }
        }
        if matches!(kind, "any" | "note") {
            for n in &c.notes {
                if n.text.to_ascii_lowercase().contains(&q) {
                    results.push(json!({
                        "kind": "note", "file": path, "line": n.line, "text": n.text,
                    }));
                }
            }
        }
        let sites = c
            .calls
            .iter()
            .map(|s| ("call", s))
            .filter(|_| search_calls)
            .chain(c.uses.iter().map(|s| ("use", s)).filter(|_| search_uses));
        for (site_kind, site) in sites {
            let name_ok =
                !want_name.is_empty() && site.name.to_ascii_lowercase().contains(&want_name);
            let qualifier_ok = match &want_qualifier {
                None => true,
                Some(w) => site
                    .qualifier
                    .as_deref()
                    .map(|cq| qualifier_matches(Some(&cq.to_ascii_lowercase()), w))
                    .unwrap_or(false),
            };
            if name_ok && qualifier_ok {
                results.push(json!({
                    "kind": site_kind, "file": path, "line": site.line,
                    "name": site.name, "qualifier": site.qualifier,
                    "caller": site.caller, "test_ctx": site.test_ctx,
                }));
            }
        }
    }
    let total = results.len();
    results.truncate(FIND_CAP);
    Ok(json!({
        "query": query,
        "kind": kind,
        "count": total,
        "truncated": total > FIND_CAP,
        "results": results,
    }))
}

// `serde_json::to_string` / `client.charge` -> (Some("serde_json"), "to_string");
// a bare name passes through as (None, name).
fn split_qualified(symbol: &str) -> (Option<&str>, &str) {
    let by_colons = symbol.rfind("::").map(|i| (i, i + 2));
    let by_dot = symbol.rfind('.').map(|i| (i, i + 1));
    let sep = match (by_colons, by_dot) {
        (Some(a), Some(b)) => Some(if a.0 > b.0 { a } else { b }),
        (a, b) => a.or(b),
    };
    match sep {
        Some((start, end)) if end < symbol.len() => {
            let name = &symbol[end..];
            let qualifier = symbol[..start].trim();
            if qualifier.is_empty() {
                (None, name)
            } else {
                (Some(qualifier), name)
            }
        }
        _ => (None, symbol),
    }
}

// a call qualifier matches when its identifier segments end with the wanted
// segments: want `serde_json` matches `serde_json`; want `a::b` matches
// `x::a::b` but not `b`.
fn qualifier_matches(call_qualifier: Option<&str>, want: &str) -> bool {
    let Some(cq) = call_qualifier else {
        return false;
    };
    fn segs(s: &str) -> Vec<&str> {
        s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|seg| !seg.is_empty())
            .collect()
    }
    let have = segs(cq);
    let want = segs(want);
    !want.is_empty() && have.len() >= want.len() && have[have.len() - want.len()..] == want[..]
}

fn q_references(map: &MapState, symbol: &str) -> Result<Value, String> {
    let symbol = symbol.trim();
    if symbol.is_empty() {
        return Err("empty symbol".into());
    }
    let (qualifier, name) = split_qualified(symbol);
    let mut definitions = Vec::new();
    let mut stem_matched_defs = Vec::new();
    let mut references = Vec::new();
    for c in &map.caches {
        let path = map.path_of(c);
        let stem = c
            .rel_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        for f in &c.funcs {
            if f.name == name {
                let def = json!({
                    "kind": "func", "file": path, "line": f.line, "col": f.col,
                    "ret": f.ret, "doc": f.comment, "span": [f.start_line, f.end_line],
                });
                if qualifier.is_some_and(|q| crate::changes::qualifier_names_service(q, stem)) {
                    stem_matched_defs.push(def.clone());
                }
                definitions.push(def);
            }
        }
        for k in &c.consts {
            if k.name == name {
                let def = json!({
                    "kind": "const", "file": path, "line": k.line, "type": k.ty,
                });
                // `money::MAX` narrows by file stem, `Encoding::O200kBase`
                // by the owning enum recorded as the const's type
                let narrows = qualifier.is_some_and(|q| {
                    crate::changes::qualifier_names_service(q, stem)
                        || k.ty
                            .as_deref()
                            .is_some_and(|t| crate::changes::qualifier_names_service(q, t))
                });
                if narrows {
                    stem_matched_defs.push(def.clone());
                }
                definitions.push(def);
            }
        }
        for (kind, site) in c
            .calls
            .iter()
            .map(|s| ("call", s))
            .chain(c.uses.iter().map(|s| ("use", s)))
        {
            let qualifier_ok = match qualifier {
                None => true,
                Some(q) => qualifier_matches(site.qualifier.as_deref(), q),
            };
            if site.name == name && qualifier_ok {
                references.push(json!({
                    "kind": kind, "file": path, "line": site.line, "caller": site.caller,
                    "qualifier": site.qualifier, "test_ctx": site.test_ctx,
                }));
            }
        }
    }
    // a qualifier that names a file (`money::charge` -> lib/money.rs) narrows
    // the definitions to that file; otherwise all bare-name definitions stay
    if !stem_matched_defs.is_empty() {
        definitions = stem_matched_defs;
    }
    if definitions.is_empty() && references.is_empty() {
        return Err(format!("symbol '{symbol}' not found in the map"));
    }
    let total_refs = references.len();
    references.truncate(REFS_CAP);
    Ok(json!({
        "symbol": symbol,
        "name": name,
        "qualifier": qualifier,
        "counts": {"definitions": definitions.len(), "references": total_refs},
        "truncated": total_refs > REFS_CAP,
        "definitions": definitions,
        "references": references,
    }))
}

// package name from a root Cargo.toml, if any - the name code uses to
// qualify calls through the crate facade
fn cargo_package_name(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let mut in_package = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_package = t == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(v) = t.strip_prefix("name").map(str::trim_start) {
            if let Some(v) = v.strip_prefix('=') {
                return Some(v.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

// File-level dependency edges, resolved from imports and calls. A call only
// produces a cross-file edge with positive evidence - its qualifier names the
// target module, or the callee (or its qualifier) is imported from the target
// file. Name-only matches (`.ok()` vs a local `fn ok`) are excluded: they
// produced phantom edges when a stdlib method name collided with a project fn.
fn q_dependencies(map: &MapState, file: Option<&str>) -> Result<Value, String> {
    let defs = map.def_files();
    fn path_segs(s: &str) -> impl Iterator<Item = &str> {
        s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
    }
    let stems: Vec<&str> = map
        .caches
        .iter()
        .map(|c| c.rel_path.file_stem().and_then(|s| s.to_str()).unwrap_or(""))
        .collect();
    // names a file answers to: its stem, a facade file's containing directory
    // (pkg/__init__.py, utils/index.ts, sub/mod.rs), and - for the crate root -
    // the Cargo package name (`codecache::scan(..)` in main.rs names lib.rs)
    let mut stem_files: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, s) in stems.iter().enumerate() {
        stem_files.entry(s.to_string()).or_default().push(i);
        if matches!(*s, "__init__" | "index" | "mod") {
            if let Some(dir) = map.caches[i]
                .rel_path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|d| d.to_str())
            {
                stem_files.entry(dir.to_string()).or_default().push(i);
            }
        }
    }
    if let Some(pkg) = cargo_package_name(&map.root) {
        if let Some(i) = map
            .caches
            .iter()
            .position(|c| c.rel_path.ends_with("src/lib.rs") || c.rel_path == Path::new("lib.rs"))
        {
            // hyphenated package names appear underscored in code
            stem_files.entry(pkg.replace('-', "_")).or_default().push(i);
        }
    }

    // (from, to) -> symbols
    let mut edges: BTreeMap<(usize, usize), BTreeSet<String>> = BTreeMap::new();
    let mut ambiguous: BTreeSet<String> = BTreeSet::new();
    let mut excluded: BTreeSet<String> = BTreeSet::new();

    // pass 1: import edges (covering type-only dependencies the call map
    // cannot see) and, per file, which files each bound name came from
    let mut imports_of: Vec<BTreeMap<&str, BTreeSet<usize>>> = Vec::new();
    for (a, c) in map.caches.iter().enumerate() {
        let mut imported: BTreeMap<&str, BTreeSet<usize>> = BTreeMap::new();
        for imp in &c.imports {
            for seg in path_segs(&imp.module) {
                let Some(files) = stem_files.get(seg) else { continue };
                for &b in files {
                    if b == a {
                        continue;
                    }
                    let syms = edges.entry((a, b)).or_default();
                    if imp.names.is_empty() {
                        syms.insert(seg.to_string());
                    }
                    for n in &imp.names {
                        syms.insert(n.clone());
                        imported.entry(n).or_default().insert(b);
                    }
                }
            }
            // bound names that themselves name a module file
            // (`use crate::{scan, render}`, `from pkg import helpers`)
            for n in &imp.names {
                let Some(files) = stem_files.get(n.as_str()) else { continue };
                for &b in files {
                    if b == a {
                        continue;
                    }
                    edges.entry((a, b)).or_default().insert(n.clone());
                    imported.entry(n).or_default().insert(b);
                }
            }
        }
        imports_of.push(imported);
    }

    // pass 2: call edges with evidence
    for (a, c) in map.caches.iter().enumerate() {
        let imported = &imports_of[a];
        for call in &c.calls {
            let Some(files) = defs.get(call.name.as_str()) else { continue };
            if files.contains(&a) {
                continue; // resolves locally
            }
            let qual_segs: Vec<&str> = call
                .qualifier
                .as_deref()
                .map(|q| path_segs(q).filter(|s| !s.is_empty()).collect())
                .unwrap_or_default();
            let evidenced: Vec<usize> = files
                .iter()
                .copied()
                .filter(|&b| {
                    // the qualifier names the target module directly
                    let by_qualifier = qual_segs.contains(&stems[b]);
                    let name_imported = |n: &str| imported.get(n).is_some_and(|s| s.contains(&b));
                    // the callee (or the type/module it hangs off) is imported
                    // from the target file
                    let by_import =
                        name_imported(&call.name) || qual_segs.iter().any(|s| name_imported(s));
                    // the qualifier names a facade that re-exports the callee
                    // from the target file (`codecache::scan(..)` where lib.rs
                    // does `pub use scan::scan`)
                    let by_reexport = qual_segs.iter().any(|seg| {
                        stem_files.get(*seg).is_some_and(|fs| {
                            fs.iter().any(|&f| {
                                f != b
                                    && imports_of[f]
                                        .get(call.name.as_str())
                                        .is_some_and(|s| s.contains(&b))
                            })
                        })
                    });
                    by_qualifier || by_import || by_reexport
                })
                .collect();
            match evidenced[..] {
                [] => {
                    excluded.insert(call.name.clone());
                }
                [b] => {
                    edges.entry((a, b)).or_default().insert(call.name.clone());
                }
                _ => {
                    ambiguous.insert(call.name.clone());
                }
            }
        }
    }

    let edge_json = |(&(a, b), symbols): (&(usize, usize), &BTreeSet<String>)| {
        json!({
            "from": map.path_of(&map.caches[a]),
            "to": map.path_of(&map.caches[b]),
            "symbols": symbols.iter().take(EDGE_SYMBOL_CAP).collect::<Vec<_>>(),
        })
    };

    match file {
        None => Ok(json!({
            "files": map.caches.len(),
            "edges": edges.iter().map(edge_json).collect::<Vec<_>>(),
            "ambiguous_symbols": ambiguous.iter().take(50).collect::<Vec<_>>(),
            // call names that matched a definition elsewhere but lacked
            // qualifier/import evidence - surfaced so exclusions are auditable
            "excluded_symbols": excluded.iter().take(50).collect::<Vec<_>>(),
        })),
        Some(key) => {
            let target = map.find_file(key)?;
            let idx = map
                .caches
                .iter()
                .position(|c| std::ptr::eq(c, target))
                .unwrap_or_default();
            let depends_on: Vec<Value> = edges
                .iter()
                .filter(|((a, _), _)| *a == idx)
                .map(edge_json)
                .collect();
            let depended_on_by: Vec<Value> = edges
                .iter()
                .filter(|((_, b), _)| *b == idx)
                .map(edge_json)
                .collect();
            Ok(json!({
                "file": map.path_of(target),
                "depends_on": depends_on,
                "depended_on_by": depended_on_by,
            }))
        }
    }
}

fn q_file(map: &MapState, key: &str) -> Result<Value, String> {
    let c = map.find_file(key)?;
    Ok(json!({
        "path": map.path_of(c),
        "language": c.language.as_str(),
        "cache_name": c.cache_name,
        "consts": c.consts.iter().map(|k| json!({
            "line": k.line, "name": k.name, "type": k.ty,
        })).collect::<Vec<_>>(),
        "funcs": c.funcs.iter().map(|f| json!({
            "line": f.line, "col": f.col, "name": f.name, "ret": f.ret,
            "doc": f.comment, "span": [f.start_line, f.end_line],
        })).collect::<Vec<_>>(),
        "refs": c.refs.iter().map(|r| json!({
            "caller": r.caller, "call_line": r.call_line,
            "target": r.target_name, "target_line": r.target_line,
        })).collect::<Vec<_>>(),
        "notes": c.notes.iter().map(|n| json!({
            "line": n.line, "text": n.text,
        })).collect::<Vec<_>>(),
        "markdown": render::render_file(c, &map.ts),
    }))
}

fn q_notes(map: &MapState, marker: Option<&str>) -> Value {
    let want = marker.map(|m| m.trim().to_ascii_uppercase());
    let mut notes = Vec::new();
    for c in &map.caches {
        let path = map.path_of(c);
        for n in &c.notes {
            let keep = match &want {
                Some(m) => n.text.to_ascii_uppercase().contains(m),
                None => true,
            };
            if keep {
                notes.push(json!({"file": path, "line": n.line, "text": n.text}));
            }
        }
    }
    json!({"count": notes.len(), "marker": marker, "notes": notes})
}

fn mcp_tools() -> Value {
    let tool = |name: &str, desc: &str, props: Value, required: &[&str]| {
        json!({
            "name": name,
            "description": desc,
            "inputSchema": {
                "type": "object",
                "properties": props,
                "required": required,
            },
        })
    };
    json!({ "tools": [
        tool("index", "Project overview: every mapped file with its function/const/ref/note counts.", json!({}), &[]),
        tool(
            "find",
            "Search the code map for symbols by substring (case-insensitive). Returns file:line locations with return types and doc summaries. A qualified query like `serde_json::to_string` or `client.charge` matches call sites (kind `call`) filtered by that qualifier.",
            json!({
                "query": {"type": "string", "description": "substring to search for; qualified form (a::b / a.b) searches call sites"},
                "kind": {"type": "string", "enum": ["any", "func", "const", "note", "call"], "description": "filter by symbol kind (default any)"},
            }),
            &["query"],
        ),
        tool(
            "references",
            "Definitions, every call site, and qualified value usages (enum variants, consts: `Encoding::O200kBase`) of an exact symbol name across the project. Accepts qualified names (`serde_json::to_string`, `client.charge`) to count one qualifier's sites without any text search. Each hit carries the enclosing caller, qualifier, and test context. Use before changing a function's signature.",
            json!({"symbol": {"type": "string", "description": "exact symbol name, optionally qualified (a::b or a.b)"}}),
            &["symbol"],
        ),
        tool(
            "dependencies",
            "File-level dependency edges resolved from imports and calls (type-only imports included). Call edges require the call site to name the target module or use an imported symbol; name-only matches are excluded and listed in excluded_symbols. Without arguments: the whole project graph; with a file: what it depends on and what depends on it.",
            json!({"file": {"type": "string", "description": "relative path (optional)"}}),
            &[],
        ),
        tool(
            "file",
            "The map entry for one source file: the rendered .ccc markdown (constants, functions with return types and doc summaries, notes). Pass structured=true instead when you need definition spans and the intra-file call graph.",
            json!({
                "path": {"type": "string", "description": "relative path, cache name, or unique path suffix"},
                "structured": {"type": "boolean", "description": "return spans and the intra-file call graph instead of the rendered markdown (default false)"},
            }),
            &["path"],
        ),
        tool(
            "notes",
            "All marker comments (TODO/FIXME/XXX/HACK/BUG/NOTE/SAFETY), optionally filtered by marker.",
            json!({"marker": {"type": "string", "description": "e.g. TODO (optional)"}}),
            &[],
        ),
        tool("refresh", "Rescan the source tree into memory. Call after editing source files.", json!({}), &[]),

        // analysis tools. All six are views onto one pass, computed once
        // per (map generation, base), paged.
        tool(
            "changes",
            "What this branch changed, diffed against a base ref: changed functions with the tests that name them, which services need testing, service edges, and the calls the resolver refused to attribute. Includes uncommitted edits and untracked files by default. This is the change set the `test_triggers` tool refers to.",
            json!({
                "base": {"type": "string", "description": "git ref to diff against (default: merge-base with origin/main, main, origin/master or master - first that exists)"},
                "limit": {"type": "integer", "description": "changed functions per page (default 40, max 500)"},
                "offset": {"type": "integer", "description": "changed functions to skip (default 0)"},
            }),
            &[],
        ),
        tool(
            "test_triggers",
            "Which tests to run for the changes on this branch, and which are missing. Tests are matched to changed functions through the call graph, so a change deep in the stack still surfaces the tests above it; `distance` is how many call hops away each one sits. Returns a runnable command per language. Call this before running a suite, and after editing, to find what a change puts at risk.",
            json!({
                "base": {"type": "string", "description": "git ref to diff against (default: merge-base with origin/main, main, origin/master or master - first that exists)"},
                "limit": {"type": "integer", "description": "triggered tests and gaps per page (default 25, max 500)"},
                "offset": {"type": "integer", "description": "triggered tests and gaps to skip (default 0)"},
            }),
            &[],
        ),
        tool(
            "test_targets",
            "Functions ranked by how much a missing test would cost, each with the kind of test the measurements justify (smoke-test, integration-test, contract-test, perf-test, load-test), the reasoning behind that choice, and language-specific advice. Ranked by complexity, call depth, loop depth, call sites, cross-service callers, and whether anything names the function today.",
            json!({
                "kind": {"type": "string", "enum": ["smoke-test", "integration-test", "contract-test", "perf-test", "load-test"], "description": "only targets recommending this kind"},
                "limit": {"type": "integer", "description": "targets per page (default 15, max 500)"},
                "offset": {"type": "integer", "description": "targets to skip (default 0)"},
            }),
            &[],
        ),
        tool(
            "lints",
            "Syntax-level findings: leaked resources, unrollable loops, inline candidates, deep nesting and similar. Every finding cites the measurement it came from, and every rule ships its own limits - there is no type or data-flow information behind these, so verify before acting.",
            json!({
                "rule": {"type": "string", "description": "only findings from this rule (see the rules section of any result)"},
                "limit": {"type": "integer", "description": "findings per page (default 40, max 500)"},
                "offset": {"type": "integer", "description": "findings to skip (default 0)"},
            }),
            &[],
        ),
        tool(
            "hot",
            "Call-graph shape: the most-called functions, the widest fan-outs, the most complex, the deepest call chains, and recursion cycles. Structural, not measured - it ranks by graph shape, not execution frequency, so treat it as where to look rather than where time goes.",
            json!({
                "view": {"type": "string", "enum": ["most_called", "widest", "most_complex", "deepest_chains", "cycles"], "description": "one view (default: all five)"},
                "limit": {"type": "integer", "description": "rows per view (default 15, max 500)"},
                "offset": {"type": "integer", "description": "rows to skip (default 0)"},
            }),
            &[],
        ),
        tool(
            "services",
            "The service map and the call edges between services, with the call sites that carry each hop. Services come from `.ccc/map.json` when present, top-level directories otherwise. An edge is `declared` if the config lists it, `detected` if calls were resolved across it - both are reported, since a declared HTTP or queue link resolves no calls by design.",
            json!({
                "service": {"type": "string", "description": "drill into one service: its definition plus every edge touching it"},
                "limit": {"type": "integer", "description": "edges per page (default 25, max 500)"},
                "offset": {"type": "integer", "description": "edges to skip (default 0)"},
            }),
            &[],
        ),
    ]})
}

fn mcp_initialize(params: &Value) -> Value {
    let asked = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or(MCP_LATEST);
    let version = if MCP_VERSIONS.contains(&asked) {
        asked
    } else {
        MCP_LATEST
    };
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {}, "resources": {}},
        "serverInfo": {
            "name": "ccc",
            "title": "ContextCodeCache",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "In-memory code map of the project (the .ccc ContextCodeCache). \
            Prefer these tools over grep or text search for symbol and call-site \
            questions in this project: the map already indexes every definition, \
            call site, and qualified constant-like usage (enum variants, consts), \
            each hit is structured (file, line, enclosing caller, qualifier, test \
            context) with no textual false positives, and qualified names like \
            `serde_json::to_string` or `Encoding::O200kBase` are understood \
            directly - one `references` call replaces a grep plus manual filtering. \
            Orient with `index`, locate symbols with `find`, check `references` before \
            changing a signature, `dependencies` for file-level impact, `file` for one \
            file's full map, `notes` for TODO/FIXME markers. The map auto-refreshes \
            when source files change (three seconds of lag); call `refresh` to force an \
            immediate rescan after editing. Reach for text search only for non-symbol \
            text (string literals, config). \
            Six further tools analyse the map rather than index it: `changes` for what \
            this branch touched, `test_triggers` for which tests that makes necessary \
            (call it before running a suite and after editing), `test_targets` for \
            where a missing test would cost most and which kind to write, `lints` for \
            syntax-level findings, `hot` for call-graph shape, `services` for the \
            service map and the calls crossing it. These are heuristics over a syntax \
            tree - no type inference, data flow or runtime profile - so each result \
            carries the evidence behind it and the limits of the rule that produced \
            it; read those before acting. They are paged, not truncated: when a result \
            says `showing 1-40 of 152`, pass `offset` to walk the rest. \
            Results are markdown; the same data is available as JSON from this \
            server's HTTP endpoints (/index, /find, /references, /dependencies, /file, \
            /notes, /insights.json) when something needs to parse it. Open real source \
            files for exact code - this map is for navigation and impact, not \
            authoritative content.",
    })
}

fn mcp_md(text: &str, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

// markdown rendering of tool results
// MCP results are text content blocks, so the map is rendered as markdown
// rather than pretty JSON: the same information for ~40-50% fewer tokens

fn jstr(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn jnum(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(|x| x.as_i64()).unwrap_or(0)
}

fn jbool(v: &Value, k: &str) -> bool {
    v.get(k).and_then(|x| x.as_bool()).unwrap_or(false)
}

fn jarr(v: &Value, k: &str) -> Vec<Value> {
    v.get(k)
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default()
}

// comma-joined string array (`symbols`, `ambiguous_symbols`, ...)
fn jnames(v: &Value, k: &str) -> String {
    jarr(v, k)
        .iter()
        .filter_map(|x| x.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

// a section body, or `(none)` when empty
fn md_section(out: &mut String, title: &str, body: &str) {
    out.push_str(&format!("\n## {title}\n"));
    out.push_str(if body.is_empty() { "(none)\n" } else { body });
}

// one map hit - covers every kind `find` and `references` emit (func, const,
// note, call, use), printing only the fields that kind carries
fn md_hit(r: &Value) -> String {
    let mut line = format!("{}:{}", jstr(r, "file"), jnum(r, "line"));
    if let Some(col) = r.get("col").and_then(|x| x.as_i64()) {
        line.push_str(&format!(":{col}"));
    }
    line.push_str(&format!(" {}", jstr(r, "kind")));
    let name = jstr(r, "name");
    if !name.is_empty() {
        line.push_str(&format!(" {name}"));
    }
    if let Some(ty) = r.get("type").and_then(|x| x.as_str()) {
        line.push_str(&format!(": {ty}"));
    }
    if let Some(ret) = r.get("ret").and_then(|x| x.as_str()) {
        line.push_str(&format!(" -> {ret}"));
    }
    if let Some(span) = r.get("span").and_then(|x| x.as_array()) {
        if let [a, b] = &span[..] {
            line.push_str(&format!(" span {a}-{b}"));
        }
    }
    if let Some(q) = r.get("qualifier").and_then(|x| x.as_str()) {
        line.push_str(&format!(" qualifier={q}"));
    }
    let caller = jstr(r, "caller");
    if !caller.is_empty() {
        line.push_str(&format!(" in {caller}"));
    }
    if jbool(r, "test_ctx") {
        line.push_str(" (test)");
    }
    let text = jstr(r, "text");
    if !text.is_empty() {
        line.push_str(&format!(" {text}"));
    }
    if let Some(doc) = r.get("doc").and_then(|x| x.as_str()) {
        line.push_str(&format!(" - {doc}"));
    }
    line.push('\n');
    line
}

fn md_index(v: &Value) -> String {
    let t = v.get("totals").cloned().unwrap_or_default();
    let mut out = format!(
        "# {} - {} files (generated {})\n{} funcs, {} consts, {} refs, {} notes\n\n\
         | file | lang | funcs | consts | refs | notes |\n|---|---|---|---|---|---|\n",
        jstr(v, "root"),
        jnum(&t, "files"),
        jstr(v, "generated"),
        jnum(&t, "funcs"),
        jnum(&t, "consts"),
        jnum(&t, "refs"),
        jnum(&t, "notes"),
    );
    for f in jarr(v, "files") {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            jstr(&f, "path"),
            jstr(&f, "language"),
            jnum(&f, "funcs"),
            jnum(&f, "consts"),
            jnum(&f, "refs"),
            jnum(&f, "notes"),
        ));
    }
    out
}

fn md_dependencies(v: &Value) -> String {
    let edges = |key: &str| -> String {
        jarr(v, key)
            .iter()
            .map(|e| {
                format!(
                    "{} -> {}: {}\n",
                    jstr(e, "from"),
                    jstr(e, "to"),
                    jnames(e, "symbols")
                )
            })
            .collect()
    };
    // per-file shape carries `file`; the whole-project shape carries `edges`
    if v.get("file").is_some() {
        let mut out = format!("# dependencies for {}\n", jstr(v, "file"));
        md_section(&mut out, "depends on", &edges("depends_on"));
        md_section(&mut out, "depended on by", &edges("depended_on_by"));
        return out;
    }
    let mut out = format!(
        "# dependencies - {} files, {} edges\n\n{}",
        jnum(v, "files"),
        jarr(v, "edges").len(),
        edges("edges"),
    );
    let ambiguous = jnames(v, "ambiguous_symbols");
    if !ambiguous.is_empty() {
        out.push_str(&format!(
            "\nambiguous (defined in more than one file, no evidence to pick one): {ambiguous}\n"
        ));
    }
    let excluded = jnames(v, "excluded_symbols");
    if !excluded.is_empty() {
        out.push_str(&format!(
            "\nexcluded (name matches a definition elsewhere, but the call site \
             neither qualifies it nor imports it): {excluded}\n"
        ));
    }
    out
}

fn md_find(v: &Value) -> String {
    let shown = jarr(v, "results");
    let mut out = format!(
        "# find \"{}\" (kind {}) - {} result(s){}\n\n",
        jstr(v, "query"),
        jstr(v, "kind"),
        jnum(v, "count"),
        if jbool(v, "truncated") {
            format!(", showing {}", shown.len())
        } else {
            String::new()
        },
    );
    for r in &shown {
        out.push_str(&md_hit(r));
    }
    out
}

fn md_references(v: &Value) -> String {
    let counts = v.get("counts").cloned().unwrap_or_default();
    let mut out = format!(
        "# references {} - {} definition(s), {} reference(s){}\n",
        jstr(v, "symbol"),
        jnum(&counts, "definitions"),
        jnum(&counts, "references"),
        if jbool(v, "truncated") {
            format!(", showing {}", jarr(v, "references").len())
        } else {
            String::new()
        },
    );
    let hits = |key: &str| -> String { jarr(v, key).iter().map(md_hit).collect() };
    md_section(&mut out, "definitions", &hits("definitions"));
    md_section(&mut out, "references", &hits("references"));
    out
}

fn md_notes(v: &Value) -> String {
    let marker = jstr(v, "marker");
    let mut out = format!(
        "# notes{} - {}\n\n",
        if marker.is_empty() {
            String::new()
        } else {
            format!(" ({marker})")
        },
        jnum(v, "count"),
    );
    for x in jarr(v, "notes") {
        out.push_str(&format!(
            "{}:{} {}\n",
            jstr(&x, "file"),
            jnum(&x, "line"),
            jstr(&x, "text")
        ));
    }
    out
}

// the structured half of a `file` result: spans and the intra-file call graph,
// without the rendered markdown that restates them
fn md_file_structured(v: &Value) -> String {
    let mut out = format!(
        "# {} ({})\ncache entry: {}\n",
        jstr(v, "path"),
        jstr(v, "language"),
        jstr(v, "cache_name"),
    );
    let consts: String = jarr(v, "consts")
        .iter()
        .map(|k| {
            format!(
                "{} {}{}\n",
                jnum(k, "line"),
                jstr(k, "name"),
                k.get("type")
                    .and_then(|t| t.as_str())
                    .map(|t| format!(": {t}"))
                    .unwrap_or_default(),
            )
        })
        .collect();
    let funcs: String = jarr(v, "funcs")
        .iter()
        .map(|f| {
            let mut l = format!("{}:{} {}", jnum(f, "line"), jnum(f, "col"), jstr(f, "name"));
            if let Some(ret) = f.get("ret").and_then(|x| x.as_str()) {
                l.push_str(&format!(" -> {ret}"));
            }
            if let Some(span) = f.get("span").and_then(|x| x.as_array()) {
                if let [a, b] = &span[..] {
                    l.push_str(&format!(" span {a}-{b}"));
                }
            }
            if let Some(doc) = f.get("doc").and_then(|x| x.as_str()) {
                l.push_str(&format!(" - {doc}"));
            }
            l.push('\n');
            l
        })
        .collect();
    let refs: String = jarr(v, "refs")
        .iter()
        .map(|r| {
            format!(
                "{} {} -> {}:{}\n",
                jnum(r, "call_line"),
                jstr(r, "caller"),
                jstr(r, "target"),
                jnum(r, "target_line"),
            )
        })
        .collect();
    let notes: String = jarr(v, "notes")
        .iter()
        .map(|n| format!("{} {}\n", jnum(n, "line"), jstr(n, "text")))
        .collect();
    md_section(&mut out, "consts (line name: type)", &consts);
    md_section(&mut out, "funcs (line:col name -> ret)", &funcs);
    md_section(&mut out, "calls (line caller -> target:line)", &refs);
    md_section(&mut out, "notes (line text)", &notes);
    out
}

// The analysis sections are large - `test_targets` alone runs to six figures of
// JSON on a medium repo. Handing an agent a silently truncated list is the
// failure mode to avoid: it reads as "that was everything". So every
// list-shaped tool takes the same window and always says what it left out.
#[derive(Clone, Copy)]
struct Page {
    offset: usize,
    limit: usize,
}

impl Page {
    fn from(args: &Value, default_limit: usize) -> Page {
        let n = |k: &str, d: usize| {
            args.get(k)
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(d)
        };
        Page {
            offset: n("offset", 0),
            limit: n("limit", default_limit).clamp(1, 500),
        }
    }

    // the window, plus the line that accounts for everything outside it
    fn apply<'a>(&self, items: &'a [Value]) -> (&'a [Value], String) {
        let total = items.len();
        let start = self.offset.min(total);
        let end = (start + self.limit).min(total);
        let window = &items[start..end];
        let note = if total == 0 {
            String::new()
        } else if start == 0 && end == total {
            format!("({total} total)\n")
        } else {
            let more = if end < total {
                format!(", pass offset={end} for the next page")
            } else {
                String::new()
            };
            format!("(showing {}-{end} of {total}{more})\n", start + 1)
        };
        (window, note)
    }
}

// `file:line`, the form every other tool here emits
fn at(v: &Value) -> String {
    format!("{}:{}", jstr(v, "file"), jnum(v, "line"))
}

// With no `.ccc/map.json`, `changes` names the implicit whole-root service
// `.`, which on its own tells a reader nothing.
fn svc(name: &str) -> &str {
    if name == "." {
        "whole project"
    } else {
        name
    }
}

fn svc_names(v: &Value, k: &str) -> String {
    jarr(v, k)
        .iter()
        .filter_map(|x| x.as_str())
        .map(svc)
        .collect::<Vec<_>>()
        .join(", ")
}

// Evidence, not an index: a helper named by forty tests would otherwise fill
// the result with names that add nothing after the first few.
fn jnames_capped(v: &Value, k: &str, cap: usize) -> String {
    let all = jarr(v, k);
    let shown: Vec<&str> = all.iter().filter_map(|x| x.as_str()).take(cap).collect();
    let mut s = shown.join(", ");
    if all.len() > cap {
        s.push_str(&format!(" (+{} more)", all.len() - cap));
    }
    s
}

// The change set is unavailable outside a git repo, on a shallow clone, or with
// no base ref. Say which, rather than render an empty list that reads as
// "nothing changed".
fn md_unavailable(what: &str, v: &Value) -> Option<String> {
    if jbool(v, "available") || v.get("available").is_none() {
        return None;
    }
    Some(format!(
        "# {what}\nunavailable: {}\n\n{}\n",
        jstr(v, "reason"),
        jstr(v, "hint")
    ))
}

fn md_changes(v: &Value, page: Page) -> String {
    if let Some(why) = md_unavailable("changes", v) {
        return why;
    }
    let c = v.get("counts").cloned().unwrap_or(json!({}));
    let short = |k: &str| jstr(v, k).chars().take(9).collect::<String>();
    let mut out = format!(
        "# changes\nbase {} ({}..{}) - {} service(s)\n{} file(s), {} function(s) changed, {} untested\nservices to test: {}\n",
        jstr(v, "base"),
        short("base_sha"),
        short("head_sha"),
        jarr(v, "services").len(),
        jnum(&c, "changed_files"),
        jnum(&c, "changed_functions"),
        jarr(v, "untested").len(),
        {
            let s = svc_names(v, "services_to_test");
            if s.is_empty() { "(none)".into() } else { s }
        },
    );

    let funcs = jarr(v, "changed_functions");
    let (window, note) = page.apply(&funcs);
    let body: String = window
        .iter()
        .map(|f| {
            let lines = jarr(f, "lines");
            let span = match &lines[..] {
                [a, b] => format!("{a}-{b}"),
                _ => jnum(f, "line").to_string(),
            };
            let tested = jarr(f, "tested_by");
            format!(
                "{}:{} {} [{}] {}\n",
                jstr(f, "file"),
                span,
                jstr(f, "function"),
                svc_names(f, "services"),
                if tested.is_empty() {
                    "UNTESTED".to_string()
                } else {
                    format!("tested_by: {}", jnames_capped(f, "tested_by", 5))
                },
            )
        })
        .collect();
    md_section(&mut out, &format!("changed functions {note}"), &body);

    let edges: String = jarr(v, "edges")
        .iter()
        .map(|e| {
            let mut tags = Vec::new();
            if jbool(e, "declared") {
                tags.push("declared");
            }
            if jbool(e, "detected") {
                tags.push("detected");
            }
            format!(
                "{} -> {} [{}] {}\n",
                svc(&jstr(e, "from")),
                svc(&jstr(e, "to")),
                tags.join("+"),
                jarr(e, "symbols")
                    .iter()
                    .filter_map(|s| s.get("symbol").and_then(|x| x.as_str()))
                    .take(8)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect();
    md_section(&mut out, "service edges", &edges);

    // calls the resolver refused to attribute: the honest counterpart to the
    // edges above, and the first place to look when an edge is missing
    let unresolved: String = jarr(v, "unresolved_calls")
        .iter()
        .take(40)
        .map(|u| {
            format!(
                "{} {} at {} - candidates: {}\n",
                svc(&jstr(u, "from")),
                jstr(u, "symbol"),
                at(u),
                {
                    let c = svc_names(u, "candidates");
                    if c.is_empty() { "none".into() } else { c }
                }
            )
        })
        .collect();
    md_section(&mut out, "unresolved calls", &unresolved);
    out
}

fn md_triggers(v: &Value, targets: &Value, page: Page) -> String {
    if let Some(why) = md_unavailable("test triggers", v) {
        return why;
    }
    let c = v.get("counts").cloned().unwrap_or(json!({}));
    let mut out = format!(
        "# test triggers\nbase {} - {} of {} test(s) trigger, {} directly, {} gap(s)\n{} function(s) changed in {} file(s); {} uncommitted\n",
        jstr(v, "base"),
        jnum(&c, "tests_to_run"),
        jnum(v, "total_tests"),
        jnum(&c, "direct"),
        jnum(&c, "gaps"),
        jnum(&c, "changed_functions"),
        jnum(&c, "changed_files"),
        jnum(&c, "uncommitted_files"),
    );
    if jbool(v, "full_suite_advised") {
        out.push_str(
            "the trigger set covers most of the suite - run everything, a long name filter is slower and more fragile\n",
        );
    }

    // A name filter selecting 80 tests runs to kilobytes, and pasting it is
    // worse than running the suite. Never truncate it - a half-copied command
    // silently selects the wrong tests - so past the budget, describe it and
    // point at the endpoint that serves it whole.
    const MAX_COMMAND: usize = 1200;
    let cmds: String = jarr(v, "commands")
        .iter()
        .map(|x| {
            let cmd = jstr(x, "command");
            if cmd.len() > MAX_COMMAND {
                format!(
                    "({}: a filter naming {} tests, {} chars - omitted)\n  \
                     Run the whole suite instead; the exact command is in /insights.json \
                     under test_triggers.commands.\n",
                    jstr(x, "language"),
                    jnum(x, "selects"),
                    cmd.len(),
                )
            } else {
                format!(
                    "$ {cmd}\n  ({}, selects {}) {}\n",
                    jstr(x, "language"),
                    jnum(x, "selects"),
                    jstr(x, "caveat")
                )
            }
        })
        .collect();
    md_section(&mut out, "commands", &cmds);

    let run = jarr(v, "run");
    let (window, note) = page.apply(&run);
    let body: String = window
        .iter()
        .map(|r| {
            let d = jnum(r, "distance");
            format!(
                "{:<7} {} {} - {}\n",
                if d == 0 {
                    "direct".to_string()
                } else {
                    format!("{d} hop{}", if d > 1 { "s" } else { "" })
                },
                at(r),
                jstr(r, "test"),
                jstr(r, "reason")
            )
        })
        .collect();
    md_section(&mut out, &format!("run {note}"), &body);

    // Each gap is a `target` id into `test_targets`; the recommendation lives
    // there and is looked up rather than restated.
    let rows = jarr(targets, "targets");
    let by_id: BTreeMap<String, &Value> = rows.iter().map(|t| (jstr(t, "id"), t)).collect();
    let all_gaps = jarr(v, "add");
    // gaps get the same window as the run list - on a large branch there are
    // more of them than tests, and an unpaged list would dwarf everything above
    let (gap_window, gap_note) = page.apply(&all_gaps);
    let gaps: String = gap_window
        .iter()
        .map(|a| {
            let id = jstr(a, "target");
            match by_id.get(&id) {
                Some(t) => format!(
                    "[{}] {} {} - {}\n",
                    jstr(t, "kind"),
                    at(t),
                    jstr(t, "function"),
                    jstr(t, "suggest")
                ),
                None => format!("{id} - nothing covers it\n"),
            }
        })
        .collect();
    md_section(&mut out, &format!("missing coverage {gap_note}"), &gaps);
    out.push_str(&format!("\n{}\n", jstr(v, "note")));
    out
}

fn md_targets(v: &Value, kind: Option<&str>, page: Page) -> String {
    let s = v.get("summary").cloned().unwrap_or(json!({}));
    let by_kind = s
        .get("by_kind")
        .and_then(|b| b.as_object())
        .map(|b| {
            b.iter()
                .map(|(k, n)| format!("{k} {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let mut out = format!(
        "# test targets\n{} function(s) ranked, {} with no test naming them\nby kind: {by_kind}\n",
        jnum(&s, "functions"),
        jnum(&s, "untested"),
    );

    let all = jarr(v, "targets");
    let rows: Vec<Value> = match kind {
        Some(k) => all
            .iter()
            .filter(|t| jstr(t, "kind") == k || jarr(t, "also").iter().any(|a| a == k))
            .cloned()
            .collect(),
        None => all,
    };
    let (window, note) = page.apply(&rows);
    let body: String = window
        .iter()
        .map(|t| {
            let why = jarr(t, "why")
                .iter()
                .map(|w| jstr(w, "detail"))
                .collect::<Vec<_>>()
                .join("; ");
            let mut l = format!(
                "[{}] {} {} ({}) priority {}{}\n  {}\n",
                jstr(t, "kind"),
                at(t),
                jstr(t, "function"),
                svc(&jstr(t, "service")),
                jnum(t, "priority"),
                if jbool(t, "covered") { "" } else { " UNTESTED" },
                jstr(t, "suggest"),
            );
            if !why.is_empty() {
                l.push_str(&format!("  because: {why}\n"));
            }
            for sem in jarr(t, "semantics").iter().filter_map(|x| x.as_str()) {
                l.push_str(&format!("  - {sem}\n"));
            }
            l
        })
        .collect();
    md_section(&mut out, &format!("targets {note}"), &body);

    // the rubric is what makes a `kind` actionable rather than a label
    let rubric: String = jarr(v, "rubric")
        .iter()
        .map(|r| {
            format!(
                "{} - {} (chosen when {})\n",
                jstr(r, "kind"),
                jstr(r, "for"),
                jstr(r, "chosen_when")
            )
        })
        .collect();
    md_section(&mut out, "kinds", &rubric);
    out.push_str(&format!("\n{}\n", jstr(v, "note")));
    out
}

fn md_lints(v: &Value, rule: Option<&str>, page: Page) -> Result<String, String> {
    let catalogue = jarr(v, "rules");
    // A misspelled rule that quietly returns nothing reads as "clean". Refuse
    // it and name the rules that exist.
    if let Some(r) = rule {
        if !catalogue.iter().any(|c| jstr(c, "rule") == r) {
            return Err(format!(
                "unknown rule '{r}'; expected one of {}",
                catalogue
                    .iter()
                    .map(|c| jstr(c, "rule"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    let all = jarr(v, "findings");
    let rows: Vec<Value> = match rule {
        Some(r) => all.iter().filter(|f| jstr(f, "rule") == r).cloned().collect(),
        None => all,
    };
    let mut out = format!("# lints\n{} finding(s)\n", rows.len());
    if jbool(v, "truncated") {
        out.push_str("the finding list hit its cap - narrow it with `rule`\n");
    }
    let (window, note) = page.apply(&rows);
    let body: String = window
        .iter()
        .map(|f| {
            format!(
                "[{}] {} {} in `{}` - {}\n  {}\n",
                jstr(f, "severity"),
                jstr(f, "rule"),
                at(f),
                jstr(f, "function"),
                jstr(f, "message"),
                jstr(f, "hint"),
            )
        })
        .collect();
    md_section(&mut out, &format!("findings {note}"), &body);

    // Every rule ships its own limits. They are the difference between a
    // finding worth acting on and one worth ignoring, so they travel with it -
    // but only for the rules actually in play.
    let rules: String = catalogue
        .iter()
        .filter(|r| rule.is_none() || rule == Some(jstr(r, "rule").as_str()))
        .map(|r| {
            format!(
                "{} ({}) - {}\n  evidence: {}\n  limits: {}\n",
                jstr(r, "rule"),
                jstr(r, "severity"),
                jstr(r, "what"),
                jstr(r, "evidence"),
                jstr(r, "limits"),
            )
        })
        .collect();
    md_section(&mut out, "rules", &rules);
    out.push_str(&format!("\n{}\n", jstr(v, "note")));
    Ok(out)
}

const HOT_VIEWS: &[&str] = &[
    "most_called",
    "widest",
    "most_complex",
    "deepest_chains",
    "cycles",
];

fn md_hot(v: &Value, view: Option<&str>, page: Page) -> String {
    let mut out = String::from("# hot paths\n");
    let wanted: Vec<&str> = match view {
        Some(x) => vec![x],
        None => HOT_VIEWS.to_vec(),
    };
    for name in wanted {
        let rows = jarr(v, name);
        let (window, note) = page.apply(&rows);
        let body: String = window
            .iter()
            .map(|r| match name {
                "deepest_chains" => format!(
                    "depth {} ({} call sites): {}\n",
                    jnum(r, "depth"),
                    jnum(r, "call_sites"),
                    jarr(r, "chain")
                        .iter()
                        .map(|c| jstr(c, "name"))
                        .collect::<Vec<_>>()
                        .join(" -> ")
                ),
                "cycles" => format!(
                    "{}\n",
                    jarr(r, "chain")
                        .iter()
                        .map(|c| jstr(c, "name"))
                        .collect::<Vec<_>>()
                        .join(" -> ")
                ),
                _ => format!(
                    "{} {} callers={} sites={} calls={} cx={} loops={} lines={}{}\n",
                    at(r),
                    jstr(r, "name"),
                    jnum(r, "callers"),
                    jnum(r, "call_sites"),
                    jnum(r, "calls"),
                    jnum(r, "complexity"),
                    jnum(r, "loop_depth"),
                    jnum(r, "lines"),
                    if jbool(r, "recursive") { " recursive" } else { "" },
                ),
            })
            .collect();
        md_section(&mut out, &format!("{} {note}", name.replace('_', " ")), &body);
    }
    out.push_str(&format!("\n{}\n", jstr(v, "note")));
    out
}

fn md_services(v: &Value, only: Option<&str>, page: Page) -> String {
    let mut out = format!("# services\nsource: {}\n", jstr(v, "source"));
    let svcs = jarr(v, "services");
    let body: String = svcs
        .iter()
        .filter(|s| only.is_none() || only == Some(jstr(s, "name").as_str()))
        .map(|s| {
            format!(
                "{} - {} file(s), {} function(s) [{}]\n",
                svc(&jstr(s, "name")),
                jnum(s, "files"),
                jnum(s, "funcs"),
                jnames(s, "globs"),
            )
        })
        .collect();
    md_section(&mut out, &format!("services ({})", svcs.len()), &body);

    let edges: Vec<Value> = jarr(v, "edges")
        .into_iter()
        .filter(|e| {
            only.is_none()
                || only == Some(jstr(e, "from").as_str())
                || only == Some(jstr(e, "to").as_str())
        })
        .collect();
    let (window, note) = page.apply(&edges);
    let body: String = window
        .iter()
        .map(|e| {
            let mut tags = Vec::new();
            if jbool(e, "declared") {
                tags.push("declared");
            }
            if jbool(e, "detected") {
                tags.push("detected");
            }
            if jbool(e, "declared") && !jbool(e, "detected") {
                tags.push("no calls found");
            }
            let mut l = format!(
                "{} -> {} [{}] {} call site(s)\n",
                svc(&jstr(e, "from")),
                svc(&jstr(e, "to")),
                tags.join(", "),
                jnum(e, "count"),
            );
            // the call sites are the drill-down: what actually carries the hop
            for s in jarr(e, "sites").iter().take(6) {
                l.push_str(&format!(
                    "  {}:{} {} -> {} ({}:{})\n",
                    jstr(s, "caller_file"),
                    jnum(s, "caller_line"),
                    jstr(s, "caller"),
                    jstr(s, "symbol"),
                    jstr(s, "target_file"),
                    jnum(s, "target_line"),
                ));
            }
            l
        })
        .collect();
    md_section(&mut out, &format!("edges {note}"), &body);

    let unassigned = jnames(v, "unassigned_files");
    if !unassigned.is_empty() && only.is_none() {
        md_section(&mut out, "unassigned files", &format!("{unassigned}\n"));
    }
    out
}

fn mcp_tool_call(state: &RwLock<MapState>, params: &Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or((-32602, "missing tool name".to_string()))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let arg = |k: &str| args.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());

    if name == "refresh" {
        let mut map = state.write().expect("map lock poisoned");
        return match map.rescan() {
            Ok((before, after)) => Ok(mcp_md(
                &format!(
                    "rescanned: {before} -> {after} files (generated {})",
                    map.ts
                ),
                false,
            )),
            Err(e) => Ok(mcp_md(&format!("error: {e}"), true)),
        };
    }

    let map = state.read().expect("map lock poisoned");
    // `file` returns the rendered markdown by default; `structured: true` swaps
    // it for spans plus the intra-file call graph. One representation per call -
    // returning both duplicated the same content at roughly 3x the tokens.
    let structured = args
        .get("structured")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let out: Result<String, String> = match name {
        "index" => Ok(md_index(&q_index(&map))),
        "find" => q_find(
            &map,
            &arg("query").unwrap_or_default(),
            arg("kind").as_deref().unwrap_or("any"),
        )
        .map(|v| md_find(&v)),
        "references" => q_references(&map, &arg("symbol").unwrap_or_default()).map(|v| md_references(&v)),
        "dependencies" => q_dependencies(&map, arg("file").as_deref()).map(|v| md_dependencies(&v)),
        "file" => q_file(&map, &arg("path").unwrap_or_default()).map(|v| {
            if structured {
                md_file_structured(&v)
            } else {
                jstr(&v, "markdown")
            }
        }),
        "notes" => Ok(md_notes(&q_notes(&map, arg("marker").as_deref()))),
        "changes" => {
            let a = map.analysis(arg("base").as_deref());
            Ok(md_changes(&a["changes"], Page::from(&args, 40)))
        }
        "test_triggers" => {
            let a = map.analysis(arg("base").as_deref());
            Ok(md_triggers(
                &a["test_triggers"],
                &a["test_targets"],
                Page::from(&args, 25),
            ))
        }
        "test_targets" => {
            let a = map.analysis(None);
            Ok(md_targets(
                &a["test_targets"],
                arg("kind").as_deref(),
                Page::from(&args, 15),
            ))
        }
        "lints" => {
            let a = map.analysis(None);
            md_lints(&a["lints"], arg("rule").as_deref(), Page::from(&args, 40))
        }
        "hot" => {
            let view = arg("view");
            match view.as_deref() {
                Some(v) if !HOT_VIEWS.contains(&v) => Err(format!(
                    "unknown view '{v}'; expected one of {}",
                    HOT_VIEWS.join(", ")
                )),
                _ => {
                    let a = map.analysis(None);
                    Ok(md_hot(&a["hot"], view.as_deref(), Page::from(&args, 15)))
                }
            }
        }
        "services" => {
            let a = map.analysis(None);
            Ok(md_services(
                &a["services"],
                arg("service").as_deref(),
                Page::from(&args, 25),
            ))
        }
        _ => return Err((-32602, format!("unknown tool '{name}'"))),
    };
    Ok(match out {
        Ok(text) => mcp_md(&text, false),
        Err(e) => mcp_md(&format!("error: {e}"), true),
    })
}

fn mcp_resources_list(state: &RwLock<MapState>) -> Value {
    let map = state.read().expect("map lock poisoned");
    let mut resources = vec![json!({
        "uri": "ccc://index",
        "name": "CCC.md",
        "description": "ContextCodeCache index for the whole project",
        "mimeType": "text/markdown",
    })];
    for c in &map.caches {
        resources.push(json!({
            "uri": format!("ccc://entry/{}", c.cache_name),
            "name": c.cache_name,
            "description": format!("map entry for {}", map.path_of(c)),
            "mimeType": "text/markdown",
        }));
    }
    json!({"resources": resources})
}

fn mcp_resources_read(state: &RwLock<MapState>, params: &Value) -> Result<Value, (i64, String)> {
    let uri = params
        .get("uri")
        .and_then(|u| u.as_str())
        .ok_or((-32602, "missing uri".to_string()))?;
    let map = state.read().expect("map lock poisoned");
    let text = if uri == "ccc://index" {
        render::render_index(&map.root, &map.caches, &map.ts)
    } else if let Some(name) = uri.strip_prefix("ccc://entry/") {
        let c = map
            .caches
            .iter()
            .find(|c| c.cache_name == name)
            .ok_or((-32002, format!("resource not found: {uri}")))?;
        render::render_file(c, &map.ts)
    } else {
        return Err((-32002, format!("resource not found: {uri}")));
    };
    Ok(json!({"contents": [{"uri": uri, "mimeType": "text/markdown", "text": text}]}))
}

fn mcp_handle(state: &RwLock<MapState>, msg: &Value) -> Option<Value> {
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = msg.get("id").cloned();
    if method.is_empty() || id.is_none() || id == Some(Value::Null) {
        return None; // notification (or a response we never asked for)
    }
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(mcp_initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(mcp_tools()),
        "tools/call" => mcp_tool_call(state, &params),
        "resources/list" => Ok(mcp_resources_list(state)),
        "resources/read" => mcp_resources_read(state, &params),
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    })
}

// percent-decoder for query components (`%2F`, `+` as space)
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(url: &str) -> (String, BTreeMap<String, String>) {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let mut params = BTreeMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(url_decode(k), url_decode(v));
    }
    (path.to_string(), params)
}

// only loopback origins - plus "null", the Origin a browser sends for pages
// opened from file:// (the generated `ccc changes --html` report)
fn origin_ok(origin: Option<&str>) -> bool {
    let Some(origin) = origin else { return true };
    if origin == "null" {
        return true;
    }
    let host = origin
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let host = host.split([':', '/']).next().unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

enum ReplyBody {
    Json(Value),
    Html(String),
    Empty,
}

struct Reply {
    status: u16,
    body: ReplyBody,
}

fn ok(body: Value) -> Reply {
    Reply {
        status: 200,
        body: ReplyBody::Json(body),
    }
}

fn bad(status: u16, msg: impl Into<String>) -> Reply {
    Reply {
        status,
        body: ReplyBody::Json(json!({"error": msg.into()})),
    }
}

// fragment endpoints always answer 200 with self-describing HTML (soft
// errors styled inline) so HTMX swaps them without error-handling config
fn html_ok(html: String) -> Reply {
    Reply {
        status: 200,
        body: ReplyBody::Html(html),
    }
}

// tiny Tailwind-styled snippets consumed by the `ccc changes --html` report's
// live-query panel; same q_* data, HTML instead of JSON

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn frag_err(msg: &str) -> String {
    format!(r#"<p class="text-amber-400 text-xs">{}</p>"#, esc(msg))
}

fn frag_health(map: &MapState) -> String {
    format!(
        r#"<span class="text-emerald-400">●</span> <span class="text-slate-400">{} files · {}</span>"#,
        map.caches.len(),
        esc(&map.ts),
    )
}

// `file:line` code location chip
fn frag_loc(file: &str, line: u64) -> String {
    format!(
        r#"<code class="px-1 py-0.5 rounded bg-slate-800 text-slate-300 text-xs">{}:{}</code>"#,
        esc(file),
        line
    )
}

fn frag_find(v: &Value) -> String {
    let results = v["results"].as_array().cloned().unwrap_or_default();
    if results.is_empty() {
        return frag_err(&format!("no matches for \"{}\"", v["query"].as_str().unwrap_or("")));
    }
    let rows: String = results
        .iter()
        .map(|r| {
            let name = r["name"].as_str().or(r["text"].as_str()).unwrap_or("");
            let ret = r["ret"].as_str().map(|t| format!(":{t}")).unwrap_or_default();
            let doc = r["doc"].as_str().unwrap_or("");
            format!(
                r#"<div class="flex flex-wrap items-baseline gap-2 py-0.5">{}<span class="text-xs text-slate-500">{}</span><span class="font-mono text-slate-200 text-xs">{}{}</span><span class="text-xs text-slate-500 truncate">{}</span></div>"#,
                frag_loc(r["file"].as_str().unwrap_or(""), r["line"].as_u64().unwrap_or(0)),
                esc(r["kind"].as_str().unwrap_or("")),
                esc(name),
                esc(&ret),
                esc(doc),
            )
        })
        .collect();
    format!(
        r#"<p class="text-xs text-slate-500 mb-1">{} match(es){}</p><div class="max-h-64 overflow-y-auto">{}</div>"#,
        v["count"].as_u64().unwrap_or(0),
        if v["truncated"].as_bool().unwrap_or(false) { " (truncated)" } else { "" },
        rows
    )
}

fn frag_references(v: &Value) -> String {
    let defs = v["definitions"].as_array().cloned().unwrap_or_default();
    let refs = v["references"].as_array().cloned().unwrap_or_default();
    let def_rows: String = defs
        .iter()
        .map(|d| {
            format!(
                r#"<div class="py-0.5">defined {} <span class="text-xs text-slate-500">{}</span></div>"#,
                frag_loc(d["file"].as_str().unwrap_or(""), d["line"].as_u64().unwrap_or(0)),
                esc(d["doc"].as_str().unwrap_or("")),
            )
        })
        .collect();
    let ref_rows: String = refs
        .iter()
        .map(|r| {
            let test = if r["test_ctx"].as_bool().unwrap_or(false) {
                r#" <span class="text-emerald-400 text-xs">test</span>"#
            } else {
                ""
            };
            format!(
                r#"<div class="py-0.5">{} <span class="font-mono text-xs text-slate-400">{}</span>{}</div>"#,
                frag_loc(r["file"].as_str().unwrap_or(""), r["line"].as_u64().unwrap_or(0)),
                esc(r["caller"].as_str().unwrap_or("")),
                test,
            )
        })
        .collect();
    format!(
        r#"<p class="text-xs text-slate-500 mb-1">{} definition(s), {} reference(s)</p><div class="max-h-64 overflow-y-auto">{}{}</div>"#,
        defs.len(),
        v["counts"]["references"].as_u64().unwrap_or(0),
        def_rows,
        ref_rows
    )
}

fn frag_dependencies(v: &Value) -> String {
    let list = |edges: &[Value], arrow: &str| -> String {
        edges
            .iter()
            .map(|e| {
                let other = e[arrow].as_str().unwrap_or("");
                let symbols: String = e["symbols"]
                    .as_array()
                    .map(|s| {
                        s.iter()
                            .filter_map(|x| x.as_str())
                            .map(|x| format!(r#"<code class="px-1 rounded bg-slate-800 text-xs">{}</code>"#, esc(x)))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                format!(
                    r#"<div class="py-0.5 font-mono text-xs text-slate-300">{} {}</div>"#,
                    esc(other),
                    symbols
                )
            })
            .collect()
    };
    if let Some(file) = v["file"].as_str() {
        let on = v["depends_on"].as_array().cloned().unwrap_or_default();
        let by = v["depended_on_by"].as_array().cloned().unwrap_or_default();
        format!(
            r#"<p class="text-xs text-slate-500 mb-1">{}</p><div class="max-h-64 overflow-y-auto"><div class="text-xs text-slate-400 mt-1">depends on ({})</div>{}<div class="text-xs text-slate-400 mt-2">depended on by ({})</div>{}</div>"#,
            esc(file),
            on.len(),
            list(&on, "to"),
            by.len(),
            list(&by, "from"),
        )
    } else {
        let edges = v["edges"].as_array().cloned().unwrap_or_default();
        if edges.is_empty() {
            return frag_err("no cross-file edges resolved");
        }
        let rows: String = edges
            .iter()
            .map(|e| {
                format!(
                    r#"<div class="py-0.5 font-mono text-xs text-slate-300">{} <span class="text-slate-600">→</span> {}</div>"#,
                    esc(e["from"].as_str().unwrap_or("")),
                    esc(e["to"].as_str().unwrap_or("")),
                )
            })
            .collect();
        format!(
            r#"<p class="text-xs text-slate-500 mb-1">{} edge(s)</p><div class="max-h-64 overflow-y-auto">{}</div>"#,
            edges.len(),
            rows
        )
    }
}

const ENDPOINTS: &[&str] = &[
    "GET /index",
    "GET /find?q=<substring>[&kind=func|const|note]",
    "GET /references?symbol=<name>",
    "GET /dependencies[?file=<path>]",
    "GET /file?path=<path>",
    "GET /notes[?marker=TODO]",
    "GET /health",
    "GET /insights.json[?base=<ref>] (the whole analysis payload)",
    "GET /insights (human UI over the same data; needs --html)",
    "POST /refresh",
    "POST /mcp (Model Context Protocol, JSON-RPC)",
    "GET /fragment/{find,references,dependencies,health} (HTML for HTMX)",
];

fn route(state: &RwLock<MapState>, method: &str, url: &str, body: &[u8]) -> Reply {
    let (path, params) = parse_query(url);
    let get = |k: &str| params.get(k).map(|s| s.as_str());

    match (method, path.as_str()) {
        ("GET", "/") | ("GET", "/index") => {
            let map = state.read().expect("map lock poisoned");
            ok(q_index(&map))
        }
        ("GET", "/health") => {
            let map = state.read().expect("map lock poisoned");
            ok(json!({
                "ok": true,
                "files": map.caches.len(),
                "generated": map.ts,
                "watch_secs": map.watch_secs,
                "version": env!("CARGO_PKG_VERSION"),
            }))
        }
        ("GET", "/find") => {
            let Some(q) = get("q").or_else(|| get("query")) else {
                return bad(400, "missing ?q=<substring>");
            };
            let map = state.read().expect("map lock poisoned");
            match q_find(&map, q, get("kind").unwrap_or("any")) {
                Ok(v) => ok(v),
                Err(e) => bad(400, e),
            }
        }
        ("GET", "/references") => {
            let Some(symbol) = get("symbol") else {
                return bad(400, "missing ?symbol=<name>");
            };
            let map = state.read().expect("map lock poisoned");
            match q_references(&map, symbol) {
                Ok(v) => ok(v),
                Err(e) => bad(404, e),
            }
        }
        ("GET", "/dependencies") => {
            let map = state.read().expect("map lock poisoned");
            match q_dependencies(&map, get("file")) {
                Ok(v) => ok(v),
                Err(e) => bad(404, e),
            }
        }
        ("GET", "/file") => {
            let Some(p) = get("path") else {
                return bad(400, "missing ?path=<relative path>");
            };
            let map = state.read().expect("map lock poisoned");
            match q_file(&map, p) {
                Ok(v) => ok(v),
                Err(e) => bad(404, e),
            }
        }
        ("GET", "/notes") => {
            let map = state.read().expect("map lock poisoned");
            ok(q_notes(&map, get("marker")))
        }
        ("GET", "/insights.json") => {
            let map = state.read().expect("map lock poisoned");
            ok((*map.analysis(get("base"))).clone())
        }
        // human-facing insights UI; off unless `ccc serve --html`
        ("GET", "/insights") => {
            let map = state.read().expect("map lock poisoned");
            if !map.html {
                return bad(
                    404,
                    "insights UI is disabled; restart with `ccc serve --html` \
                     (the data is at /insights.json either way)",
                );
            }
            html_ok(crate::html::render_insights_html(&map.root_label, None))
        }
        // HTML fragments for the HTMX live-query panel (always 200, errors inline)
        ("GET", "/fragment/health") => {
            let map = state.read().expect("map lock poisoned");
            html_ok(frag_health(&map))
        }
        ("GET", "/fragment/find") => {
            let map = state.read().expect("map lock poisoned");
            let q = get("q").or_else(|| get("query")).unwrap_or("");
            html_ok(match q_find(&map, q, get("kind").unwrap_or("any")) {
                Ok(v) => frag_find(&v),
                Err(e) => frag_err(&e),
            })
        }
        ("GET", "/fragment/references") => {
            let map = state.read().expect("map lock poisoned");
            html_ok(match q_references(&map, get("symbol").unwrap_or("")) {
                Ok(v) => frag_references(&v),
                Err(e) => frag_err(&e),
            })
        }
        ("GET", "/fragment/dependencies") => {
            let map = state.read().expect("map lock poisoned");
            let file = get("file").filter(|f| !f.trim().is_empty());
            html_ok(match q_dependencies(&map, file) {
                Ok(v) => frag_dependencies(&v),
                Err(e) => frag_err(&e),
            })
        }
        // CORS preflight (the file:// report page sends these before requests)
        ("OPTIONS", _) => Reply {
            status: 204,
            body: ReplyBody::Empty,
        },
        ("POST", "/refresh") => {
            let mut map = state.write().expect("map lock poisoned");
            match map.rescan() {
                Ok((before, after)) => ok(json!({
                    "files_before": before,
                    "files_after": after,
                    "generated": map.ts,
                })),
                Err(e) => bad(500, format!("rescan failed: {e:#}")),
            }
        }
        ("POST", "/mcp") => {
            let msg: Value = match serde_json::from_slice(body) {
                Ok(v) => v,
                Err(e) => {
                    return Reply {
                        status: 400,
                        body: ReplyBody::Json(json!({
                            "jsonrpc": "2.0", "id": null,
                            "error": {"code": -32700, "message": format!("parse error: {e}")},
                        })),
                    }
                }
            };
            match mcp_handle(state, &msg) {
                Some(resp) => ok(resp),
                // notification: acknowledged, no body
                None => Reply {
                    status: 202,
                    body: ReplyBody::Empty,
                },
            }
        }
        ("GET", "/mcp") | ("DELETE", "/mcp") => bad(405, "POST JSON-RPC messages to /mcp"),
        _ => Reply {
            status: 404,
            body: ReplyBody::Json(
                json!({"error": format!("no route {method} {path}"), "endpoints": ENDPOINTS}),
            ),
        },
    }
}

// start the server and block
pub fn serve(root: &Path, opts: &ServeOptions) -> Result<()> {
    let state = Arc::new(RwLock::new(MapState::build(root)?));
    {
        let mut map = state.write().expect("map lock poisoned");
        map.watch_secs = opts.watch.map(|d| d.as_secs());
        map.html = opts.html;
        if map.caches.is_empty() {
            eprintln!("warning: no supported source files under {}", root.display());
        }
    }

    let bind = format!("{}:{}", opts.addr, opts.port);
    let server = tiny_http::Server::http(&bind)
        .map_err(|e| anyhow::anyhow!("binding {bind}: {e}"))?;
    let addr = server.server_addr();
    {
        let map = state.read().expect("map lock poisoned");
        println!(
            "ccc serve: {} files mapped from {}",
            map.caches.len(),
            root.display()
        );
    }
    println!("listening on http://{addr}  (MCP endpoint: http://{addr}/mcp)");
    println!("endpoints: {}", ENDPOINTS.join(" | "));
    if opts.html {
        println!("insights UI: http://{addr}/insights");
    }
    match opts.watch {
        Some(interval) => {
            println!("watching for changes every {}s", interval.as_secs().max(1));
            spawn_watcher(Arc::clone(&state), root.to_path_buf(), interval);
        }
        None => println!("watching disabled - POST /refresh after editing source"),
    }

    let server = Arc::new(server);
    let mut workers = Vec::new();
    for _ in 0..4 {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);
        workers.push(std::thread::spawn(move || loop {
            let request = match server.recv() {
                Ok(r) => r,
                Err(_) => return,
            };
            handle_request(&state, request);
        }));
    }
    for w in workers {
        let _ = w.join();
    }
    Ok(())
}

fn handle_request(state: &RwLock<MapState>, mut request: tiny_http::Request) {
    let header_val = |name: &'static str| {
        request
            .headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str().to_string())
    };
    let origin = header_val("Origin");
    let cors_headers = header_val("Access-Control-Request-Headers");
    let method = request.method().as_str().to_string();
    let url = request.url().to_string();

    let allowed = origin_ok(origin.as_deref());
    let reply = if !allowed {
        bad(403, "cross-origin requests are not allowed")
    } else {
        let mut body = Vec::new();
        if request.as_reader().read_to_end(&mut body).is_err() {
            bad(400, "could not read request body")
        } else {
            route(state, &method, &url, &body)
        }
    };

    let header = |k: &str, v: &str| {
        tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("valid header")
    };
    let mut response = match reply.body {
        ReplyBody::Empty => tiny_http::Response::empty(reply.status).boxed(),
        ReplyBody::Json(v) => {
            let data = serde_json::to_vec(&v).unwrap_or_else(|_| b"{}".to_vec());
            tiny_http::Response::from_data(data)
                .with_status_code(reply.status)
                .with_header(header("Content-Type", "application/json"))
                .boxed()
        }
        ReplyBody::Html(s) => tiny_http::Response::from_data(s.into_bytes())
            .with_status_code(reply.status)
            .with_header(header("Content-Type", "text/html; charset=utf-8"))
            .boxed(),
    };
    // CORS: echo an allowed origin (incl. "null" for file:// report pages) so
    // the browser-side HTMX panel can read responses; foreign origins got 403
    // above and no allow header, so they stay blocked
    if allowed {
        if let Some(o) = &origin {
            response = response
                .with_header(header("Access-Control-Allow-Origin", o))
                .with_header(header("Vary", "Origin"))
                .with_header(header("Access-Control-Allow-Methods", "GET, POST, OPTIONS"));
            let allow = cors_headers.as_deref().unwrap_or("Content-Type");
            response = response.with_header(header("Access-Control-Allow-Headers", allow));
        }
    }
    let _ = request.respond(response);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> MapState {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ccc-serve-test-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("api")).unwrap();
        fs::create_dir_all(dir.join("lib")).unwrap();
        fs::write(
            dir.join("lib/money.rs"),
            "pub const MAX: u64 = 9;\n\
             pub enum Mode { Fast, Slow }\n\
             // Charge an amount.\npub fn charge(c: u64) -> u64 { c }\n\
             // TODO: support currencies\npub fn refund(c: u64) -> u64 { c }\n",
        )
        .unwrap();
        fs::write(
            dir.join("api/main.rs"),
            "fn handle() -> u64 { money::charge(1) + helper() }\nfn helper() -> u64 { 2 }\n\
             fn cap() -> u64 { money::MAX }\n\
             fn pick() -> u64 { let _m = money::Mode::Fast; 0 }\n",
        )
        .unwrap();
        let state = MapState::build(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        state
    }

    #[test]
    fn index_and_find() {
        let map = fixture();
        let idx = q_index(&map);
        assert_eq!(idx["totals"]["files"], 2);
        let found = q_find(&map, "char", "any").unwrap();
        assert_eq!(found["count"], 1);
        assert_eq!(found["results"][0]["name"], "charge");
        assert_eq!(found["results"][0]["kind"], "func");
        let none = q_find(&map, "charge", "const").unwrap();
        assert_eq!(none["count"], 0);
        assert!(q_find(&map, "  ", "any").is_err());
    }

    #[test]
    fn find_qualified_queries_match_call_sites() {
        let map = fixture();
        // qualified query resolves against call sites, dot or colons
        for query in ["money::charge", "money.charge"] {
            let found = q_find(&map, query, "any").unwrap();
            assert_eq!(found["count"], 1, "query {query}");
            assert_eq!(found["results"][0]["kind"], "call");
            assert_eq!(found["results"][0]["file"], "api/main.rs");
            assert_eq!(found["results"][0]["qualifier"], "money");
        }
        // wrong qualifier matches nothing
        assert_eq!(q_find(&map, "bogus::charge", "any").unwrap()["count"], 0);
        // kind=call searches call sites without a qualifier
        let calls = q_find(&map, "charge", "call").unwrap();
        assert_eq!(calls["count"], 1);
        assert_eq!(calls["results"][0]["caller"], "handle");
        // unqualified `any` still returns only the definition, not the call
        assert_eq!(q_find(&map, "charge", "any").unwrap()["count"], 1);
    }

    #[test]
    fn references_finds_defs_and_calls() {
        let map = fixture();
        let refs = q_references(&map, "charge").unwrap();
        assert_eq!(refs["counts"]["definitions"], 1);
        assert_eq!(refs["counts"]["references"], 1);
        assert_eq!(refs["references"][0]["file"], "api/main.rs");
        assert_eq!(refs["references"][0]["qualifier"], "money");
        assert!(q_references(&map, "nowhere").is_err());
    }

    #[test]
    fn references_accepts_qualified_names() {
        let map = fixture();
        let refs = q_references(&map, "money::charge").unwrap();
        assert_eq!(refs["name"], "charge");
        assert_eq!(refs["qualifier"], "money");
        assert_eq!(refs["counts"]["definitions"], 1);
        assert_eq!(refs["counts"]["references"], 1);
        assert_eq!(refs["definitions"][0]["file"], "lib/money.rs");
        // qualifier that names no file: call sites filter to zero, but the
        // bare-name definitions are still reported
        let miss = q_references(&map, "bogus::charge").unwrap();
        assert_eq!(miss["counts"]["references"], 0);
        assert_eq!(miss["counts"]["definitions"], 1);
        // dot-form qualifier works the same as colons
        assert_eq!(
            q_references(&map, "money.charge").unwrap()["counts"]["references"],
            1
        );
    }

    #[test]
    fn references_and_find_cover_qualified_usages() {
        let map = fixture();
        // `money::MAX` in api/main.rs is a usage, not a call
        let refs = q_references(&map, "money::MAX").unwrap();
        assert_eq!(refs["counts"]["references"], 1);
        assert_eq!(refs["references"][0]["kind"], "use");
        assert_eq!(refs["references"][0]["file"], "api/main.rs");
        assert_eq!(refs["references"][0]["caller"], "cap");
        // the const declaration is its definition, narrowed by the qualifier
        assert_eq!(refs["counts"]["definitions"], 1);
        assert_eq!(refs["definitions"][0]["kind"], "const");
        assert_eq!(refs["definitions"][0]["file"], "lib/money.rs");
        // unqualified `find` surfaces constant-like usages alongside the const
        let found = q_find(&map, "MAX", "any").unwrap();
        let kinds: Vec<_> = found["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["kind"].as_str().unwrap().to_string())
            .collect();
        assert!(kinds.contains(&"const".to_string()));
        assert!(kinds.contains(&"use".to_string()));
    }

    #[test]
    fn enum_variants_pair_declaration_with_usages() {
        let map = fixture();
        // `Mode::Fast`: declaration in lib/money.rs, usage in api/main.rs
        let refs = q_references(&map, "Mode::Fast").unwrap();
        assert_eq!(refs["counts"]["definitions"], 1);
        assert_eq!(refs["definitions"][0]["kind"], "const");
        assert_eq!(refs["definitions"][0]["file"], "lib/money.rs");
        assert_eq!(refs["definitions"][0]["type"], "Mode");
        assert_eq!(refs["counts"]["references"], 1);
        assert_eq!(refs["references"][0]["kind"], "use");
        assert_eq!(refs["references"][0]["file"], "api/main.rs");
        assert_eq!(refs["references"][0]["caller"], "pick");
        // the deep qualifier is preserved on the usage
        assert_eq!(refs["references"][0]["qualifier"], "money::Mode");
    }

    #[test]
    fn mcp_results_are_markdown_not_json() {
        let state = RwLock::new(fixture());
        let call = |name: &str, args: Value| -> String {
            let v = mcp_handle(
                &state,
                &json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                        "params": {"name": name, "arguments": args}}),
            )
            .unwrap();
            assert_eq!(v["result"]["isError"], false);
            v["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };

        let idx = call("index", json!({}));
        assert!(idx.starts_with('#'), "{idx}");
        assert!(!idx.contains("\"path\":"), "still JSON: {idx}");
        assert!(idx.contains("| lib/money.rs | rust |"), "{idx}");

        let deps = call("dependencies", json!({}));
        assert!(deps.contains("api/main.rs -> lib/money.rs: charge"), "{deps}");

        let refs = call("references", json!({"symbol": "charge"}));
        assert!(refs.contains("## definitions"), "{refs}");
        assert!(refs.contains("lib/money.rs:4"), "{refs}");

        // markdown renders the same facts for a fraction of the JSON cost (the
        // table header is fixed overhead, so the margin widens with file count)
        let as_json = serde_json::to_string_pretty(&q_index(&fixture())).unwrap();
        assert!(
            idx.len() * 3 < as_json.len() * 2,
            "markdown {} vs json {}",
            idx.len(),
            as_json.len()
        );
    }

    #[test]
    fn file_tool_returns_one_representation_per_call() {
        let state = RwLock::new(fixture());
        let call = |args: Value| -> String {
            let v = mcp_handle(
                &state,
                &json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                        "params": {"name": "file", "arguments": args}}),
            )
            .unwrap();
            v["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        };

        // default: the rendered .ccc markdown, nothing else
        let md = call(json!({"path": "lib/money.rs"}));
        assert!(md.contains("# source: lib/money.rs [rust]"), "{md}");
        assert!(!md.contains("## funcs (line:col"), "{md}");

        // structured: spans and the call graph, without restating the markdown
        let st = call(json!({"path": "lib/money.rs", "structured": true}));
        assert!(st.contains("## funcs (line:col name -> ret)"), "{st}");
        assert!(st.contains("span"), "{st}");
        assert!(!st.contains("# source:"), "{st}");

        // neither ships both halves the way the old JSON result did
        let both = serde_json::to_string_pretty(&q_file(&fixture(), "lib/money.rs").unwrap())
            .unwrap()
            .len();
        assert!(md.len() < both / 2, "markdown {} vs json {both}", md.len());
        assert!(st.len() < both / 2, "structured {} vs json {both}", st.len());
    }

    #[test]
    fn dependencies_edges_and_per_file() {
        let map = fixture();
        let all = q_dependencies(&map, None).unwrap();
        let edges = all["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["from"], "api/main.rs");
        assert_eq!(edges[0]["to"], "lib/money.rs");
        let per = q_dependencies(&map, Some("lib/money.rs")).unwrap();
        assert_eq!(per["depended_on_by"].as_array().unwrap().len(), 1);
        assert!(per["depends_on"].as_array().unwrap().is_empty());
    }

    #[test]
    fn dependencies_require_evidence_and_cover_type_only_imports() {
        let dir = std::env::temp_dir().join(format!("ccc-deps-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("lib")).unwrap();
        fs::create_dir_all(dir.join("api")).unwrap();
        fs::write(
            dir.join("lib/util.rs"),
            "pub enum Mode { A }\n\
             pub fn shrink(x: u64) -> u64 { x }\n\
             pub fn truncate(s: &str) -> &str { s }\n",
        )
        .unwrap();
        // `shrink(1)` is evidenced by the import; `v.truncate(2)` is a stdlib
        // method that merely shares a name with a util fn - no edge for it.
        // `use util::Mode` alone must still produce a (type-only) edge.
        fs::write(
            dir.join("api/main.rs"),
            "use util::shrink;\n\
             use util::Mode;\n\
             fn a() -> u64 { shrink(1) }\n\
             fn b(mut v: Vec<u64>) { v.truncate(2); }\n",
        )
        .unwrap();
        let map = MapState::build(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);

        let all = q_dependencies(&map, None).unwrap();
        let edges = all["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["from"], "api/main.rs");
        assert_eq!(edges[0]["to"], "lib/util.rs");
        let symbols: Vec<&str> = edges[0]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert!(symbols.contains(&"shrink"));
        assert!(symbols.contains(&"Mode"));
        assert!(!symbols.contains(&"truncate"));
        let excluded: Vec<&str> = all["excluded_symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap())
            .collect();
        assert_eq!(excluded, vec!["truncate"]);
    }

    #[test]
    fn dependencies_resolve_through_facade_reexports() {
        let dir = std::env::temp_dir().join(format!("ccc-facade-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("pkg")).unwrap();
        fs::write(dir.join("pkg/impl.py"), "def work():\n    return 1\n").unwrap();
        fs::write(dir.join("pkg/__init__.py"), "from pkg.impl import work\n").unwrap();
        // `pkg.work()` names the facade (pkg/__init__.py), which re-exports
        // `work` from pkg/impl.py - the call must resolve through the hop
        fs::write(
            dir.join("app.py"),
            "import pkg\n\ndef go():\n    return pkg.work()\n",
        )
        .unwrap();
        let map = MapState::build(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);

        let all = q_dependencies(&map, None).unwrap();
        let has_edge = |from: &str, to: &str, sym: &str| {
            all["edges"].as_array().unwrap().iter().any(|e| {
                e["from"] == from
                    && e["to"] == to
                    && e["symbols"].as_array().unwrap().iter().any(|s| s == sym)
            })
        };
        assert!(has_edge("app.py", "pkg/__init__.py", "pkg"));
        assert!(has_edge("pkg/__init__.py", "pkg/impl.py", "work"));
        assert!(has_edge("app.py", "pkg/impl.py", "work"));
    }

    #[test]
    fn file_lookup_and_suffix() {
        let map = fixture();
        let f = q_file(&map, "lib/money.rs").unwrap();
        assert_eq!(f["funcs"].as_array().unwrap().len(), 2);
        assert!(f["markdown"].as_str().unwrap().contains("# const"));
        // unique suffix works, junk errors with suggestions
        assert!(q_file(&map, "money.rs").is_ok());
        assert!(q_file(&map, "nope.rs").is_err());
    }

    #[test]
    fn notes_filtering() {
        let map = fixture();
        assert_eq!(q_notes(&map, None)["count"], 1);
        assert_eq!(q_notes(&map, Some("TODO"))["count"], 1);
        assert_eq!(q_notes(&map, Some("FIXME"))["count"], 0);
    }

    #[test]
    fn watcher_detects_edits_adds_and_deletes() {
        let dir = std::env::temp_dir().join(format!("ccc-watch-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.rs"), "fn one() {}\n").unwrap();

        let fp0 = fingerprint(&dir).unwrap();
        assert!(check_and_rebuild(&dir, &fp0).unwrap().is_none());

        // appended function shows up in the fresh map
        fs::write(dir.join("a.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        let (fp1, caches, delta) = check_and_rebuild(&dir, &fp0).unwrap().expect("edit detected");
        assert_eq!(delta, 1);
        assert!(caches[0].funcs.iter().any(|f| f.name == "two"));

        // add a file
        fs::write(dir.join("b.rs"), "fn three() {}\n").unwrap();
        let (fp2, caches, _) = check_and_rebuild(&dir, &fp1).unwrap().expect("add detected");
        assert_eq!(caches.len(), 2);

        // delete it again q
        fs::remove_file(dir.join("b.rs")).unwrap();
        let (_, caches, delta) = check_and_rebuild(&dir, &fp2).unwrap().expect("delete detected");
        assert_eq!(caches.len(), 1);
        assert_eq!(delta, 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mcp_initialize_negotiates_version() {
        let known = mcp_initialize(&json!({"protocolVersion": "2024-11-05"}));
        assert_eq!(known["protocolVersion"], "2024-11-05");
        let unknown = mcp_initialize(&json!({"protocolVersion": "1999-01-01"}));
        assert_eq!(unknown["protocolVersion"], MCP_LATEST);
        assert_eq!(known["serverInfo"]["name"], "ccc");
    }

    #[test]
    fn mcp_lifecycle_and_tools() {
        let state = RwLock::new(fixture());
        assert!(mcp_handle(
            &state,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        )
        .is_none());
        // ping
        let pong = mcp_handle(&state, &json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
            .unwrap();
        assert_eq!(pong["result"], json!({}));
        let list = mcp_handle(
            &state,
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .unwrap();
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                // the code map
                "index",
                "find",
                "references",
                "dependencies",
                "file",
                "notes",
                "refresh",
                // views onto the one analysis pass
                "changes",
                "test_triggers",
                "test_targets",
                "lints",
                "hot",
                "services",
            ]
        );
        // tools/call find
        let call = mcp_handle(
            &state,
            &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": {"name": "find", "arguments": {"query": "charge"}}}),
        )
        .unwrap();
        assert_eq!(call["result"]["isError"], false);
        assert!(call["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("lib/money.rs"));
        // soft error: unknown symbol is isError, not a protocol error
        let miss = mcp_handle(
            &state,
            &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                    "params": {"name": "references", "arguments": {"symbol": "ghost"}}}),
        )
        .unwrap();
        assert_eq!(miss["result"]["isError"], true);
        // unknown method -> -32601
        let nope = mcp_handle(
            &state,
            &json!({"jsonrpc": "2.0", "id": 5, "method": "prompts/list"}),
        )
        .unwrap();
        assert_eq!(nope["error"]["code"], -32601);
    }

    #[test]
    fn mcp_resources_roundtrip() {
        let state = RwLock::new(fixture());
        let list = mcp_resources_list(&state);
        let uris: Vec<&str> = list["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uri"].as_str().unwrap())
            .collect();
        assert!(uris.contains(&"ccc://index"));
        assert!(uris.iter().any(|u| u.ends_with("lib-money.rs.md")));
        let read =
            mcp_resources_read(&state, &json!({"uri": "ccc://entry/lib-money.rs.md"})).unwrap();
        assert!(read["contents"][0]["text"].as_str().unwrap().contains("charge"));
        assert!(mcp_resources_read(&state, &json!({"uri": "ccc://entry/ghost.md"})).is_err());
    }

    fn json_of(r: &Reply) -> &Value {
        match &r.body {
            ReplyBody::Json(v) => v,
            _ => panic!("expected a JSON body"),
        }
    }

    fn html_of(r: &Reply) -> &str {
        match &r.body {
            ReplyBody::Html(s) => s,
            _ => panic!("expected an HTML body"),
        }
    }

    #[test]
    fn http_routing_shapes() {
        let state = RwLock::new(fixture());
        let r = route(&state, "GET", "/find?q=charge", b"");
        assert_eq!(r.status, 200);
        assert_eq!(json_of(&r)["count"], 1);
        assert_eq!(route(&state, "GET", "/find", b"").status, 400);
        assert_eq!(route(&state, "GET", "/references?symbol=ghost", b"").status, 404);
        assert_eq!(route(&state, "GET", "/nope", b"").status, 404);
        assert_eq!(route(&state, "GET", "/mcp", b"").status, 405);
        // MCP notification over HTTP -> 202
        let n = route(
            &state,
            "POST",
            "/mcp",
            br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        );
        assert_eq!(n.status, 202);
        assert!(matches!(n.body, ReplyBody::Empty));
        // URL-encoded queryy decodes
        let enc = route(&state, "GET", "/file?path=lib%2Fmoney.rs", b"");
        assert_eq!(enc.status, 200);
        // CORS preflight
        let pre = route(&state, "OPTIONS", "/fragment/find", b"");
        assert_eq!(pre.status, 204);
    }

    // Call every analysis tool the way an agent would, and check the two
    // things that make one usable: it renders as markdown rather than raw
    // JSON, and a list it cannot fit says how to get the rest.
    #[test]
    fn analysis_tools_render_markdown_and_page_rather_than_truncate() {
        let state = RwLock::new(fixture());
        let call = |name: &str, args: Value| -> (bool, String) {
            let v = mcp_handle(
                &state,
                &json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                        "params": {"name": name, "arguments": args}}),
            )
            .unwrap();
            (
                v["result"]["isError"].as_bool().unwrap_or(false),
                v["result"]["content"][0]["text"].as_str().unwrap().to_string(),
            )
        };

        // The fixture has no git repo, so the two git-relative tools must
        // explain themselves instead of rendering an empty list.
        for name in ["changes", "test_triggers"] {
            let (err, out) = call(name, json!({}));
            assert!(!err, "{name}: {out}");
            assert!(out.contains("unavailable"), "{name}: {out}");
            assert!(out.contains("fetch-depth"), "{name} must say how to fix it: {out}");
        }

        // The other four work off the map alone.
        for name in ["test_targets", "lints", "hot", "services"] {
            let (err, out) = call(name, json!({}));
            assert!(!err, "{name}: {out}");
            assert!(out.starts_with('#'), "{name} is not markdown: {out}");
            assert!(!out.contains("\":"), "{name} leaked raw JSON: {out}");
        }

        // Paging: a window smaller than the list says how to reach the rest,
        // and `offset` actually moves it.
        let (_, first) = call("test_targets", json!({"limit": 1}));
        assert!(first.contains("showing 1-1 of"), "{first}");
        assert!(first.contains("pass offset=1"), "{first}");
        let (_, second) = call("test_targets", json!({"limit": 1, "offset": 1}));
        assert!(second.contains("showing 2-2 of"), "{second}");
        assert_ne!(
            first, second,
            "offset returned the same window - paging is not wired up"
        );

        // A filter that matches nothing must not read as "nothing to report".
        let (err, out) = call("lints", json!({"rule": "no-such-rule"}));
        assert!(err, "an unknown rule silently returned findings: {out}");
        assert!(out.contains("expected one of"), "{out}");
        let (err, out) = call("hot", json!({"view": "no-such-view"}));
        assert!(err, "an unknown view silently returned rows: {out}");
        assert!(out.contains("expected one of"), "{out}");

        // A rule that exists but has no hits is a real, empty answer.
        let (err, out) = call("lints", json!({"rule": "leak-risk"}));
        assert!(!err, "{out}");
        assert!(out.contains("leak-risk"), "the rule's limits travel with it: {out}");
    }

    // Six tools over one analysis pass: computing it per call would repeat the
    // whole graph build each time.
    #[test]
    fn the_analysis_is_computed_once_per_generation_and_base() {
        let map = fixture();
        let first = map.analysis(None);
        let again = map.analysis(None);
        assert!(Arc::ptr_eq(&first, &again), "the analysis was recomputed");

        // a different base is a different question
        let other = map.analysis(Some("HEAD~1"));
        assert!(!Arc::ptr_eq(&first, &other));

        // and a rescan must not serve the previous map's analysis
        let mut map = map;
        map.ts = "later".into();
        map.invalidate();
        assert!(!Arc::ptr_eq(&first, &map.analysis(None)));
    }

    #[test]
    fn insights_ui_is_opt_in() {
        let state = RwLock::new(fixture());
        assert_eq!(route(&state, "GET", "/insights", b"").status, 404);
        let off = route(&state, "GET", "/insights", b"");
        assert!(json_of(&off)["error"].as_str().unwrap().contains("--html"));
        assert_eq!(route(&state, "GET", "/insights.json", b"").status, 200);

        state.write().unwrap().html = true;
        let page = route(&state, "GET", "/insights", b"");
        assert_eq!(page.status, 200);
        let ReplyBody::Html(body) = &page.body else {
            panic!("the UI must be served as HTML");
        };
        assert!(body.contains("ccc insights"));
        assert!(body.contains("/insights.json"));

        let data = route(&state, "GET", "/insights.json", b"");
        assert_eq!(data.status, 200);
        let v = json_of(&data);
        assert_eq!(v["schema"], crate::insights::SCHEMA);
        // the fixture's cross-file call resolves, so the graph is not empty
        assert!(v["totals"]["functions"].as_u64().unwrap() >= 5);
        assert!(v["totals"]["edges"].as_u64().unwrap() >= 1);
        // every tab the page renders has a section to render from
        for key in ["flame", "hot", "services", "lints", "languages"] {
            assert!(!v[key].is_null(), "insights payload missing {key}");
        }
        // findings never ship without the caveat the UI prints beside them
        assert!(v["lints"]["note"].as_str().unwrap().contains("heuristics"));
        // the page reports how long the analysis took, beside when it ran
        assert!(v["took_ns"].as_u64().unwrap() > 0);
        assert!(!v["generated"].as_str().unwrap().is_empty());
        // the flame view is grouped, so per-service trees have somewhere to go
        assert!(!v["flame"]["groups"].as_array().unwrap().is_empty());
    }

    #[test]
    fn html_fragments_for_htmx() {
        let state = RwLock::new(fixture());
        // hits are Tailwind-classed HTML with file:line locations
        let hit = route(&state, "GET", "/fragment/find?q=charge", b"");
        assert_eq!(hit.status, 200);
        let html = html_of(&hit);
        assert!(html.contains("lib/money.rs"));
        assert!(html.contains("match(es)"));
        // soft errors are 200 with inline styling, so HTMX always swaps
        let miss = route(&state, "GET", "/fragment/references?symbol=ghost", b"");
        assert_eq!(miss.status, 200);
        assert!(html_of(&miss).contains("not found"));
        // dependencies: whole graph and per-file
        let graph = route(&state, "GET", "/fragment/dependencies", b"");
        assert!(html_of(&graph).contains("edge(s)"));
        let one = route(&state, "GET", "/fragment/dependencies?file=lib/money.rs", b"");
        assert!(html_of(&one).contains("depended on by"));
        // health chip
        let health = route(&state, "GET", "/fragment/health", b"");
        assert!(html_of(&health).contains("files"));
        // fragment content is escaped (searching for markup finds nothing raw)
        let esc_probe = route(&state, "GET", "/fragment/find?q=%3Cscript%3E", b"");
        assert!(!html_of(&esc_probe).contains("<script>"));
    }

    #[test]
    fn origin_gate() {
        assert!(origin_ok(None));
        assert!(origin_ok(Some("http://localhost:3000")));
        assert!(origin_ok(Some("http://127.0.0.1")));
        // file:// pages (the generated changes HTML report) send Origin: null
        assert!(origin_ok(Some("null")));
        assert!(!origin_ok(Some("https://evil.example.com")));
        assert!(!origin_ok(Some("http://nullable.example.com")));
    }
}
