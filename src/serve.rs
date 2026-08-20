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
    externals: Vec<ExternalDep>,
    facade: Option<String>,
    watch_secs: Option<u64>,
    html: bool,
    origin: String,
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
            externals: manifest_deps(root),
            facade: cargo_package_name(root),
            watch_secs: None,
            html: false,
            // `--port 0` picks one at runtime
            origin: {
                let d = ServeOptions::default();
                format!("http://{}:{}", d.addr, d.port)
            },
            analysis: Mutex::new(None),
        })
    }

    fn rescan(&mut self) -> Result<(usize, usize)> {
        let before = self.caches.len();
        let files = scan::collect_files(&self.root)?;
        self.caches = scan::build_caches(&self.root, &files);
        self.externals = manifest_deps(&self.root);
        self.facade = cargo_package_name(&self.root);
        self.ts = render::now_ts();
        self.invalidate();
        Ok((before, self.caches.len()))
    }

    // swap in a fresh map (built outside lock by watcher)
    fn swap_in(&mut self, caches: Vec<FileCache>) {
        self.caches = caches;
        self.externals = manifest_deps(&self.root);
        self.facade = cargo_package_name(&self.root);
        self.ts = render::now_ts();
        self.invalidate();
    }

    fn external_named(&self, want: &str) -> Option<&ExternalDep> {
        let norm = |s: &str| s.replace('-', "_").to_ascii_lowercase();
        let want = norm(want);
        self.externals.iter().find(|d| norm(&d.name) == want)
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

// `prefix` narrows the overview to one subtree; totals then describe the
// filtered set, with the project-wide file count kept alongside so a filtered
// answer can never be mistaken for the whole project
fn q_index(map: &MapState, prefix: Option<&str>) -> Value {
    let want = prefix
        .map(|p| {
            p.trim()
                .trim_start_matches("./")
                .trim_matches('/')
                .to_string()
        })
        .filter(|p| !p.is_empty());
    let mut totals = Counts::default();
    let mut files: Vec<Value> = Vec::new();
    for c in &map.caches {
        let path = map.path_of(c);
        if let Some(w) = &want {
            if path != *w && !path.starts_with(&format!("{w}/")) {
                continue;
            }
        }
        let n = c.counts();
        totals.add(n);
        files.push(json!({
            "path": path,
            "language": c.language.as_str(),
            "funcs": n.funcs,
            "consts": n.consts,
            "refs": n.refs,
            "notes": n.notes,
            "mods": n.mods,
            "exports": n.reexports,
        }));
    }
    let mut out = json!({
        "root": map.root_label,
        "generated": map.ts,
        "totals": {
            "files": files.len(),
            "funcs": totals.funcs,
            "consts": totals.consts,
            "refs": totals.refs,
            "notes": totals.notes,
            "mods": totals.mods,
            "exports": totals.reexports,
        },
        "project_files": map.caches.len(),
        "files": files,
    });
    if let Some(w) = want {
        // a filter that matches nothing is an answer, not an error - say which
        // top-level directories the map does hold
        if out["files"].as_array().is_some_and(|a| a.is_empty()) {
            let mut tops: BTreeSet<String> = BTreeSet::new();
            for c in &map.caches {
                let p = map.path_of(c);
                tops.insert(match p.split_once('/') {
                    Some((d, _)) => format!("{d}/"),
                    None => p,
                });
            }
            out["available"] = json!(tops.into_iter().collect::<Vec<_>>());
        }
        out["filter"] = json!(w);
    }
    out
}

// A query ending in a separator (`clap::`, `client.`) names a qualifier with no
// symbol after it: list everything under that qualifier rather than looking for
// a symbol literally called "clap::".
fn split_query(query: &str) -> (Option<&str>, &str) {
    let q = query.trim();
    for sep in ["::", "."] {
        if let Some(prefix) = q.strip_suffix(sep) {
            let prefix = prefix.trim();
            return if prefix.is_empty() {
                (None, "")
            } else {
                (Some(prefix), "")
            };
        }
    }
    split_qualified(q)
}

fn q_find(map: &MapState, query: &str, kind: &str) -> Result<Value, String> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return Err("empty query".into());
    }
    if !matches!(
        kind,
        "any" | "func" | "const" | "note" | "call" | "type" | "import"
    ) {
        return Err(format!(
            "kind '{kind}' not one of any|func|const|note|call|type|import"
        ));
    }
    let (qualifier_raw, want_name) = split_query(query);
    let want_qualifier = qualifier_raw.map(|s| s.to_ascii_lowercase());
    let want_name = want_name.to_ascii_lowercase();

    let qualifier_only = want_name.is_empty() && want_qualifier.is_some();
    if want_name.is_empty() && want_qualifier.is_none() {
        return Err(format!(
            "query '{query}' names no symbol; use `mod::` to list a qualifier"
        ));
    }

    // qualified queries (`serde_json::to_string`) resolve against call sites;
    // under `any` unqualified calls stay out so definitions aren't drowned.
    // uses (enum variants, consts) are constant-like and rare, so they are
    // searched under `any` even unqualified
    let search_calls = kind == "call" || (kind == "any" && want_qualifier.is_some());
    let search_uses = matches!(kind, "any" | "call");
    // a qualifier-only query is about where a module is used, so definitions
    // (which carry no qualifier) cannot answer it
    let search_defs = !qualifier_only;
    let mut results = Vec::new();
    for c in &map.caches {
        let path = map.path_of(c);
        if search_defs && matches!(kind, "any" | "func") {
            for f in &c.funcs {
                if f.name.to_ascii_lowercase().contains(&q) {
                    results.push(json!({
                        "kind": "func", "file": path, "line": f.line, "col": f.col,
                        "name": f.name, "ret": f.ret, "doc": f.comment,
                        "owner": f.owner,
                        "span": [f.start_line, f.end_line],
                    }));
                }
            }
        }
        if search_defs && matches!(kind, "any" | "const") {
            for k in &c.consts {
                if k.name.to_ascii_lowercase().contains(&q) {
                    results.push(json!({
                        "kind": "const", "file": path, "line": k.line,
                        "name": k.name, "type": k.ty,
                    }));
                }
            }
        }
        // type definitions: structs, enums, traits, classes, interfaces,
        // aliases. Extracted all along, searchable only since they were wired
        // in here - a struct used purely through its type was invisible.
        if search_defs && matches!(kind, "any" | "type") {
            for t in &c.types {
                if t.name.to_ascii_lowercase().contains(&q) {
                    results.push(json!({
                        "kind": "type", "file": path, "line": t.line,
                        "name": t.name, "type": t.kind,
                    }));
                }
            }
        }
        if search_defs && matches!(kind, "any" | "note") {
            for n in &c.notes {
                if n.text.to_ascii_lowercase().contains(&q) {
                    results.push(json!({
                        "kind": "note", "file": path, "line": n.line, "text": n.text,
                    }));
                }
            }
        }
        // import statements
        if matches!(kind, "any" | "import") {
            for imp in &c.imports {
                let module_ok = match (&want_qualifier, qualifier_only) {
                    (Some(w), true) => qualifier_under(Some(&imp.module), w),
                    (Some(w), false) => {
                        qualifier_matches(Some(&imp.module.to_ascii_lowercase()), w)
                    }
                    (None, _) => imp.module.to_ascii_lowercase().contains(&q),
                };
                let bound: Vec<&String> = imp
                    .names
                    .iter()
                    .filter(|n| {
                        want_name.is_empty() || n.to_ascii_lowercase().contains(&want_name)
                    })
                    .collect();
                let hit = if qualifier_only {
                    module_ok
                } else if want_qualifier.is_some() {
                    module_ok && !bound.is_empty()
                } else {
                    module_ok || !bound.is_empty()
                };
                if hit {
                    results.push(json!({
                        "kind": "import", "file": path, "line": imp.line,
                        "name": bound.first().map(|n| n.as_str()).unwrap_or(&imp.module),
                        "module": imp.module, "names": imp.names,
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
            let name_ok = qualifier_only || site.name.to_ascii_lowercase().contains(&want_name);
            let qualifier_ok = match (&want_qualifier, qualifier_only) {
                (None, _) => true,
                (Some(w), true) => qualifier_under(site.qualifier.as_deref(), w),
                (Some(w), false) => site
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
    // report what was actually looked at
    let mut searched: Vec<&str> = Vec::new();
    for (name, on) in [
        ("func", search_defs && matches!(kind, "any" | "func")),
        ("const", search_defs && matches!(kind, "any" | "const")),
        ("type", search_defs && matches!(kind, "any" | "type")),
        ("note", search_defs && matches!(kind, "any" | "note")),
        ("import", matches!(kind, "any" | "import")),
        ("call", search_calls),
        ("use", search_uses),
    ] {
        if on {
            searched.push(name);
        }
    }
    let total = results.len();
    results.truncate(FIND_CAP);
    let mut out = json!({
        "query": query,
        "kind": kind,
        "qualifier": qualifier_raw,
        "count": total,
        "truncated": total > FIND_CAP,
        "searched": searched,
        "results": results,
    });
    if total == 0 {
        out["miss"] = json!(true);
        let probe = if want_name.is_empty() {
            want_qualifier.clone().unwrap_or_default()
        } else {
            want_name.clone()
        };
        out["suggestions"] = Value::Array(nearest_names(map, &probe, 8));
        add_miss_evidence(map, &mut out, qualifier_raw);
    }
    Ok(out)
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

// `money::` asks for everything *under* a qualifier, so it matches by prefix
fn qualifier_under(call_qualifier: Option<&str>, want: &str) -> bool {
    let segs = |s: &str| -> Vec<String> {
        s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|seg| !seg.is_empty())
            .map(|seg| seg.to_ascii_lowercase())
            .collect()
    };
    let Some(cq) = call_qualifier else {
        return false;
    };
    let have = segs(cq);
    let want = segs(want);
    !want.is_empty() && have.len() >= want.len() && have[..want.len()] == want[..]
}

// Levenshtein dist
fn edit_distance(a: &str, b: &str, cap: usize) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > cap {
        return None;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(cur[j]);
        }
        if row_min > cap {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[b.len()];
    (d <= cap).then_some(d)
}

const BAND: usize = 1000;

fn name_distance(want: &str, have: &str) -> Option<usize> {
    if want == have {
        return Some(0);
    }
    if have.contains(want) || want.contains(have) {
        return Some(BAND + have.len().abs_diff(want.len()).min(BAND - 1));
    }
    Some(2 * BAND + edit_distance(want, have, std::cmp::max(2, want.len() / 3))?)
}

// nearest indexed names to a query that found nothing
fn nearest_names(map: &MapState, want: &str, limit: usize) -> Vec<Value> {
    let want = want.trim().to_ascii_lowercase();
    if want.is_empty() {
        return Vec::new();
    }
    // a definition is a more useful suggestion than the import that binds it
    let rank = |kind: &str| match kind {
        "type" => 0,
        "func" => 1,
        "const" => 2,
        _ => 3,
    };
    let mut seen: BTreeSet<(String, &'static str)> = BTreeSet::new();
    let mut scored: Vec<(usize, usize, String, &'static str, String)> = Vec::new();
    for c in &map.caches {
        let path = map.path_of(c);
        let named = c
            .funcs
            .iter()
            .map(|f| (f.name.as_str(), "func"))
            .chain(c.consts.iter().map(|k| (k.name.as_str(), "const")))
            .chain(c.types.iter().map(|t| (t.name.as_str(), "type")))
            .chain(
                c.imports
                    .iter()
                    .flat_map(|i| i.names.iter().map(|n| (n.as_str(), "import"))),
            );
        for (name, kind) in named {
            if !seen.insert((name.to_ascii_lowercase(), kind)) {
                continue;
            }
            if let Some(score) = name_distance(&want, &name.to_ascii_lowercase()) {
                scored.push((score, rank(kind), name.to_string(), kind, path.clone()));
            }
        }
    }
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    // one row per name
    let mut named: BTreeSet<String> = BTreeSet::new();
    scored
        .into_iter()
        .filter(|(_, _, name, _, _)| named.insert(name.to_ascii_lowercase()))
        .take(limit)
        .map(|(_, _, name, kind, file)| json!({"name": name, "kind": kind, "file": file}))
        .collect()
}

// the kinds a lookup actually searched
const SEARCHED_KINDS: &[&str] = &[
    "func", "const", "type", "call", "use", "import", "reexport",
];

// the module name a file's contents are reachable under from outside it
fn file_facade(map: &MapState, c: &FileCache) -> Option<String> {
    let stem = c.rel_path.file_stem().and_then(|s| s.to_str())?;
    match stem {
        "lib" => map.facade.clone(),
        "mod" | "__init__" => c
            .rel_path
            .parent()
            .and_then(std::path::Path::file_name)
            .and_then(|s| s.to_str())
            .map(str::to_string),
        _ => Some(stem.to_string()),
    }
}

// one place a symbol is re-exported: which file publishes it, under what name,
// and which module it actually came from
struct Route {
    facade: String,
    module: String,
    file: String,
    line: usize,
    glob: bool,
}

// Where `name` is re-exported to, restricted to routes a `qualifier` could mean.
fn reexport_routes(map: &MapState, name: &str, qualifier: Option<&str>) -> Vec<Route> {
    let mut routes = Vec::new();
    for c in &map.caches {
        let Some(facade) = file_facade(map, c) else {
            continue;
        };
        for imp in c.imports.iter().filter(|i| i.reexport) {
            let glob = imp.names.is_empty();
            if !glob && !imp.names.iter().any(|n| n == name) {
                continue;
            }
            // facade and the module it re-exports from 
            // both are paths a caller legitimately writes, so both have to match
            let via = format!("{facade}::{}", imp.module);
            let wanted = match qualifier {
                None => true,
                Some(q) => {
                    qualifier_matches(Some(&facade), q) || qualifier_matches(Some(&via), q)
                }
            };
            if wanted {
                routes.push(Route {
                    facade: facade.clone(),
                    module: imp.module.clone(),
                    file: map.path_of(c),
                    line: imp.line,
                    glob,
                });
            }
        }
    }
    routes
}

// what the map knows about a *qualifier* whose symbol missed
fn qualifier_evidence(map: &MapState, qualifier: &str) -> (usize, Vec<String>) {
    let mut sites = 0usize;
    let mut examples = Vec::new();

    let note = |path: &str, line: usize, sites: &mut usize, examples: &mut Vec<String>| {
        *sites += 1;
        let at = format!("{path}:{line}");
        if examples.len() < 3 && !examples.contains(&at) {
            examples.push(at);
        }
    };
    for c in &map.caches {
        let path = map.path_of(c);
        for site in c.calls.iter().chain(c.uses.iter()) {
            if qualifier_under(site.qualifier.as_deref(), qualifier) {
                note(&path, site.line, &mut sites, &mut examples);
            }
        }
        for imp in &c.imports {
            if qualifier_under(Some(&imp.module), qualifier) {
                note(&path, imp.line, &mut sites, &mut examples);
            }
        }
    }
    (sites, examples)
}

// Attach the qualifier verdict to a zero-hit result.
fn add_miss_evidence(map: &MapState, out: &mut Value, qualifier: Option<&str>) {
    let Some(q) = qualifier else { return };
    let head = q.split([':', '.']).next().unwrap_or(q);
    let (sites, examples) = qualifier_evidence(map, q);
    out["qualifier_sites"] = json!(sites);
    out["qualifier_examples"] = json!(examples);

    match map.external_named(head) {
        Some(dep) => {
            out["declared"] = json!(true);
            out["external_dependency"] = dep.json();
        }
        None => out["declared"] = json!(false),
    }
}

fn q_references(map: &MapState, symbol: &str) -> Result<Value, String> {
    let symbol = symbol.trim();
    if symbol.is_empty() {
        return Err("empty symbol".into());
    }
    let (qualifier, name) = split_qualified(symbol);
    let mut definitions = Vec::new();
    let mut narrowed_defs = Vec::new();
    let mut references = Vec::new();
    let names_it = |q: &str, candidate: &str| crate::changes::qualifier_names_service(q, candidate);
    // a qualifier may name a facade rather than the file the definition sits in
    let routes = reexport_routes(map, name, qualifier);
    let via_reexport = |stem: &str, owner: Option<&str>| {
        qualifier.is_some()
            && routes
                .iter()
                .any(|r| names_it(&r.module, stem) || owner.is_some_and(|o| names_it(&r.module, o)))
    };
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
                    "ret": f.ret, "doc": f.comment, "owner": f.owner,
                    "span": [f.start_line, f.end_line],
                });
                // `money::charge` narrows by file stem; `Encoding::parse`
                // narrows by the type the method hangs off. Without the owner
                // check every `X::new` collapsed onto whichever project
                // function happened to be called `new`.
                let narrows = qualifier.is_some_and(|q| {
                    names_it(q, stem) || f.owner.as_deref().is_some_and(|o| names_it(q, o))
                }) || via_reexport(stem, f.owner.as_deref());
                if narrows {
                    narrowed_defs.push(def.clone());
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
                    names_it(q, stem) || k.ty.as_deref().is_some_and(|t| names_it(q, t))
                }) || via_reexport(stem, k.ty.as_deref());
                if narrows {
                    narrowed_defs.push(def.clone());
                }
                definitions.push(def);
            }
        }
        for t in &c.types {
            if t.name == name {
                let def = json!({
                    "kind": "type", "file": path, "line": t.line, "type": t.kind,
                });
                if qualifier.is_some_and(|q| names_it(q, stem)) || via_reexport(stem, None) {
                    narrowed_defs.push(def.clone());
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
        // An import binds a name without ever calling it. This is the only
        // evidence a derive macro, a trait or a type-only dependency leaves.
        for imp in &c.imports {
            let bound = imp.names.iter().any(|n| n == name);
            // `use foo::bar::*` binds nothing but still names `bar`
            let tail_named = imp.names.is_empty()
                && imp
                    .module
                    .rsplit(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                    .next()
                    .is_some_and(|seg| seg == name);
            let qualifier_ok = match qualifier {
                None => true,
                Some(q) => qualifier_matches(Some(&imp.module), q),
            };
            if (bound || tail_named) && qualifier_ok {
                // a re-export is not a use of the symbol, it is a second route
                // to it - the difference between "someone depends on this" and
                // "this is published API", which is exactly what a caller
                // weighing a rename needs to tell apart
                references.push(json!({
                    "kind": if imp.reexport { "reexport" } else { "import" },
                    "file": path, "line": imp.line,
                    "module": imp.module, "names": imp.names,
                }));
            }
        }
    }
    // A qualifier that names a file or an owning type narrows the definitions
    // to it. When it narrows nothing, the bare-name matches are not this
    // symbol at all - `Regex::new` is not the project's `Dir::new` - so they
    // are withheld from `definitions` and reported separately as what they are.
    let mut name_only = Vec::new();
    if !narrowed_defs.is_empty() {
        definitions = narrowed_defs;
    } else if qualifier.is_some() && !definitions.is_empty() {
        name_only = std::mem::take(&mut definitions);
    }
    // Whether this symbol is published, however the caller happened to spell the
    // lookup. A rename that reads as local is not local when a crate root
    // re-exports the name, and nothing else in the answer says so.
    let public = if qualifier.is_some() {
        reexport_routes(map, name, None)
    } else {
        routes
    };
    let defined_in = |module: &str| {
        definitions.iter().any(|d| {
            d.get("file")
                .and_then(|f| f.as_str())
                .map(std::path::Path::new)
                .and_then(std::path::Path::file_stem)
                .and_then(|s| s.to_str())
                .is_some_and(|stem| names_it(module, stem))
        })
    };
    let mut seen_export = BTreeSet::new();
    let exported_as: Vec<Value> = public
        .iter()
        .filter(|r| !r.glob || defined_in(&r.module))
        .filter_map(|r| {
            let path = format!("{}::{name}", r.facade);
            let at = format!("{}:{}", r.file, r.line);
            seen_export.insert((path.clone(), at.clone())).then(|| {
                json!({"path": path, "from": r.module, "at": at})
            })
        })
        .collect();

    let total_refs = references.len();
    references.truncate(REFS_CAP);
    let mut out = json!({
        "symbol": symbol,
        "name": name,
        "qualifier": qualifier,
        "counts": {"definitions": definitions.len(), "references": total_refs},
        "truncated": total_refs > REFS_CAP,
        "searched": SEARCHED_KINDS,
        "definitions": definitions,
        "references": references,
    });
    if !name_only.is_empty() {
        let shown: Vec<Value> = name_only.into_iter().take(REFS_CAP).collect();
        out["name_only_matches"] = Value::Array(shown);
    }
    if !exported_as.is_empty() {
        out["exported_as"] = Value::Array(exported_as);
    }
    // A miss is an answer, not an error: say what was covered and what to try,
    // so the lookup can be carried on rather than abandoned.
    if out["counts"]["definitions"] == 0 && total_refs == 0 {
        out["miss"] = json!(true);
        out["suggestions"] = Value::Array(nearest_names(map, name, 8));
        add_miss_evidence(map, &mut out, qualifier);
    }
    Ok(out)
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

// one externally declared package, straight from a manifest
#[derive(Debug, Clone)]
struct ExternalDep {
    name: String,
    version: Option<String>,
    // dependencies | dev-dependencies | build-dependencies | ...
    kind: String,
    manifest: String,
}

impl ExternalDep {
    fn json(&self) -> Value {
        json!({
            "name": self.name,
            "version": self.version,
            "kind": self.kind,
            "manifest": self.manifest,
        })
    }
}

// `"1.0"` or `{ version = "0.4", features = [..] }` -> the version string
fn toml_version(rest: &str) -> Option<String> {
    let r = rest.trim();
    let tail = match r.strip_prefix('{') {
        Some(inner) => &inner[inner.find("version")? + "version".len()..],
        None => r,
    };
    tail.split('"').nth(1).map(str::to_string)
}

// Declared dependencies from whatever manifests the root carries
fn manifest_deps(root: &Path) -> Vec<ExternalDep> {
    let mut out = Vec::new();
    let read = |name: &str| std::fs::read_to_string(root.join(name)).ok();

    if let Some(text) = read("Cargo.toml") {
        let mut section = String::new();
        for line in text.lines() {
            let t = line.split('#').next().unwrap_or("").trim();
            if let Some(h) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                section = h.trim().to_string();
                // `[dependencies.serde]` declares one dependency by section name
                if let Some((kind, name)) = section.split_once('.') {
                    if is_cargo_dep_section(kind) && !name.contains('.') {
                        out.push(ExternalDep {
                            name: name.trim_matches('"').to_string(),
                            version: None,
                            kind: kind.to_string(),
                            manifest: "Cargo.toml".into(),
                        });
                    }
                }
                continue;
            }
            let kind = section.rsplit('.').next().unwrap_or("");
            if !is_cargo_dep_section(kind) {
                continue;
            }
            let Some((name, rest)) = t.split_once('=') else {
                continue;
            };
            let name = name.trim().trim_matches('"');
            if name.is_empty() {
                continue;
            }
            out.push(ExternalDep {
                name: name.to_string(),
                version: toml_version(rest),
                kind: kind.to_string(),
                manifest: "Cargo.toml".into(),
            });
        }
    }

    // package.json: the two dependency objects, scanned as `"name": "range"`
    if let Some(text) = read("package.json") {
        for kind in ["dependencies", "devDependencies"] {
            let Some(start) = text.find(&format!("\"{kind}\"")) else {
                continue;
            };
            let Some(open) = text[start..].find('{').map(|i| start + i + 1) else {
                continue;
            };
            let Some(close) = text[open..].find('}').map(|i| open + i) else {
                continue;
            };
            for entry in text[open..close].split(',') {
                let Some((name, range)) = entry.split_once(':') else {
                    continue;
                };
                let name = name.trim().trim_matches('"');
                if name.is_empty() {
                    continue;
                }
                out.push(ExternalDep {
                    name: name.to_string(),
                    version: Some(range.trim().trim_matches('"').to_string()),
                    kind: kind.to_string(),
                    manifest: "package.json".into(),
                });
            }
        }
    }

    // go.mod: `require path v1.2.3`, single line or block
    if let Some(text) = read("go.mod") {
        let mut in_block = false;
        for line in text.lines() {
            let t = line.split("//").next().unwrap_or("").trim();
            if t.starts_with("require (") {
                in_block = true;
                continue;
            }
            if in_block && t == ")" {
                in_block = false;
                continue;
            }
            let spec = if in_block {
                t
            } else {
                match t.strip_prefix("require ") {
                    Some(s) => s.trim(),
                    None => continue,
                }
            };
            let mut parts = spec.split_whitespace();
            let (Some(path), version) = (parts.next(), parts.next()) else {
                continue;
            };
            if path.is_empty() {
                continue;
            }
            out.push(ExternalDep {
                name: path.to_string(),
                version: version.map(str::to_string),
                kind: "require".into(),
                manifest: "go.mod".into(),
            });
        }
    }

    // requirements.txt: one pinned distribution per line
    if let Some(text) = read("requirements.txt") {
        for line in text.lines() {
            let t = line.split('#').next().unwrap_or("").trim();
            if t.is_empty() || t.starts_with('-') {
                continue;
            }
            let split = t.find(|c| "=<>!~[ ;".contains(c)).unwrap_or(t.len());
            let (name, rest) = t.split_at(split);
            if name.is_empty() {
                continue;
            }
            out.push(ExternalDep {
                name: name.to_string(),
                version: (!rest.trim().is_empty()).then(|| rest.trim().to_string()),
                kind: "requirements".into(),
                manifest: "requirements.txt".into(),
            });
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.kind.cmp(&b.kind)));
    out.dedup_by(|a, b| a.name == b.name && a.kind == b.kind && a.manifest == b.manifest);
    out
}

fn is_cargo_dep_section(name: &str) -> bool {
    matches!(
        name,
        "dependencies" | "dev-dependencies" | "build-dependencies"
    )
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
                        // an import that binds no names still makes a whole
                        // file available
                        for f in &map.caches[b].funcs {
                            imported.entry(f.name.as_str()).or_default().insert(b);
                        }
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
            // declared external packages
            "external": map.externals.iter().map(ExternalDep::json).collect::<Vec<_>>(),
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
            // the raw count and the 1-10 band it falls in
            "complexity": f.metrics.complexity(),
            "complexity_score": f.metrics.complexity_score(),
            "branches": f.metrics.branches,
            "loop_depth": f.metrics.max_loop_depth(),
            "body_lines": f.metrics.body_lines,
        })).collect::<Vec<_>>(),
        "refs": c.refs.iter().map(|r| json!({
            "caller": r.caller, "call_line": r.call_line,
            "target": r.target_name, "target_line": r.target_line,
        })).collect::<Vec<_>>(),
        "notes": c.notes.iter().map(|n| json!({
            "line": n.line, "text": n.text,
        })).collect::<Vec<_>>(),
        // a file that defines nothing may still declare the shape of everything
        // around it
        "modules": c.modules,
        "imports": c.imports.iter().map(|i| json!({
            "line": i.line, "module": i.module, "names": i.names,
            "reexport": i.reexport,
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
        tool(
            "index",
            "START HERE in an unfamiliar project: what it contains, which directories carry the weight, where to look next. Use instead of `ls -R`, `find . -name '*.rs'` or `tree`. Every file the map holds is named, in path order, one row each - nothing is ever collapsed into a directory summary, so a row always tells you where something actually is. Most projects come back whole; the answer is capped at about a thousand lines, and past that the rest waits behind `offset` (or narrow with `path` instead of paging). The header totals always describe the whole filtered set, not the page. Where a project has module roots the rows carry two more columns: `mods`, the submodules a file declares, and `exp`, the names it re-exports - so a Rust `lib.rs` or `mod.rs`, which defines nothing and reads as zero in every other column, is visible as the module graph and public surface it is. Then drill: `path` for the files under one directory, `offset` for the next page of rows.",
            json!({
                "path": {"type": "string", "description": "limit to one subtree, e.g. `src/api` (optional)"},
                "limit": {"type": "integer", "description": "rows per page (default: as many as fit the ~1000-line ceiling, usually the whole project)"},
                "offset": {"type": "integer", "description": "rows to skip (default 0)"},
            }),
            &[],
        ),
        tool(
            "find",
            "USE INSTEAD OF `grep -rn <name>` whenever you know part of a name but not where it lives: is there a function like parse_config, what types end in Error, which files pull in clap. Substring match (case-insensitive) over functions, constants, type definitions (struct/enum/trait/class/interface/alias), notes, call and use sites, and import statements. Returns file:line with return types and doc summaries - no matches inside comments or strings, so nothing to filter by hand. A qualified query (`serde_json::to_string`, `client.charge`) matches sites under that qualifier; a trailing separator (`clap::`) lists everything under it. Zero hits names the kinds it searched and the nearest indexed names, so a miss tells you the symbol is absent rather than that your pattern was wrong.",
            json!({
                "query": {"type": "string", "description": "substring to search for; qualified form (a::b / a.b) searches call, use and import sites; trailing `::` lists a whole qualifier"},
                "kind": {"type": "string", "enum": ["any", "func", "const", "type", "note", "call", "import"], "description": "filter by symbol kind (default any)"},
            }),
            &["query"],
        ),
        tool(
            "references",
            "CALL THIS BEFORE renaming a symbol, changing a signature, or deleting anything that looks unused - it answers what calls this, who imports it, and is this dead. Use instead of `grep -rn 'foo('`, which misses imports and type-only uses while inventing hits in comments. Definitions, call sites, qualified value usages (enum variants, consts: `Encoding::O200kBase`) and import bindings of an exact name. Type definitions and imports are covered, so a struct used only through its type, or a crate pulled in for a derive, is still found. Qualified names (`serde_json::to_string`, `client.charge`, `Encoding::parse`) narrow by file, owning type and import module, and definitions that merely share the bare name are listed separately rather than passed off as the symbol. Re-exports are followed, so a crate-facade path (`mycrate::thing`, from a `pub use` in lib.rs) resolves to the definition in the module it actually lives in, and any symbol a module root republishes is reported under `published as` - renaming one is a breaking change even when every call site is local. Each hit carries its enclosing caller and test context, so production callers are distinguishable from test ones at a glance. A miss is an answer, not an error - it names the kinds searched, the nearest indexed names, and whether the qualifier is a declared dependency.",
            json!({"symbol": {"type": "string", "description": "exact symbol name, optionally qualified (a::b or a.b)"}}),
            &["symbol"],
        ),
        tool(
            "dependencies",
            "ANSWERS what breaks if I change this file, what this module pulls in, and whether a declared package is actually used anywhere. Use instead of opening files to read their import blocks. File-level edges resolved from imports and calls (type-only imports included), plus the external packages declared in the manifests (Cargo.toml, package.json, go.mod, requirements.txt). Call edges require the site to name the target module or use an imported symbol; name-only matches are excluded and listed in excluded_symbols, so an edge here is evidence rather than a guess. Without arguments: the whole graph plus declared dependencies; with `file`: what it depends on and what depends on it.",
            json!({"file": {"type": "string", "description": "relative path (optional)"}}),
            &[],
        ),
        tool(
            "file",
            "ANSWERS what is in this file - the submodules it declares, its imports (`pub` marks a re-export), every constant and function with its return type and doc summary, plus notes - for a fraction of the tokens reading it would cost. Use it to decide whether a file is worth opening at all, and on a module root (lib.rs, mod.rs, __init__.py) to read the module graph and published API of everything around it. NOT for editing: this is a map, not the code, so open the real source before you change a line. Pass structured=true for definition spans and the intra-file call graph instead of the rendered markdown.",
            json!({
                "path": {"type": "string", "description": "relative path, cache name, or unique path suffix"},
                "structured": {"type": "boolean", "description": "return spans and the intra-file call graph instead of the rendered markdown (default false)"},
            }),
            &["path"],
        ),
        tool(
            "notes",
            "ANSWERS what is left to do or known-broken in this project. Use instead of grepping for TODO and FIXME. All marker comments (TODO/FIXME/XXX/HACK/BUG/NOTE/SAFETY) with their file, line and enclosing function, optionally filtered to one marker.",
            json!({"marker": {"type": "string", "description": "e.g. TODO (optional)"}}),
            &[],
        ),
        tool("refresh", "CALL AFTER EDITING source files. Rescans the tree into memory; without it the map lags about three seconds behind your edit, and every other tool answers from that stale map.", json!({}), &[]),

        // analysis tools. All six are views onto one pass, computed once
        // per (map generation, base), paged.
        tool(
            "changes",
            "ANSWERS what have I changed on this branch, at function granularity rather than line granularity. Use instead of reading `git diff` by hand when you want to know which functions moved and what they touch. Diffed against a base ref: changed functions with the tests that name them, which services need testing, service edges, and the calls the resolver refused to attribute. Includes uncommitted edits and untracked files by default. This is the change set `test_triggers` refers to.",
            json!({
                "base": {"type": "string", "description": "git ref to diff against (default: merge-base with origin/main, main, origin/master or master - first that exists)"},
                "limit": {"type": "integer", "description": "changed functions per page (default 40, max 500)"},
                "offset": {"type": "integer", "description": "changed functions to skip (default 0)"},
            }),
            &[],
        ),
        tool(
            "test_triggers",
            "CALL BEFORE RUNNING ANY TEST SUITE, and again after editing: it answers which tests your changes actually put at risk, so you run those instead of everything, and which changes no test covers at all. Tests are matched to changed functions through the call graph, so a change deep in the stack still surfaces the tests above it; `distance` is how many call hops away each sits. Returns a runnable command per language.",
            json!({
                "base": {"type": "string", "description": "git ref to diff against (default: merge-base with origin/main, main, origin/master or master - first that exists)"},
                "limit": {"type": "integer", "description": "triggered tests and gaps per page (default 25, max 500)"},
                "offset": {"type": "integer", "description": "triggered tests and gaps to skip (default 0)"},
            }),
            &[],
        ),
        tool(
            "test_targets",
            "ANSWERS where should I add a test, and what kind. Functions ranked by how much a missing test would cost, each with the kind the measurements justify (smoke-test, integration-test, contract-test, perf-test, load-test), the reasoning behind that choice, and language-specific advice. Ranked by complexity, call depth, loop depth, call sites, cross-service callers, and whether anything names the function today.",
            json!({
                "kind": {"type": "string", "enum": ["smoke-test", "integration-test", "contract-test", "perf-test", "load-test"], "description": "only targets recommending this kind"},
                "limit": {"type": "integer", "description": "targets per page (default 15, max 500)"},
                "offset": {"type": "integer", "description": "targets to skip (default 0)"},
            }),
            &[],
        ),
        tool(
            "lints",
            "ANSWERS what looks risky in this code before you review or refactor it. Syntax-level findings: leaked resources, unrollable loops, inline candidates, deep nesting and similar. Every finding cites the measurement it came from, and every rule ships its own limits - there is no type or data-flow information behind these, so read the evidence and confirm in the source before acting on one.",
            json!({
                "rule": {"type": "string", "description": "only findings from this rule (see the rules section of any result)"},
                "limit": {"type": "integer", "description": "findings per page (default 40, max 500)"},
                "offset": {"type": "integer", "description": "findings to skip (default 0)"},
            }),
            &[],
        ),
        tool(
            "hot",
            "ANSWERS what is central to this codebase and where to start reading. Call-graph shape: the most-called functions, the widest fan-outs, the most complex, the deepest call chains, and recursion cycles. Static, not sampled: it ranks by call-graph shape - how many sites reach a function, how complex it is, how deep it sits - which is what the source says is hot, as opposed to what an execution trace would.",
            json!({
                "view": {"type": "string", "enum": ["most_called", "widest", "most_complex", "deepest_chains", "cycles"], "description": "one view (default: all five)"},
                "limit": {"type": "integer", "description": "rows per view (default 15, max 500)"},
                "offset": {"type": "integer", "description": "rows to skip (default 0)"},
            }),
            &[],
        ),
        tool(
            "services",
            "ANSWERS how the parts of this system talk to each other, and which code carries each hop. The service map and the call edges between services, with the call sites behind them. Services come from `.ccc/map.json` when present, top-level directories otherwise. An edge is `declared` if the config lists it, `detected` if calls were resolved across it - both are reported, since a declared HTTP or queue link resolves no calls by design. Edges also cross repositories: `externals` in `.ccc/map.json` names peer repos (a local checkout, or a surface they published with `ccc export`), and `ccc:calls` / `ccc:serves` comments naming the same key join a call here to its handler there, whatever language that repo is written in.",
            json!({
                "service": {"type": "string", "description": "drill into one service: its definition plus every edge touching it"},
                "limit": {"type": "integer", "description": "edges per page (default 25, max 500)"},
                "offset": {"type": "integer", "description": "edges to skip (default 0)"},
            }),
            &[],
        ),

        // the one tool aimed at the person rather than the agent
        tool(
            "insights",
            "CALL THIS WHEN THE USER ASKS TO SEE the analysis - show me the insights, open the dashboard, what does this codebase look like. Opens the human-facing insights UI (`/insights`) in their browser: the flame graph of the static call tree, hot paths, the service map, this branch's changes, test triggers and targets, lints and per-language totals - the same analysis pass the other tools read, laid out to be looked at rather than parsed. Returns the URL and the headline totals, so you can talk about the page while they read it. Needs the server started with `ccc serve --html`; without it the tool says so, and the data is at /insights.json either way.",
            json!({}),
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
        "instructions": "Code map of this project (the .ccc ContextCodeCache), held in \
            memory and refreshed automatically about three seconds after source changes.\n\n\
            1. SEARCHING - always start here. For any question about where something is \
            defined, called, imported or changed in this project, call a ccc tool before \
            reaching for grep or opening files. Hits carry file, line, enclosing caller \
            and qualifier with no textual false positives, and qualified names \
            (`serde_json::to_string`, `Encoding::O200kBase`) resolve directly. \
            Definitions, call sites, usages, imports and re-exports are all indexed, so \
            no hits is evidence of absence rather than a gap in coverage - a miss names \
            the kinds it searched and the nearest indexed names. A module root (Rust \
            `lib.rs`/`mod.rs`, a package `__init__.py`) defines nothing and is not \
            empty: it is read as the module graph and published surface it declares, and \
            a crate-facade path resolves through the `pub use` behind it. Use text \
            search only for non-symbol text: string literals, config, prose in \
            comments.\n\n\
            2. EDITING - do not work from the map. It records where code is and how it \
            connects, not its exact content: open the real source file before changing a \
            line, and never hand-edit anything under `.ccc`, which is overwritten on the \
            next scan. Call `refresh` after editing so later queries see your change.\n\n\
            3. TOOLS. Map: `index` project overview - every file named in path order, \
            never collapsed into directory summaries, most projects whole; past ~1000 \
            lines the rest waits behind `offset`, and `path` narrows to a subtree. \
            `find` symbols by \
            substring, `references` every definition, call, usage, import and re-export \
            of an exact name - check it before changing a signature, `dependencies` \
            file-level edges and declared packages, `file` one file's full map including \
            the submodules it declares, `notes` TODO/FIXME markers, `refresh` force a \
            rescan. Analysis: `changes` what this branch touched, `test_triggers` the \
            tests those changes make necessary, `test_targets` where a missing test \
            would cost most, `lints` syntax-level findings, `hot` call-graph shape, \
            `services` the service map and the calls crossing it. For a person rather \
            than an agent: `insights` opens the UI over that same analysis in their \
            browser - call it when they ask to *see* the code, not to read about it. \
            The analysis tools are \
            heuristics over a syntax tree - no type inference, data flow or runtime \
            profile - so each result carries its evidence and limits; read those before \
            acting. They page rather than truncate: on `showing 1-40 of 152`, pass \
            `offset` for the rest.\n\n\
            Results are markdown; the same data is JSON over HTTP (/index, /find, \
            /references, /dependencies, /file, /notes, /insights.json).",
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
// "note", call, use), printing only the fields that kind carries
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
    if let Some(module) = r.get("module").and_then(|x| x.as_str()) {
        line.push_str(&format!(" {module}"));
        let names = jnames(r, "names");
        if !names.is_empty() {
            line.push_str(&format!(" ({names})"));
        }
    }
    if let Some(span) = r.get("span").and_then(|x| x.as_array()) {
        if let [a, b] = &span[..] {
            line.push_str(&format!(" span {a}-{b}"));
        }
    }
    // the type a method hangs off - the evidence a qualified lookup narrows on
    if let Some(owner) = r.get("owner").and_then(|x| x.as_str()) {
        line.push_str(&format!(" owner={owner}"));
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

// What one `index` answer may spend, in rendered lines. Every file the filter
// selects is named, in path order; the ceiling only decides how many of them fit
// in one page. Nothing is ever summarised away - a row always stands for exactly
// one file, so a caller can act on what it says without having to ask whether
// this row is a file or a directory standing in for forty of them.
//
// Counted in lines rather than bytes because that is the thing the caller is
// actually rationing. Path length does not change the verdict: 900 files under
// `cmd/` and 900 under `packages/api/src/main/java/com/acme/` are the same 900
// lines to read past, whatever they cost to print.
const INDEX_LINE_CEILING: usize = 1_000;

// The page a caller gets without asking for one. Equal to the ceiling: paging is
// a backstop for output past it, not the normal way to read an index.
const INDEX_DEFAULT_ROWS: usize = INDEX_LINE_CEILING;

// one page of rows, plus the line accounting for whatever sits outside it.
// `structure` adds the two columns that describe a file's shape rather than its
// contents; it is off for maps where every file would report zero for both,
// since two dead columns on every row buy the caller nothing.
fn md_index_rows(out: &mut String, rows: &[[String; 8]], page: &Page, structure: bool) {
    let (window, note) = page.window(rows);
    let (extra_head, extra_rule) = if structure {
        (" mods | exp |", "---|---|")
    } else {
        ("", "")
    };
    out.push_str(&format!(
        "| file | lang | funcs | consts | refs | notes |{extra_head}\n\
         |---|---|---|---|---|---|{extra_rule}\n"
    ));
    for r in window {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |",
            r[0], r[1], r[2], r[3], r[4], r[5]
        ));
        if structure {
            out.push_str(&format!(" {} | {} |", r[6], r[7]));
        }
        out.push('\n');
    }
    if !note.is_empty() {
        out.push_str(&note);
    }
}

fn md_index(v: &Value, page: &Page) -> String {
    let t = v.get("totals").cloned().unwrap_or_default();
    let files = jarr(v, "files");
    // the totals line is always exact and always the whole filtered set, however
    // many of its rows this page has room for
    let structure = jnum(&t, "mods") > 0 || jnum(&t, "exports") > 0;
    let mut out = format!(
        "# {} - {} files (generated {})\n{} funcs, {} consts, {} refs, {} notes{}\n",
        jstr(v, "root"),
        jnum(&t, "files"),
        jstr(v, "generated"),
        jnum(&t, "funcs"),
        jnum(&t, "consts"),
        jnum(&t, "refs"),
        jnum(&t, "notes"),
        if structure {
            format!(
                ", {} mods, {} re-exports",
                jnum(&t, "mods"),
                jnum(&t, "exports")
            )
        } else {
            String::new()
        },
    );
    if let Some(f) = v.get("filter").and_then(|x| x.as_str()) {
        out.push_str(&format!(
            "filtered to `{f}` - {} of {} mapped files\n",
            files.len(),
            jnum(v, "project_files"),
        ));
    }
    out.push('\n');
    if files.is_empty() {
        out.push_str("no mapped files here\n");
        let avail = jnames(v, "available");
        if !avail.is_empty() {
            out.push_str(&format!("the map holds: {avail}\n"));
        }
        return out;
    }

    let row = |f: &Value| {
        [
            jstr(f, "path"),
            jstr(f, "language"),
            jnum(f, "funcs").to_string(),
            jnum(f, "consts").to_string(),
            jnum(f, "refs").to_string(),
            jnum(f, "notes").to_string(),
            jnum(f, "mods").to_string(),
            jnum(f, "exports").to_string(),
        ]
    };
    // What the ceiling has left for rows. The header already in `out` is charged
    // against it, as are the table's own two header lines and the count line
    // below it - the limit is on the answer, not on the part of it that happens
    // to be rows. When the listing will not fit, the two lines telling the caller
    // how to narrow it come out of the same budget.
    let spent = out.lines().count() + 3;
    let fits = files.len() <= INDEX_LINE_CEILING.saturating_sub(spent);
    let row_budget = INDEX_LINE_CEILING.saturating_sub(if fits { spent } else { spent + 2 });

    // Listed in path order, one row per file, always. A row that stood for a
    // whole directory would be cheaper, but it answers a question nobody asked:
    // the caller wants to know where something lives, and a subtree summary can
    // only tell them where to ask again. Path order is what makes `offset`
    // worth paging through - it walks the project - and `path` is there for
    // narrowing to the part that matters.
    let capped = Page {
        offset: page.offset,
        limit: page.limit.min(row_budget.max(1)),
    };
    // sorted here rather than assumed: `offset` only means "carry on from where
    // the last page stopped" if the order is a property of the answer, not of
    // whatever order the map happened to be built in
    let mut ordered: Vec<&Value> = files.iter().collect();
    ordered.sort_by_key(|f| jstr(f, "path"));
    let rows: Vec<[String; 8]> = ordered.into_iter().map(row).collect();
    md_index_rows(&mut out, &rows, &capped, structure);
    if !fits {
        out.push_str(&format!(
            "\nlisted in path order, one row per file, {} in total. Pass `path` \
             (e.g. `{}`) to narrow to one subtree rather than paging the whole tree.\n",
            files.len(),
            files
                .first()
                .map(|f| jstr(f, "path"))
                .and_then(|p| p.rsplit_once('/').map(|(dir, _)| dir.to_string()))
                .unwrap_or_else(|| "src".into()),
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
    let external = jarr(v, "external");
    if !external.is_empty() {
        let by_manifest: BTreeSet<String> =
            external.iter().map(|d| jstr(d, "manifest")).collect();
        out.push_str(&format!(
            "\n## declared dependencies ({}) - from {}\n",
            external.len(),
            by_manifest.into_iter().collect::<Vec<_>>().join(", "),
        ));
        for d in &external {
            let version = match d.get("version").and_then(|x| x.as_str()) {
                Some(v) => format!(" {v}"),
                None => String::new(),
            };
            out.push_str(&format!(
                "{}{version} ({})\n",
                jstr(d, "name"),
                jstr(d, "kind")
            ));
        }
    }
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
    out.push_str(&md_miss(v));
    out
}

// What a zero-hit answer owes the caller: the kinds it covered, the nearest
// names, and a next step. A bare "not found" reads as "stop" and cannot be told
// apart from "this kind is not indexed".
fn md_miss(v: &Value) -> String {
    if !jbool(v, "miss") {
        return String::new();
    }
    let suggestions = jarr(v, "suggestions");
    let mut out = format!("\nsearched kinds: {}\n", jnames(v, "searched"));
    // the verdict on the qualifier
    if let Some(q) = v.get("qualifier").and_then(|x| x.as_str()) {
        let sites = jnum(v, "qualifier_sites");
        let declared = v.get("external_dependency").map(|dep| {
            format!(
                "declared as a dependency ({} in {})",
                jstr(dep, "kind"),
                jstr(dep, "manifest")
            )
        });
        if sites > 0 {
            out.push_str(&format!(
                "`{q}` itself IS used: {sites} site(s) in the map ({}){}. \
                 This symbol is not indexed under it - the module is in use, so do \
                 not read this miss as the module being unused.\n",
                jnames(v, "qualifier_examples"),
                declared.map(|d| format!(", and {d}")).unwrap_or_default(),
            ));
        } else if let Some(d) = declared {
            out.push_str(&format!(
                "`{q}` is {d}, but no call, use or import site in the map names it. \
                 A dependency declared and never referenced is worth checking by hand.\n"
            ));
        } else {
            out.push_str(&format!(
                "`{q}` is named by no site in the map and declared in no manifest. \
                 Imports are indexed, so this is evidence of absence, not a gap in \
                 coverage - safe to conclude the project does not use it.\n"
            ));
        }
    }
    if !suggestions.is_empty() {
        let names: Vec<String> = suggestions
            .iter()
            .map(|s| format!("{} ({}, {})", jstr(s, "name"), jstr(s, "kind"), jstr(s, "file")))
            .collect();
        out.push_str(&format!("nearest indexed names: {}\n", names.join("; ")));
    }
    out.push_str(
        "next: `dependencies` for file-level edges, or text search for string \
         literals and config.\n",
    );
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
    // printed before the references because it changes what they mean
    let exported = jarr(v, "exported_as");
    if !exported.is_empty() {
        out.push_str(&format!(
            "\n## published as\nreachable outside this crate under {} - \
             renaming it is a breaking change.\n",
            if exported.len() == 1 { "this path" } else { "these paths" },
        ));
        for e in &exported {
            out.push_str(&format!(
                "{} (from {}, at {})\n",
                jstr(e, "path"),
                jstr(e, "from"),
                jstr(e, "at"),
            ));
        }
    }
    md_section(&mut out, "references", &hits("references"));
    let name_only = hits("name_only_matches");
    if !name_only.is_empty() {
        out.push_str(&format!(
            "\n## definitions sharing the name only\n\
             `{}` does not name any of these - not their file, not their owning \
             type - so they are not this symbol.\n{name_only}",
            jstr(v, "qualifier"),
        ));
    }
    out.push_str(&md_miss(v));
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
    let modules = jarr(v, "modules")
        .iter()
        .filter_map(|m| m.as_str().map(str::to_string))
        .collect::<Vec<_>>()
        .join(", ");
    let modules = if modules.is_empty() {
        String::new()
    } else {
        format!("{modules}\n")
    };
    // re-exports first and marked: they are this file's public surface, where a
    // plain `use` is only what it needed to do its own job
    let imports: String = jarr(v, "imports")
        .iter()
        .map(|i| {
            let names = jnames(i, "names");
            format!(
                "{} {}{}{}\n",
                jnum(i, "line"),
                if jbool(i, "reexport") { "pub " } else { "" },
                jstr(i, "module"),
                if names.is_empty() {
                    String::new()
                } else {
                    format!(" ({names})")
                },
            )
        })
        .collect();
    md_section(&mut out, "modules declared", &modules);
    md_section(&mut out, "consts (line name: type)", &consts);
    md_section(&mut out, "funcs (line:col name -> ret)", &funcs);
    md_section(&mut out, "calls (line caller -> target:line)", &refs);
    md_section(&mut out, "imports (line [pub] module (names))", &imports);
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
            limit: n("limit", default_limit).clamp(1, default_limit.max(500)),
        }
    }

    // the window, plus the line that accounts for everything outside it
    fn apply<'a>(&self, items: &'a [Value]) -> (&'a [Value], String) {
        self.window(items)
    }

    fn window<'a, T>(&self, items: &'a [T]) -> (&'a [T], String) {
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
                // a crossing names its transport and, when the far side is a
                // peer repository, says so - the target file is over there
                let hop = match jstr(s, "transport").as_str() {
                    "" => String::new(),
                    t if jbool(s, "external") => format!(" [{t}, other repo]"),
                    t => format!(" [{t}]"),
                };
                l.push_str(&format!(
                    "  {}:{} {} -> {} ({}:{}){}\n",
                    jstr(s, "caller_file"),
                    jnum(s, "caller_line"),
                    jstr(s, "caller"),
                    jstr(s, "symbol"),
                    jstr(s, "target_file"),
                    jnum(s, "target_line"),
                    hop,
                ));
            }
            l
        })
        .collect();
    md_section(&mut out, &format!("edges {note}"), &body);

    let externals = jarr(v, "externals");
    if !externals.is_empty() && only.is_none() {
        let body: String = externals
            .iter()
            .map(|e| {
                let status = if jbool(e, "resolved") {
                    format!(
                        "{} provided, {} consumed",
                        jnum(e, "provides"),
                        jnum(e, "consumes")
                    )
                } else {
                    format!("UNRESOLVED - {}", jstr(e, "error"))
                };
                format!(
                    "{} ({}) via {} - {}\n",
                    svc(&jstr(e, "name")),
                    jstr(e, "repo"),
                    jstr(e, "source"),
                    status
                )
            })
            .collect();
        md_section(&mut out, &format!("external repos ({})", externals.len()), &body);
    }

    // A `ccc:calls` naming a key nothing answers is either a typo at one end
    // or a peer nobody configured, and both are worth saying out loud.
    let dangling: Vec<Value> = jarr(v, "crossings")
        .into_iter()
        .filter(|c| c.get("remote").map(|r| r.is_null()).unwrap_or(true))
        .collect();
    if !dangling.is_empty() && only.is_none() {
        let body: String = dangling
            .iter()
            .take(20)
            .map(|c| {
                format!(
                    "{}:{} {} calls '{}' [{}] - nothing serves this key\n",
                    jstr(c, "file"),
                    jnum(c, "line"),
                    jstr(c, "function"),
                    jstr(c, "key"),
                    jstr(c, "transport"),
                )
            })
            .collect();
        md_section(&mut out, &format!("unanswered keys ({})", dangling.len()), &body);
    }

    let unassigned = jnames(v, "unassigned_files");
    if !unassigned.is_empty() && only.is_none() {
        md_section(&mut out, "unassigned files", &format!("{unassigned}\n"));
    }
    out
}

fn browser_origin(addr: &std::net::SocketAddr) -> String {
    let port = addr.port();
    match addr.ip() {
        ip if ip.is_unspecified() => format!("http://127.0.0.1:{port}"),
        std::net::IpAddr::V6(ip) => format!("http://[{ip}]:{port}"),
        ip => format!("http://{ip}:{port}"),
    }
}

fn open_in_browser(url: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let (program, args): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("open", &[])
    } else if cfg!(target_os = "windows") {
        // the empty string is `start`'s window-title argument; without it a
        // quoted URL would be taken as the title
        ("cmd", &["/C", "start", ""])
    } else {
        ("xdg-open", &[])
    };
    Command::new(program)
        .args(args)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|mut child| {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        })
        .map_err(|e| format!("{program}: {e}"))
}

// open the insights page
fn q_insights(map: &MapState, open: impl Fn(&str) -> Result<(), String>) -> Result<String, String> {
    let url = format!("{}/insights", map.origin);
    if !map.html {
        return Err(format!(
            "the insights UI is disabled; restart the server with `ccc serve --html` to \
             serve it at {url} (the analysis itself is at {}/insights.json, and the \
             `changes`, `test_triggers`, `test_targets`, `lints`, `hot` and `services` \
             tools read the same pass)",
            map.origin
        ));
    }
    let opened = open(&url);
    let a = map.analysis(None);
    let t = &a["totals"];
    let mut out = format!("# insights\n{url}\n\n");
    out.push_str(match &opened {
        Ok(()) => "opened in the user's browser - tell them to look at it.\n",
        Err(_) => "could not open a browser here; give the user the URL above.\n",
    });
    if let Err(e) = &opened {
        out.push_str(&format!("reason: {e}\n"));
    }
    out.push_str(&format!(
        "\n{} file(s), {} line(s), {} function(s), {} call edge(s), {} root(s)\n\
         generated {} in {} ms\n",
        jnum(t, "files"),
        jnum(t, "lines"),
        jnum(t, "functions"),
        jnum(t, "edges"),
        jnum(t, "roots"),
        jstr(&a, "generated"),
        a["took_ns"].as_u64().unwrap_or(0) / 1_000_000,
    ));
    out.push_str(
        "\ntabs: flame (static call tree), hot (call-graph shape), services, changes, \
         test triggers, test targets, lints, languages. The page reads /insights.json \
         live and has its own refresh button.\n",
    );
    Ok(out)
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
        "index" => Ok(md_index(
            &q_index(&map, arg("path").as_deref()),
            &Page::from(&args, INDEX_DEFAULT_ROWS),
        )),
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
        "insights" => q_insights(&map, open_in_browser),
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
    // a miss carries its own next step, so the panel never dead-ends
    let suggestions = jarr(v, "suggestions");
    let hint = if suggestions.is_empty() {
        String::new()
    } else {
        let names: Vec<String> = suggestions
            .iter()
            .map(|s| esc(&format!("{} ({})", jstr(s, "name"), jstr(s, "kind"))))
            .collect();
        format!(
            r#"<p class="text-xs text-amber-400 mt-1">nearest indexed names: {}</p>"#,
            names.join(", ")
        )
    };
    format!(
        r#"<p class="text-xs text-slate-500 mb-1">{} definition(s), {} reference(s)</p><div class="max-h-64 overflow-y-auto">{}{}</div>{}"#,
        defs.len(),
        v["counts"]["references"].as_u64().unwrap_or(0),
        def_rows,
        ref_rows,
        hint
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
    "GET /index[?path=<subtree>]",
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
            ok(q_index(&map, get("path")))
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
            // a symbol that is genuinely absent is a 200 with zero hits and the
            // guidance to go with it; 400 is reserved for a malformed request
            match q_references(&map, symbol) {
                Ok(v) => ok(v),
                Err(e) => bad(400, e),
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
        let mut map = state.write().expect("map lock poisoned");
        map.origin = match addr.clone().to_ip() {
            Some(ip) => browser_origin(&ip),
            None => format!("http://{}:{}", opts.addr, opts.port),
        };
    }
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

    // The shapes the fixture above has nothing to say about: imports, a name
    // shared by two owners, type definitions, and a manifest.
    fn fixture_imports() -> MapState {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ccc-serve-imports-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"shop\"\nversion = \"0.1.0\"\n\n\
             [dependencies]\n\
             serde = { version = \"1.0\", features = [\"derive\"] }\n\
             globset = \"0.4\"\n\n\
             [dependencies.tree-sitter]\n\
             version = \"0.25\"\n\n\
             [dev-dependencies]\n\
             tempfile = \"3\"\n",
        )
        .unwrap();
        fs::write(
            dir.join("src/store.rs"),
            "use serde::Serialize;\n\
             use globset::{Glob, GlobSet};\n\
             pub struct Basket { pub id: u64 }\n\
             impl Basket {\n    pub fn new(id: u64) -> Basket { Basket { id } }\n}\n\
             pub fn matcher() -> u64 { 1 }\n",
        )
        .unwrap();
        fs::write(
            dir.join("src/api.rs"),
            "use crate::store::Basket;\n\
             pub struct Cart { pub items: u64 }\n\
             impl Cart {\n    pub fn new() -> Cart { Cart { items: 0 } }\n}\n\
             pub fn open() -> u64 { 0 }\n",
        )
        .unwrap();
        let state = MapState::build(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        state
    }

    // A crate root: the file that declares the module graph and publishes the
    // API, and defines nothing itself.
    fn fixture_crate() -> MapState {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ccc-serve-crate-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"shop\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            dir.join("src/lib.rs"),
            "//! shop\n\
             pub mod store;\n\
             pub mod api;\n\
             pub use store::{checkout, Basket};\n\
             pub use api::*;\n",
        )
        .unwrap();
        fs::write(
            dir.join("src/store.rs"),
            "pub struct Basket { pub id: u64 }\n\
             pub fn checkout(b: u64) -> u64 { b }\n\
             pub fn tally(n: u64) -> u64 { n }\n",
        )
        .unwrap();
        fs::write(
            dir.join("src/api.rs"),
            "use crate::store::Basket;\n\
             pub fn open() -> u64 { 0 }\n",
        )
        .unwrap();
        let state = MapState::build(&dir).unwrap();
        let _ = fs::remove_dir_all(&dir);
        state
    }

    #[test]
    fn a_crate_root_does_not_report_as_an_empty_file() {
        let map = fixture_crate();
        let v = q_index(&map, None);
        let root = v["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == "src/lib.rs")
            .expect("lib.rs is mapped");
        // every count the index used to have reads zero here, and the file is
        // the most structural one in the project
        assert_eq!(root["funcs"], 0);
        assert_eq!(root["consts"], 0);
        assert_eq!(root["refs"], 0);
        assert_eq!(root["mods"], 2);
        // checkout + Basket, and the glob as the one name it is certain about
        assert_eq!(root["exports"], 3);

        let md = md_index(&v, &Page::from(&json!({}), INDEX_DEFAULT_ROWS));
        assert!(md.contains("| mods | exp |"), "{md}");
        assert!(md.contains("2 mods, 3 re-exports"), "{md}");
    }

    #[test]
    fn the_structure_columns_stay_off_a_map_with_no_structure() {
        // two zero columns on every row buy nothing; the fixture is a flat tree
        // of plain files with no module declarations and no re-exports
        let md = md_index(
            &q_index(&fixture(), None),
            &Page::from(&json!({}), INDEX_DEFAULT_ROWS),
        );
        assert!(!md.contains("mods"), "{md}");
        assert!(md.contains("| file | lang | funcs | consts | refs | notes |"), "{md}");
    }

    #[test]
    fn the_file_tool_shows_the_structure_a_module_root_is_made_of() {
        let map = fixture_crate();
        let v = q_file(&map, "src/lib.rs").unwrap();
        assert_eq!(
            v["modules"].as_array().unwrap(),
            &vec![json!("store"), json!("api")]
        );
        let md = md_file_structured(&v);
        assert!(md.contains("## modules declared\nstore, api"), "{md}");
        // `pub` distinguishes the published surface from a working import
        assert!(md.contains("pub store (checkout, Basket)"), "{md}");
        assert!(md.contains("pub api"), "{md}");
        // and the same file's entry in the on-disk cache says it too
        let cached = jstr(&v, "markdown");
        assert!(cached.contains("# modules\n    - store\n    - api\n"), "{cached}");
        assert!(cached.contains("L4@pub store (checkout, Basket)"), "{cached}");

        // a plain consumer is not marked
        let api = md_file_structured(&q_file(&map, "src/api.rs").unwrap());
        assert!(api.contains("1 crate::store (Basket)"), "{api}");
        assert!(!api.contains("pub crate::store"), "{api}");
    }

    // The editor draws one glyph per function from this
    #[test]
    fn the_file_tool_scores_every_function_it_reports() {
        let dir = std::env::temp_dir().join(format!("ccc-serve-cx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(
            dir.join("src/shapes.rs"),
            "pub fn flat(n: u64) -> u64 { n }\n\
             pub fn forked(n: u64) -> u64 { if n > 1 { n } else { 0 } }\n\
             pub fn knotted(n: u64) -> u64 {\n\
             \x20   let mut t = 0;\n\
             \x20   for i in 0..n { for j in 0..i { if j > 2 { t += 1; } else if j > 1 { t += 2; } } }\n\
             \x20   if t > 9 { t } else if t > 4 { t + 1 } else { 0 }\n\
             }\n",
        )
        .unwrap();
        let map = MapState::build(&dir).unwrap();
        let v = q_file(&map, "src/shapes.rs").unwrap();
        let funcs = v["funcs"].as_array().unwrap();
        assert_eq!(funcs.len(), 3, "{funcs:#?}");

        let band = |name: &str| -> u64 {
            funcs
                .iter()
                .find(|f| f["name"] == name)
                .unwrap_or_else(|| panic!("no {name} in {funcs:#?}"))["complexity_score"]
                .as_u64()
                .unwrap_or_else(|| panic!("{name} carries no band"))
        };
        // a straight-line body is the floor of the scale, not zero
        assert_eq!(band("flat"), 1);
        // and more decisions never score lower than fewer
        assert!(band("forked") > band("flat"), "{funcs:#?}");
        assert!(band("knotted") > band("forked"), "{funcs:#?}");
        for f in funcs {
            let s = f["complexity_score"].as_u64().unwrap();
            assert!((1..=10).contains(&s), "band {s} out of range: {f:#?}");
            // the raw count is what makes the band checkable, so it ships too
            assert!(f["complexity"].as_u64().unwrap() >= 1, "{f:#?}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_facade_qualifier_resolves_through_the_re_export() {
        let map = fixture_crate();
        // `shop::checkout` is a real path to src/store.rs, but `shop` names
        // neither that file nor an owning type - without following the re-export
        // the definition is withheld as a name-only collision
        let via_crate = q_references(&map, "shop::checkout").unwrap();
        assert_eq!(via_crate["counts"]["definitions"], 1, "{via_crate:#}");
        assert_eq!(via_crate["definitions"][0]["file"], "src/store.rs");
        assert!(via_crate.get("name_only_matches").is_none());

        // the fuller path a caller may equally write
        let via_module = q_references(&map, "shop::store::checkout").unwrap();
        assert_eq!(via_module["counts"]["definitions"], 1);
        assert_eq!(via_module["definitions"][0]["file"], "src/store.rs");

        // the module's own path still resolves, as it always did
        assert_eq!(
            q_references(&map, "store::checkout").unwrap()["counts"]["definitions"],
            1
        );

        // a type published the same way
        let basket = q_references(&map, "shop::Basket").unwrap();
        assert_eq!(basket["counts"]["definitions"], 1);
        assert_eq!(basket["definitions"][0]["kind"], "type");

        // and a glob re-export carries the whole module with it
        let globbed = q_references(&map, "shop::open").unwrap();
        assert_eq!(globbed["counts"]["definitions"], 1, "{globbed:#}");
        assert_eq!(globbed["definitions"][0]["file"], "src/api.rs");
    }

    #[test]
    fn a_published_symbol_says_so_however_the_lookup_was_spelled() {
        let map = fixture_crate();
        let v = q_references(&map, "checkout").unwrap();
        let exported = v["exported_as"].as_array().expect("published");
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0]["path"], "shop::checkout");
        assert_eq!(exported[0]["from"], "store");
        assert_eq!(exported[0]["at"], "src/lib.rs:4");
        assert!(
            md_references(&v).contains("shop::checkout (from store, at src/lib.rs:4)"),
            "{}",
            md_references(&v)
        );

        // the re-export is a route to the symbol, not a use of it
        let kinds: Vec<&str> = v["references"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["kind"].as_str())
            .collect();
        assert!(kinds.contains(&"reexport"), "{kinds:?}");

        // `tally` lives in the same re-exported module but is not named by the
        // re-export, so it is not published and must not claim to be
        let private = q_references(&map, "tally").unwrap();
        assert_eq!(private["counts"]["definitions"], 1);
        assert!(private.get("exported_as").is_none(), "{private:#}");
        // and a facade path that was never published does not resolve
        let bogus = q_references(&map, "shop::tally").unwrap();
        assert_eq!(bogus["counts"]["definitions"], 0);
        assert!(bogus["name_only_matches"].as_array().is_some(), "{bogus:#}");
    }

    #[test]
    fn type_definitions_are_findable() {
        let map = fixture_imports();
        // a struct used only through its type used to be invisible: extracted
        // into FileCache::types, never searched
        let found = q_find(&map, "Basket", "type").unwrap();
        assert_eq!(found["count"], 1);
        assert_eq!(found["results"][0]["kind"], "type");
        assert_eq!(found["results"][0]["file"], "src/store.rs");
        assert_eq!(found["results"][0]["type"], "struct");
        // and it resolves as a definition, with its import as a reference
        let refs = q_references(&map, "Basket").unwrap();
        assert_eq!(refs["counts"]["definitions"], 1);
        assert_eq!(refs["definitions"][0]["kind"], "type");
        let import_refs: Vec<_> = refs["references"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["kind"] == "import")
            .collect();
        assert_eq!(import_refs.len(), 1);
        assert_eq!(import_refs[0]["file"], "src/api.rs");
        // enums count too, from the other fixture
        let modes = q_find(&fixture(), "Mode", "type").unwrap();
        assert_eq!(modes["count"], 1);
        assert_eq!(modes["results"][0]["type"], "enum");
    }

    #[test]
    fn imports_are_the_only_trace_a_derive_leaves() {
        let map = fixture_imports();
        // `use serde::Serialize` never calls anything - the import is the only
        // evidence, and without it a miss could not be told from a coverage gap
        let refs = q_references(&map, "serde::Serialize").unwrap();
        assert_eq!(refs["counts"]["references"], 1);
        assert_eq!(refs["references"][0]["kind"], "import");
        assert_eq!(refs["references"][0]["file"], "src/store.rs");
        assert_eq!(refs["references"][0]["module"], "serde");
        // a braced import binds each name separately
        assert_eq!(
            q_references(&map, "globset::GlobSet").unwrap()["counts"]["references"],
            1
        );
        // and the bare name finds it without the qualifier
        let bare = q_find(&map, "GlobSet", "any").unwrap();
        assert!(bare["count"].as_u64().unwrap() >= 1);
        assert!(bare["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "import"));
        // a wrong qualifier still answers zero rather than erroring
        let wrong = q_references(&map, "notacrate::Serialize").unwrap();
        assert_eq!(wrong["counts"]["references"], 0);
        assert_eq!(wrong["miss"], true);
    }

    #[test]
    fn a_qualifier_narrows_definitions_by_owning_type() {
        let map = fixture_imports();
        // two `new`s, one per struct: the qualifier picks the right one
        for (symbol, file) in [("Basket::new", "src/store.rs"), ("Cart::new", "src/api.rs")] {
            let refs = q_references(&map, symbol).unwrap();
            assert_eq!(refs["counts"]["definitions"], 1, "{symbol}");
            assert_eq!(refs["definitions"][0]["file"], file, "{symbol}");
            assert!(refs["name_only_matches"].is_null(), "{symbol}");
        }
        // an unrelated qualifier resolves to neither. This is the `Regex::new`
        // case: it used to report whichever project `new` came first.
        let outside = q_references(&map, "Regex::new").unwrap();
        assert_eq!(outside["counts"]["definitions"], 0);
        assert_eq!(outside["name_only_matches"].as_array().unwrap().len(), 2);
        let md = md_references(&outside);
        assert!(md.contains("sharing the name only"));
        assert!(md.contains("owner=Basket"));
        // an unqualified lookup is unchanged: every same-named definition
        let bare = q_references(&map, "new").unwrap();
        assert_eq!(bare["counts"]["definitions"], 2);
    }

    #[test]
    fn a_trailing_separator_lists_a_whole_qualifier() {
        let map = fixture_imports();
        // `serde::` names a qualifier with no symbol after it
        let all = q_find(&map, "serde::", "any").unwrap();
        assert_eq!(all["count"], 1);
        assert_eq!(all["results"][0]["kind"], "import");
        assert_eq!(all["results"][0]["module"], "serde");
        // the dot form behaves the same, and a qualifier-only query matches by
        // prefix: `money::Mode::Fast` is under `money`, though `money` is not
        // the tail of its qualifier and a named lookup would not match it
        assert_eq!(q_find(&fixture(), "money.", "any").unwrap()["count"], 3);
        assert_eq!(q_find(&fixture(), "money::MAX", "any").unwrap()["count"], 1);
        // a separator with nothing in front names nothing
        assert!(q_find(&map, "::", "any").is_err());
    }

    #[test]
    fn a_miss_reports_its_coverage_and_the_nearest_names() {
        let map = fixture_imports();
        let miss = q_references(&map, "Baskett").unwrap();
        assert_eq!(miss["miss"], true);
        // the kinds it covered, so zero cannot be misread as "not indexed"
        let searched: Vec<&str> = miss["searched"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap())
            .collect();
        assert!(searched.contains(&"type") && searched.contains(&"import"));
        // one typo away from a real name, and the type definition is among the
        // suggestions rather than only the import that binds it
        assert_eq!(miss["suggestions"][0]["name"], "Basket");
        assert!(miss["suggestions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "Basket" && s["kind"] == "type"));
        let md = md_references(&miss);
        assert!(md.contains("searched kinds:"));
        assert!(md.contains("nearest indexed names: Basket"));
        // nothing remotely close: still an answer, just without guesses
        let far = q_references(&map, "zzzzzzzzzz").unwrap();
        assert!(far["suggestions"].as_array().unwrap().is_empty());
        assert!(md_references(&far).contains("searched kinds:"));
    }

    #[test]
    fn a_miss_says_whether_the_qualifier_itself_is_used() {
        let map = fixture_imports();
        // covers case where `serde::Deserializer` absent, but `serde`
        // is imported two lines away
        let wrong_symbol = q_references(&map, "serde::Deserializer").unwrap();
        assert_eq!(wrong_symbol["miss"], true);
        assert_eq!(wrong_symbol["qualifier_sites"], 1);
        assert_eq!(wrong_symbol["declared"], true);
        assert_eq!(wrong_symbol["external_dependency"]["name"], "serde");
        let md = md_references(&wrong_symbol);
        assert!(md.contains("`serde` itself IS used"));
        assert!(md.contains("src/store.rs:1"));
        assert!(md.contains("do not read this miss as the module being unused"));

        // nothing names it and no manifest declares it: the negative is now
        // safe to act on, and says so.
        let truly_absent = q_references(&map, "tokio::spawn").unwrap();
        assert_eq!(truly_absent["qualifier_sites"], 0);
        assert_eq!(truly_absent["declared"], false);
        assert!(truly_absent["external_dependency"].is_null());
        assert!(md_references(&truly_absent).contains("evidence of absence"));

        // declared but never referenced anywhere - worth a human look.
        let unused = q_references(&map, "tempfile::TempDir").unwrap();
        assert_eq!(unused["qualifier_sites"], 0);
        assert_eq!(unused["declared"], true);
        assert!(md_references(&unused).contains("declared and never referenced"));

        // `find` carries the same verdict
        let found = q_find(&map, "serde::Deserializer", "any").unwrap();
        assert_eq!(found["count"], 0);
        assert_eq!(found["qualifier_sites"], 1);
        assert!(md_find(&found).contains("`serde` itself IS used"));
    }

    #[test]
    fn declared_dependencies_come_from_the_manifests() {
        let map = fixture_imports();
        let deps = q_dependencies(&map, None).unwrap();
        let named = |name: &str| -> Option<Value> {
            deps["external"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["name"] == name)
                .cloned()
        };
        assert_eq!(named("globset").unwrap()["version"], "0.4");
        // an inline table yields its version, not its feature list
        assert_eq!(named("serde").unwrap()["version"], "1.0");
        // `[dependencies.tree-sitter]` is a dependency declared as a section
        assert_eq!(named("tree-sitter").unwrap()["kind"], "dependencies");
        assert_eq!(named("tempfile").unwrap()["kind"], "dev-dependencies");
        assert!(md_dependencies(&deps).contains("declared dependencies"));
        // hyphenated crates are written underscored in code
        assert_eq!(map.external_named("tree_sitter").unwrap().name, "tree-sitter");
    }

    #[test]
    fn manifest_parsing_covers_the_other_ecosystems() {
        let dir = std::env::temp_dir().join(format!("ccc-manifests-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("package.json"),
            "{\n  \"name\": \"web\",\n  \"dependencies\": { \"react\": \"^18.0.0\" },\n  \
             \"devDependencies\": { \"vitest\": \"1.2.0\" }\n}\n",
        )
        .unwrap();
        fs::write(
            dir.join("go.mod"),
            "module example.com/x\n\ngo 1.22\n\nrequire (\n\tgithub.com/pkg/errors v0.9.1\n)\n\
             require golang.org/x/sync v0.7.0\n",
        )
        .unwrap();
        fs::write(
            dir.join("requirements.txt"),
            "# comment\nrequests==2.31.0\nflask>=3\n-e .\n",
        )
        .unwrap();
        let deps = manifest_deps(&dir);
        let find = |n: &str| deps.iter().find(|d| d.name == n);
        assert_eq!(find("react").unwrap().kind, "dependencies");
        assert_eq!(find("vitest").unwrap().kind, "devDependencies");
        assert_eq!(
            find("github.com/pkg/errors").unwrap().version.as_deref(),
            Some("v0.9.1")
        );
        assert!(find("golang.org/x/sync").is_some());
        assert_eq!(find("requests").unwrap().version.as_deref(), Some("==2.31.0"));
        assert!(find("flask").is_some());
        // editable installs name no distribution
        assert!(!deps.iter().any(|d| d.name.starts_with('-')));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn suggestions_never_guess_wildly() {
        assert_eq!(name_distance("charge", "charge"), Some(0));
        assert!(name_distance("charge", "recharge") < name_distance("charge", "charhe"));
        assert!(name_distance("charge", "charge_all") < name_distance("charge", "charge_all_now"));
        // far enough apart is no suggestion at all, not a bad one
        assert_eq!(name_distance("charge", "zzzzzz"), None);
        assert_eq!(edit_distance("abc", "abcdefgh", 2), None);
        assert_eq!(edit_distance("kitten", "sitting", 3), Some(3));
    }

    #[test]
    fn find_kinds_are_validated_and_reported() {
        let map = fixture_imports();
        assert!(q_find(&map, "Basket", "bogus").is_err());
        // every answer says what it looked at
        let f = q_find(&map, "Basket", "any").unwrap();
        let searched: Vec<&str> = f["searched"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap())
            .collect();
        assert!(searched.contains(&"type") && searched.contains(&"import"));
        // a miss carries suggestions and the coverage "note" into the markdown
        let miss = q_find(&map, "Baskett", "any").unwrap();
        assert_eq!(miss["count"], 0);
        assert!(md_find(&miss).contains("nearest indexed names: Basket"));
    }

    #[test]
    fn index_and_find() {
        let map = fixture();
        let idx = q_index(&map, None);
        assert_eq!(idx["totals"]["files"], 2);
        let found = q_find(&map, "char", "any").unwrap();
        assert_eq!(found["count"], 1);
        assert_eq!(found["results"][0]["name"], "charge");
        assert_eq!(found["results"][0]["kind"], "func");
        let none = q_find(&map, "charge", "const").unwrap();
        assert_eq!(none["count"], 0);
        assert!(q_find(&map, "  ", "any").is_err());
    }

    // `index` as a caller gets it with no paging arguments
    fn idx_md(v: &Value) -> String {
        md_index(v, &Page::from(&json!({}), INDEX_DEFAULT_ROWS))
    }

    #[test]
    fn index_narrows_to_a_subtree_and_says_so() {
        let map = fixture();
        let idx = q_index(&map, Some("api"));
        assert_eq!(idx["totals"]["files"], 1);
        assert_eq!(idx["files"][0]["path"], "api/main.rs");
        assert_eq!(idx["project_files"], 2);
        let md = idx_md(&idx);
        assert!(md.contains("filtered to `api` - 1 of 2 mapped files"));
        // a prefix must not match a sibling that merely starts the same way
        assert_eq!(q_index(&map, Some("ap"))["totals"]["files"], 0);
        // leading ./ and stray slashes are the same request
        assert_eq!(q_index(&map, Some("./api/"))["totals"]["files"], 1);
        assert_eq!(q_index(&map, Some("  "))["totals"]["files"], 2);
    }

    #[test]
    fn an_index_filter_that_matches_nothing_reports_what_the_map_holds() {
        let map = fixture();
        let md = idx_md(&q_index(&map, Some("services/billing")));
        assert!(md.contains("no mapped files here"));
        assert!(md.contains("api/"), "{md}");
        assert!(md.contains("lib/"), "{md}");
    }

    // a project of `n` files spread over a splittable tree
    fn tree_index(n: usize, dir: &str) -> Value {
        let files: Vec<Value> = (0..n)
            .map(|i| {
                json!({
                    "path": format!("{dir}/svc{}/mod{}/f{i}.rs", i % 12, i % 60),
                    "language": "rust",
                    "funcs": 2, "consts": 1, "refs": 3, "notes": 0,
                })
            })
            .collect();
        json!({
            "root": "big", "generated": "now", "project_files": n,
            "totals": {"files": n, "funcs": 2 * n, "consts": n, "refs": 3 * n, "notes": 0},
            "files": files,
        })
    }

    #[test]
    fn a_project_under_the_ceiling_is_listed_whole() {
        // 900 files is a real project and comfortably inside the ceiling
        let md = idx_md(&tree_index(900, "src"));
        assert!(!md.contains("showing "), "{md}");
        let rows = md.lines().filter(|l| l.starts_with("| src/")).count();
        assert_eq!(rows, 900, "{}", &md[..400.min(md.len())]);
        assert!(md.contains("(900 total)"), "{md}");
        assert!(md.contains("| src/svc0/mod0/f0.rs |"), "{md}");
    }

    #[test]
    fn a_large_index_pages_and_never_summarises_a_directory() {
        let md = idx_md(&tree_index(3_000, "src"));
        // every row is one file
        assert!(!md.contains("rolled up"), "{}", &md[..400]);
        assert!(!md.contains(" files)"), "no row stands for a subtree");
        for l in md.lines().filter(|l| l.starts_with("| src/")) {
            assert!(l.contains(".rs |"), "not a file row: {l}");
        }
        // the headline totals stay exact whatever the page shows
        assert!(md.contains("3000 files"));
        assert!(md.contains("6000 funcs, 3000 consts, 9000 refs, 0 notes"));
        assert!(md.contains("showing 1-"), "{}", &md[..400]);
        assert!(md.contains("pass offset="), "{}", &md[..400]);
        assert!(md.contains("Pass `path`"), "narrowing is still offered");
        assert!(md.lines().count() <= INDEX_LINE_CEILING);
    }

    #[test]
    fn paths_are_listed_in_order_so_offset_walks_the_project() {
        let v = tree_index(3_000, "src");
        let rows = |md: &str| -> Vec<String> {
            md.lines()
                .filter(|l| l.contains(".rs |"))
                .map(str::to_string)
                .collect()
        };
        let first = rows(&idx_md(&v));
        let shown = first.len();
        let second = rows(&md_index(
            &v,
            &Page::from(&json!({"offset": shown}), INDEX_DEFAULT_ROWS),
        ));
        let sorted = |rs: &[String]| {
            let mut s = rs.to_vec();
            s.sort();
            s
        };
        assert_eq!(sorted(&first), first, "page one is in path order");
        assert_eq!(sorted(&second), second, "so is page two");
        // and page two picks up exactly where page one stopped
        assert!(first.last() < second.first(), "pages are contiguous");
    }

    #[test]
    fn the_ceiling_counts_lines_so_path_shape_does_not_decide() {
        let shallow = idx_md(&tree_index(900, "cmd"));
        let deep = idx_md(&tree_index(900, "packages/api/src/main/java/com/acme"));
        for md in [&shallow, &deep] {
            // 900 files plus the table's own header row
            assert_eq!(md.lines().filter(|l| l.starts_with("| ")).count(), 901);
        }
        assert!(deep.len() > shallow.len() * 3 / 2, "the deep one is far wider in bytes");

        // and every answer honours the ceiling, whatever its shape
        for n in [10, 900, 3_000] {
            for dir in ["cmd", "packages/api/src/main/java/com/acme"] {
                let md = idx_md(&tree_index(n, dir));
                assert!(
                    md.lines().count() <= INDEX_LINE_CEILING,
                    "{n} files under {dir}: {} lines",
                    md.lines().count()
                );
            }
        }
    }

    #[test]
    fn index_pages_only_once_the_output_runs_past_the_ceiling() {
        // a flat tree of 3000 files: nothing to roll up, so paging is the only
        // thing left to bound the answer with
        let files: Vec<Value> = (0..3_000)
            .map(|i| {
                json!({
                    "path": format!("f{i:04}.rs"), "language": "rust",
                    "funcs": 1, "consts": 0, "refs": 3_000 - i, "notes": 0,
                })
            })
            .collect();
        let v = json!({
            "root": "flat", "generated": "now", "project_files": 3_000,
            "totals": {"files": 3_000, "funcs": 3_000, "consts": 0, "refs": 1, "notes": 0},
            "files": files,
        });
        let rows = |md: &str| -> Vec<String> {
            md.lines()
                .filter(|l| l.contains(".rs |"))
                .map(str::to_string)
                .collect()
        };
        let first = md_index(&v, &Page::from(&json!({}), INDEX_DEFAULT_ROWS));
        assert!(first.lines().count() <= INDEX_LINE_CEILING, "{}", first.lines().count());
        let shown = rows(&first).len();
        // the page fills the ceiling rather than a fixed dozen rows
        assert!(shown > INDEX_LINE_CEILING - 20, "only {shown} rows");
        assert!(first.contains(&format!("showing 1-{shown} of 3000")), "{}", &first[..300]);

        // and the rest is reachable, disjoint from the first page
        let second = md_index(
            &v,
            &Page::from(&json!({"offset": shown}), INDEX_DEFAULT_ROWS),
        );
        let (a, b) = (rows(&first), rows(&second));
        assert!(a.iter().all(|r| !b.contains(r)), "pages overlap");
        assert!(second.contains(&format!("showing {}-", shown + 1)), "{}", &second[..300]);

        // a caller wanting less still gets less
        let small = md_index(&v, &Page::from(&json!({"limit": 20}), INDEX_DEFAULT_ROWS));
        assert_eq!(rows(&small).len(), 20);
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
        // a genuine absence is an answer: zero hits, the kinds covered, and the
        // nearest names - not an error that reads as "stop looking"
        let miss = q_references(&map, "nowhere").unwrap();
        assert_eq!(miss["counts"]["definitions"], 0);
        assert_eq!(miss["counts"]["references"], 0);
        assert!(miss["searched"].as_array().unwrap().contains(&json!("import")));
        assert!(miss["suggestions"].as_array().is_some());
        // only a malformed request is still an error
        assert!(q_references(&map, "  ").is_err());
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
        // A qualifier that names neither a file nor an owning type does not
        // resolve to the bare-name definition: `bogus::charge` is not this
        // `charge`. It is reported apart, as the name collision it is.
        let miss = q_references(&map, "bogus::charge").unwrap();
        assert_eq!(miss["counts"]["references"], 0);
        assert_eq!(miss["counts"]["definitions"], 0);
        assert_eq!(miss["name_only_matches"].as_array().unwrap().len(), 1);
        assert_eq!(miss["name_only_matches"][0]["file"], "lib/money.rs");
        assert!(md_references(&miss).contains("sharing the name only"));
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
        let as_json = serde_json::to_string_pretty(&q_index(&fixture(), None)).unwrap();
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
    fn the_instructions_cover_search_editing_and_every_tool() {
        let text = mcp_initialize(&json!({}))["instructions"]
            .as_str()
            .unwrap()
            .to_string();
        // the two rules a caller has to get right
        assert!(text.contains("SEARCHING - always start here"), "{text}");
        assert!(text.contains("EDITING - do not work from the map"), "{text}");
        for t in mcp_tools()["tools"].as_array().unwrap() {
            let name = t["name"].as_str().unwrap();
            assert!(
                text.contains(&format!("`{name}`")),
                "instructions never mention `{name}`"
            );
        }
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
                "index", // the map
                "find",
                "references",
                "dependencies",
                "file",
                "notes",
                "refresh",
                "changes",
                "test_triggers",
                "test_targets",
                "lints",
                "hot",
                "services",
                "insights", // for user only
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
        // an unknown symbol is a successful answer with zero hits and the
        // guidance to carry on; isError is for malformed input only
        let miss = mcp_handle(
            &state,
            &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                    "params": {"name": "references", "arguments": {"symbol": "ghost"}}}),
        )
        .unwrap();
        assert_eq!(miss["result"]["isError"], false);
        let text = miss["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("0 definition(s), 0 reference(s)"));
        assert!(text.contains("searched kinds:"));
        let bad_args = mcp_handle(
            &state,
            &json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
                    "params": {"name": "references", "arguments": {"symbol": "  "}}}),
        )
        .unwrap();
        assert_eq!(bad_args["result"]["isError"], true);
        // unknown method -> -32601
        let nope = mcp_handle(
            &state,
            &json!({"jsonrpc": "2.0", "id": 5, "method": "prompts/list"}),
        )
        .unwrap();
        assert_eq!(nope["error"]["code"], -32601);
    }


    // verify all tools dispatch as expected
    #[test]
    fn every_advertised_tool_dispatches() {
        let registry = mcp_tools();
        let tools = registry["tools"].as_array().unwrap();
        assert!(!tools.is_empty(), "the registry advertises no tools");

        for t in tools {
            let name = t["name"].as_str().unwrap();
            let schema = &t["inputSchema"];

            let mut args = json!({});
            let obj = args.as_object_mut().unwrap();
            for req in schema["required"].as_array().unwrap() {
                let key = req.as_str().unwrap();
                let placeholder = match schema["properties"][key]["type"].as_str() {
                    Some("integer") => json!(1),
                    Some("boolean") => json!(false),
                    _ => json!("charge"), // a name the fixture actually has
                };
                obj.insert(key.to_string(), placeholder);
            }

            let state = RwLock::new(fixture());
            let reply = mcp_handle(
                &state,
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": name, "arguments": args}}),
            )
            .unwrap_or_else(|| panic!("{name}: a tools/call with an id must be answered"));

            assert!(
                reply.get("error").is_none(),
                "{name} is advertised by tools/list but does not dispatch: {}",
                reply["error"]
            );

            assert!(
                reply["result"]["content"][0]["text"].is_string(),
                "{name}: dispatched without content: {reply}"
            );
        }
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
        // absent symbol: 200 with zero hits, not 404 - the map has an answer
        let ghost = route(&state, "GET", "/references?symbol=ghost", b"");
        assert_eq!(ghost.status, 200);
        assert_eq!(json_of(&ghost)["counts"]["references"], 0);
        // a malformed request is still a 400
        assert_eq!(route(&state, "GET", "/references?symbol=", b"").status, 400);
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

    // The `insights` tool is the one that acts on the user's machine, so the
    // browser call is injected: these assertions must never launch anything.
    #[test]
    fn the_insights_tool_hands_the_user_a_reachable_url() {
        let mut map = fixture();
        map.origin = "http://127.0.0.1:7788".into();

        // disabled UI: the error names the flag and the JSON way in, and no
        // browser is opened for a page that would 404
        let err = q_insights(&map, |_| panic!("must not open a browser with --html off"))
            .expect_err("the UI is off in the fixture");
        assert!(err.contains("ccc serve --html"), "{err}");
        assert!(err.contains("http://127.0.0.1:7788/insights.json"), "{err}");

        map.html = true;
        let asked = Mutex::new(Vec::new());
        let out = q_insights(&map, |url| {
            asked.lock().unwrap().push(url.to_string());
            Ok(())
        })
        .expect("the UI is enabled");
        assert_eq!(
            asked.into_inner().unwrap(),
            vec!["http://127.0.0.1:7788/insights".to_string()]
        );
        assert!(out.contains("http://127.0.0.1:7788/insights"), "{out}");
        assert!(out.contains("opened in the user's browser"), "{out}");
        // the headline figures the agent talks about while they read
        assert!(out.contains("function(s)"), "{out}");
        assert!(out.contains("call edge(s)"), "{out}");

        // a headless box has nothing to open: still an answer, with the URL and
        // the reason, not an error
        let headless = q_insights(&map, |_| Err("xdg-open: not found".into()))
            .expect("a missing browser is not a failed tool call");
        assert!(headless.contains("give the user the URL"), "{headless}");
        assert!(headless.contains("xdg-open: not found"), "{headless}");
        assert!(headless.contains("http://127.0.0.1:7788/insights"), "{headless}");
    }

    #[test]
    fn a_wildcard_bind_is_advertised_as_loopback() {
        let wild: std::net::SocketAddr = "0.0.0.0:6767".parse().unwrap();
        assert_eq!(browser_origin(&wild), "http://127.0.0.1:6767");
        let v6: std::net::SocketAddr = "[::]:80".parse().unwrap();
        assert_eq!(browser_origin(&v6), "http://127.0.0.1:80");
        let lan: std::net::SocketAddr = "192.168.1.9:6767".parse().unwrap();
        assert_eq!(browser_origin(&lan), "http://192.168.1.9:6767");
        let loop6: std::net::SocketAddr = "[::1]:6767".parse().unwrap();
        assert_eq!(browser_origin(&loop6), "http://[::1]:6767");
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
        // a miss is 200 with zero counts and its nearest-name hint inline, so
        // HTMX always swaps and the panel never dead-ends
        let miss = route(&state, "GET", "/fragment/references?symbol=ghost", b"");
        assert_eq!(miss.status, 200);
        assert!(html_of(&miss).contains("0 definition(s), 0 reference(s)"));
        // malformed input is still the inline error styling
        let bad_req = route(&state, "GET", "/fragment/references?symbol=", b"");
        assert_eq!(bad_req.status, 200);
        assert!(html_of(&bad_req).contains("empty symbol"));
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
