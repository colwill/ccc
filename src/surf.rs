//! `ccc surf` - surface branch changes to a continuous-testing suite.
//!
//! Groups source files into named services (from `.ccc/surf.json` and/or
//! `--service` flags), diffs the branch against a base ref.

use crate::model::FileCache;
use crate::scan;
use anyhow::{anyhow, bail, Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SCHEMA: &str = "ccc-surf/1";
const MAX_EDGE_SYMBOLS: usize = 100;


#[derive(Debug, Default)]
pub struct SurfOptions {
    // base ref to diff against; None resolves to (origin/main)
    pub base: Option<String>,
    // extra `NAME=GLOB` service definitions merged over `.ccc/surf.json`
    pub service_flags: Vec<(String, String)>,
}


// hand-maintained service map, `.ccc/surf.json`, will probably add
// best guess support if low impact.
#[derive(Debug, Default, Deserialize)]
pub struct SurfConfig {
    #[serde(default)]
    pub services: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub deps: BTreeMap<String, Vec<String>>,
}

impl SurfConfig {
    pub fn load(root: &Path) -> Result<SurfConfig> {
        let path = root.join(".ccc").join("surf.json");
        if !path.is_file() {
            return Ok(SurfConfig::default());
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }
}


#[derive(Debug, Serialize, Clone)]
pub struct ChangedFile {
    pub path: String,
    // added | modified | deleted | renamed | copied | type_changed
    pub status: String,
    pub services: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChangedFunction {
    pub services: Vec<String>,
    pub file: String,
    pub function: String,
    pub lines: [usize; 2],
    pub tested: bool,
    pub called_from: Vec<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct EdgeSymbol {
    pub symbol: String,
    pub file: String,
    pub line: usize,
}

#[derive(Debug, Serialize, Clone)]
pub struct ServiceEdge {
    pub from: String,
    pub to: String,
    // listed in `.ccc/surf.json` `deps` (vs detected by symbol matching)
    pub declared: bool,
    // detected call-site evidence, unique per symbol
    pub symbols: Vec<EdgeSymbol>,
}

#[derive(Debug, Serialize, Clone)]
pub struct Impact {
    pub service: String,
    pub reason: String,
    pub path: Vec<String>,
}

#[derive(Debug, Serialize, Clone, Copy)]
pub struct SurfCounts {
    pub services_to_test: usize,
    pub changed_files: usize,
    pub changed_functions: usize,
    pub untested: usize,
}

#[derive(Debug, Serialize)]
pub struct SurfReport {
    pub schema: &'static str,
    pub root: String,
    pub base: String,
    pub base_sha: String,
    pub head_sha: String,
    pub services: Vec<String>,
    pub changed_files: Vec<ChangedFile>,
    pub changed_functions: Vec<ChangedFunction>,
    pub edges: Vec<ServiceEdge>,
    pub services_to_test: Vec<String>,
    pub impact: Vec<Impact>,
    pub untested: Vec<ChangedFunction>,
    pub unassigned_files: Vec<String>,
    pub counts: SurfCounts,
}

// analyze `root` and build the surf report.
pub fn surf(root: &Path, root_label: &str, opts: &SurfOptions) -> Result<SurfReport> {
    let mut config = SurfConfig::load(root)?;
    for (name, glob) in &opts.service_flags {
        config
            .services
            .entry(name.clone())
            .or_default()
            .push(glob.clone());
    }
    let implicit = config.services.is_empty();
    if implicit {
        // no config: the whole root is one service "." no cross-service edges
        config.services.insert(".".into(), vec!["**".into()]);
    }
    for (from, tos) in &config.deps {
        for t in std::iter::once(from).chain(tos) {
            if !config.services.contains_key(t) {
                bail!(
                    "surf.json deps mention unknown service '{t}' \
                     (known: {})",
                    config.services.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }
        }
    }
    let matchers = build_matchers(&config.services)?;
    let service_names: Vec<String> = config.services.keys().cloned().collect();

    let (base_label, base_sha) = resolve_base(root, opts.base.as_deref())?;
    let head_sha = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    let name_status = git_bytes(
        root,
        &["diff", "--relative", "--name-status", "-z", "-M", &base_sha, "HEAD"],
    )?;
    let statuses = parse_name_status(&name_status);
    let hunk_text = git(
        root,
        &["diff", "--relative", "--unified=0", "-M", &base_sha, "HEAD"],
    )?;
    let hunks = parse_hunks(&hunk_text);

    // parse the current tree (same walker as `scan`; no `.ccc` needed)
    let files = scan::collect_files(root)?;
    let caches = scan::build_caches(root, &files);
    let idx = build_indexes(&caches, &matchers);

    // cross-service edges + per-symbol caller map
    let (edges, symbol_callers) = detect_edges(&idx, &config.deps);

    // changed files -> services
    let mut changed_files = Vec::new();
    let mut unassigned = BTreeSet::new();
    let mut changed_services = BTreeSet::new();
    for (status, path) in &statuses {
        let services = assign(&matchers, path);
        if services.is_empty() {
            unassigned.insert(path.clone());
        }
        for s in &services {
            changed_services.insert(s.clone());
        }
        changed_files.push(ChangedFile {
            path: path.clone(),
            status: status.clone(),
            services,
        });
    }
    changed_files.sort_by(|a, b| a.path.cmp(&b.path));

    // changed functions: hunk ranges vs function spans
    let mut changed_functions = Vec::new();
    for cache in &caches {
        let rel = path_str(&cache.rel_path);
        let Some(ranges) = hunks.get(&rel) else { continue };
        let services = assign(&matchers, &rel);
        let file_is_test = is_test_path(&rel);
        for f in &cache.funcs {
            let touched = ranges
                .iter()
                .any(|&(s, e)| s <= f.end_line && f.start_line <= e);
            if !touched {
                continue;
            }
            let is_test_code = file_is_test || f.test_ctx;
            let tested = is_test_code || idx.test_called.contains(&f.name);
            let called_from: Vec<String> = symbol_callers
                .get(&f.name)
                .map(|callers| {
                    callers
                        .iter()
                        .filter(|c| !services.contains(c))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            changed_functions.push(ChangedFunction {
                services: services.clone(),
                file: rel.clone(),
                function: f.name.clone(),
                lines: [f.start_line, f.end_line],
                tested,
                called_from,
            });
        }
    }
    changed_functions.sort_by(|a, b| (&a.file, a.lines[0]).cmp(&(&b.file, b.lines[0])));

    // impact closure over reverse edges
    let impact = impact_closure(&changed_services, &edges);
    let services_to_test: Vec<String> = impact.iter().map(|i| i.service.clone()).collect();

    let untested: Vec<ChangedFunction> = changed_functions
        .iter()
        .filter(|f| !f.tested)
        .cloned()
        .collect();

    let counts = SurfCounts {
        services_to_test: services_to_test.len(),
        changed_files: changed_files.len(),
        changed_functions: changed_functions.len(),
        untested: untested.len(),
    };

    Ok(SurfReport {
        schema: SCHEMA,
        root: root_label.to_string(),
        base: base_label,
        base_sha,
        head_sha,
        services: service_names,
        changed_files,
        changed_functions,
        edges,
        services_to_test,
        impact,
        untested,
        unassigned_files: unassigned.into_iter().collect(),
        counts,
    })
}

// scaffold a starter `.ccc/surf.json`
pub fn init_config(root: &Path) -> Result<PathBuf> {
    let path = root.join(".ccc").join("surf.json");
    if path.exists() {
        bail!("{} already exists; edit or remove it first", path.display());
    }
    let files = scan::collect_files(root)?;
    let mut services: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in &files {
        let rel = f.strip_prefix(root).unwrap_or(f);
        let mut comps = rel.components();
        let first = comps
            .next()
            .and_then(|c| c.as_os_str().to_str())
            .unwrap_or("root")
            .to_string();
        if comps.next().is_some() {
            // nested: top-level dir becomes a service
            services
                .entry(first.clone())
                .or_insert_with(|| vec![format!("{first}/**")]);
        } else {
            // loose file at the root
            services.entry("root".into()).or_insert_with(|| vec!["*".into()]);
        }
    }
    if services.is_empty() {
        bail!("no supported source files found under {}", root.display());
    }
    let out = serde_json::json!({ "services": services, "deps": {} });
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&out)?))?;
    Ok(path)
}

struct OwnedCall {
    service: String,
    file: String,
    line: usize,
    name: String,
    qualifier: Option<String>,
}

struct Indexes {
    def_services: BTreeMap<String, BTreeSet<String>>,
    calls: Vec<OwnedCall>,
    test_called: BTreeSet<String>,
}

fn build_indexes(caches: &[FileCache], matchers: &[(String, GlobSet)]) -> Indexes {
    let mut def_services: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut calls = Vec::new();
    let mut test_called = BTreeSet::new();

    for cache in caches {
        let rel = path_str(&cache.rel_path);
        let services = assign(matchers, &rel);
        let file_is_test = is_test_path(&rel);

        for f in &cache.funcs {
            for s in &services {
                def_services.entry(f.name.clone()).or_default().insert(s.clone());
            }
        }
        for c in &cache.calls {
            if file_is_test || c.test_ctx {
                test_called.insert(c.name.clone());
            }
            for s in &services {
                calls.push(OwnedCall {
                    service: s.clone(),
                    file: rel.clone(),
                    line: c.line,
                    name: c.name.clone(),
                    qualifier: c.qualifier.clone(),
                });
            }
        }
    }
    Indexes {
        def_services,
        calls,
        test_called,
    }
}

// Detect cross-service edges by symbol matching, then overlay declared deps.
// Ambiguous unqualified names (`new`, `run`, ... defined in many services)
// produce no edge, declare those dependencies in surf.json `deps` instead.
fn detect_edges(
    idx: &Indexes,
    declared: &BTreeMap<String, Vec<String>>,
) -> (Vec<ServiceEdge>, BTreeMap<String, BTreeSet<String>>) {
    let mut edge_map: BTreeMap<(String, String), Vec<EdgeSymbol>> = BTreeMap::new();
    let mut symbol_callers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for call in &idx.calls {
        let Some(def_svcs) = idx.def_services.get(&call.name) else {
            continue;
        };
        if def_svcs.contains(&call.service) {
            continue;
        }
        let targets: Vec<&String> = if def_svcs.len() == 1 {
            def_svcs.iter().collect()
        } else {
            def_svcs
                .iter()
                .filter(|svc| {
                    call.qualifier
                        .as_deref()
                        .map(|q| qualifier_names_service(q, svc))
                        .unwrap_or(false)
                })
                .collect()
        };
        for target in targets {
            symbol_callers
                .entry(call.name.clone())
                .or_default()
                .insert(call.service.clone());
            let symbols = edge_map
                .entry((call.service.clone(), target.clone()))
                .or_default();
            if symbols.len() < MAX_EDGE_SYMBOLS
                && !symbols.iter().any(|s| s.symbol == call.name)
            {
                symbols.push(EdgeSymbol {
                    symbol: call.name.clone(),
                    file: call.file.clone(),
                    line: call.line,
                });
            }
        }
    }

    let mut declared_set = BTreeSet::new();
    for (from, tos) in declared {
        for to in tos {
            declared_set.insert((from.clone(), to.clone()));
            edge_map.entry((from.clone(), to.clone())).or_default();
        }
    }

    let edges = edge_map
        .into_iter()
        .map(|((from, to), mut symbols)| {
            symbols.sort_by(|a, b| a.symbol.cmp(&b.symbol));
            ServiceEdge {
                declared: declared_set.contains(&(from.clone(), to.clone())),
                from,
                to,
                symbols,
            }
        })
        .collect();
    (edges, symbol_callers)
}

// does a call qualifier (`billing`, `crate::billing`, `self.billing`) name
// the given service? Matched on path segments, so common receivers like
// `self`/`this` never match a service by accident :)
pub(crate) fn qualifier_names_service(qualifier: &str, service: &str) -> bool {
    qualifier
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .any(|seg| !seg.is_empty() && seg == service)
}

// changed services + everything that (transitively) calls them, BFS so each
// impacted service records its shortest chain from a changed one
fn impact_closure(changed: &BTreeSet<String>, edges: &[ServiceEdge]) -> Vec<Impact> {
    let mut rev: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for e in edges {
        rev.entry(&e.to).or_default().insert(&e.from);
    }

    let mut impact: BTreeMap<String, Impact> = BTreeMap::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    for s in changed {
        impact.insert(
            s.clone(),
            Impact {
                service: s.clone(),
                reason: "changed".into(),
                path: vec![s.clone()],
            },
        );
        queue.push_back(s.clone());
    }
    while let Some(s) = queue.pop_front() {
        let Some(callers) = rev.get(s.as_str()) else { continue };
        let base_path = impact.get(&s).map(|i| i.path.clone()).unwrap_or_default();
        for caller in callers {
            if impact.contains_key(*caller) {
                continue;
            }
            let mut path = base_path.clone();
            path.push((*caller).to_string());
            impact.insert(
                (*caller).to_string(),
                Impact {
                    service: (*caller).to_string(),
                    reason: "dependency".into(),
                    path,
                },
            );
            queue.push_back((*caller).to_string());
        }
    }
    impact.into_values().collect()
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let bytes = git_bytes(root, args)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("running git (is it installed and on PATH?)")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn ref_exists(root: &Path, r: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", &format!("{r}^{{commit}}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// resolve diff base
fn resolve_base(root: &Path, base: Option<&str>) -> Result<(String, String)> {
    const AUTO: &[&str] = &["origin/main", "main", "origin/master", "master"];
    let label = match base {
        Some(b) => {
            if !ref_exists(root, b) {
                bail!(
                    "base ref '{b}' not found. In CI make sure history is fetched \
                     (actions/checkout with `fetch-depth: 0`)"
                );
            }
            b.to_string()
        }
        None => AUTO
            .iter()
            .find(|r| ref_exists(root, r))
            .map(|r| r.to_string())
            .ok_or_else(|| {
                anyhow!(
                    "no base ref found (tried {}); pass --base <ref>. In CI make \
                     sure history is fetched (actions/checkout with `fetch-depth: 0`)",
                    AUTO.join(", ")
                )
            })?,
    };
    let mb = git(root, &["merge-base", &label, "HEAD"])
        .with_context(|| format!("finding merge-base of '{label}' and HEAD"))?;
    Ok((label, mb.trim().to_string()))
}

// parse `git diff --name-status -z` output into (status, path) pairs.
fn parse_name_status(raw: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut it = raw
        .split(|&b| b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .filter(|s| !s.is_empty());
    while let Some(status) = it.next() {
        let kind = status.chars().next().unwrap_or('?');
        match kind {
            'R' | 'C' => {
                let (Some(old), Some(new)) = (it.next(), it.next()) else { break };
                out.push((status_label(kind).to_string(), new));
                if kind == 'R' {
                    out.push(("deleted".to_string(), old));
                }
            }
            _ => {
                let Some(path) = it.next() else { break };
                out.push((status_label(kind).to_string(), path));
            }
        }
    }
    out
}

fn status_label(kind: char) -> &'static str {
    match kind {
        'A' => "added",
        'M' => "modified",
        'D' => "deleted",
        'R' => "renamed",
        'C' => "copied",
        'T' => "type_changed",
        _ => "changed",
    }
}

// parse a `--unified=0` diff into per-file changed line ranges on the NEW
// pure deletions become a 1-line boundary marker.
fn parse_hunks(diff: &str) -> BTreeMap<String, Vec<(usize, usize)>> {
    let mut out: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let rest = rest.trim_end();
            current = if rest == "/dev/null" {
                None
            } else {
                let p = rest.trim_matches('"');
                Some(p.strip_prefix("b/").unwrap_or(p).to_string())
            };
        } else if line.starts_with("@@") {
            let Some(file) = &current else { continue };
            // `@@ -A[,B] +C[,D] @@ ...` - take the +C[,D] token
            let Some(plus) = line.split_whitespace().find(|t| t.starts_with('+')) else {
                continue;
            };
            let mut nums = plus[1..].splitn(2, ',');
            let Some(start) = nums.next().and_then(|n| n.parse::<usize>().ok()) else {
                continue;
            };
            let count = nums
                .next()
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(1);
            let range = if count == 0 {
                let s = start.max(1);
                (s, s) // deletion boundary
            } else {
                (start, start + count - 1)
            };
            out.entry(file.clone()).or_default().push(range);
        }
    }
    out
}


// build one matcher per service; bare pattern with no glob is a dir prefix 
fn build_matchers(services: &BTreeMap<String, Vec<String>>) -> Result<Vec<(String, GlobSet)>> {
    let mut out = Vec::new();
    for (name, patterns) in services {
        let mut b = GlobSetBuilder::new();
        for p in patterns {
            for expanded in expand_pattern(p) {
                let glob = GlobBuilder::new(&expanded)
                    .literal_separator(true)
                    .build()
                    .with_context(|| format!("service '{name}': bad glob '{p}'"))?;
                b.add(glob);
            }
        }
        let set = b
            .build()
            .with_context(|| format!("service '{name}': building glob set"))?;
        out.push((name.clone(), set));
    }
    Ok(out)
}

fn expand_pattern(p: &str) -> Vec<String> {
    let p = p.trim_end_matches('/');
    if p.contains(['*', '?', '[', '{']) {
        vec![p.to_string()]
    } else {
        vec![p.to_string(), format!("{p}/**")]
    }
}

// services whose globs match `path` (sorted; may be several for shared code)
fn assign(matchers: &[(String, GlobSet)], path: &str) -> Vec<String> {
    matchers
        .iter()
        .filter(|(_, set)| set.is_match(path))
        .map(|(name, _)| name.clone())
        .collect()
}

// test files by path conventions that I am aware of
fn is_test_path(path: &str) -> bool {
    let mut segments = path.split('/').peekable();
    while let Some(seg) = segments.next() {
        let is_last = segments.peek().is_none();
        let s = seg.to_ascii_lowercase();
        if !is_last {
            if matches!(s.as_str(), "test" | "tests" | "__tests__" | "spec" | "testdata") {
                return true;
            }
            continue;
        }
        // file name: strip the final extension, look at the stem
        let stem = s.rsplit_once('.').map(|(st, _)| st).unwrap_or(&s);
        if stem.starts_with("test_")
            || stem.ends_with("_test")
            || stem.ends_with(".test")
            || stem.ends_with(".spec")
            || stem.ends_with("_spec")
        {
            return true;
        }
    }
    false
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunk_parser_ranges_and_deletions() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,2 +12,3 @@ fn ctx()
 body
@@ -30 +40 @@
@@ -50,2 +60,0 @@
diff --git a/gone.rs b/gone.rs
--- a/gone.rs
+++ /dev/null
@@ -1,5 +0,0 @@
";
        let h = parse_hunks(diff);
        assert_eq!(h["src/a.rs"], vec![(12, 14), (40, 40), (60, 60)]);
        // deleted files have no new side
        assert!(!h.contains_key("gone.rs"));
    }

    #[test]
    fn name_status_rename_splits_old_and_new() {
        let raw = b"M\0src/a.rs\0R100\0old/b.rs\0new/b.rs\0A\0c.rs\0";
        let got = parse_name_status(raw);
        assert_eq!(
            got,
            vec![
                ("modified".to_string(), "src/a.rs".to_string()),
                ("renamed".to_string(), "new/b.rs".to_string()),
                ("deleted".to_string(), "old/b.rs".to_string()),
                ("added".to_string(), "c.rs".to_string()),
            ]
        );
    }

    #[test]
    fn bare_patterns_are_directory_prefixes() {
        let mut services = BTreeMap::new();
        services.insert("auth".to_string(), vec!["apps/auth".to_string()]);
        services.insert("all-go".to_string(), vec!["**/*.go".to_string()]);
        let m = build_matchers(&services).unwrap();
        assert_eq!(assign(&m, "apps/auth/src/login.rs"), vec!["auth"]);
        assert_eq!(assign(&m, "apps/billing/pay.go"), vec!["all-go"]);
        assert!(assign(&m, "apps/authx/no.rs").is_empty());
    }

    #[test]
    fn edge_rule_unambiguous_ambiguous_and_qualified() {
        let mut def_services: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let def = |m: &mut BTreeMap<String, BTreeSet<String>>, n: &str, s: &[&str]| {
            m.entry(n.into())
                .or_default()
                .extend(s.iter().map(|x| x.to_string()));
        };
        def(&mut def_services, "charge", &["billing"]);
        def(&mut def_services, "new", &["billing", "auth", "gateway"]);
        def(&mut def_services, "verify", &["auth", "billing"]);

        let call = |svc: &str, name: &str, q: Option<&str>| OwnedCall {
            service: svc.into(),
            file: format!("{svc}/main.rs"),
            line: 1,
            name: name.into(),
            qualifier: q.map(|s| s.into()),
        };
        let idx = idx_empty_test_called(
            def_services,
            vec![
                call("gateway", "charge", None),                // unambiguous -> edge
                call("gateway", "new", None),                   // ambiguous -> dropped
                call("gateway", "verify", None),                // ambiguous -> dropped
                call("gateway", "verify", Some("crate::auth")), // qualified -> edge
                call("billing", "charge", None),                // own symbol -> no edge
            ],
        );
        let (edges, callers) = detect_edges(&idx, &BTreeMap::new());
        let pairs: Vec<(String, String)> = edges
            .iter()
            .map(|e| (e.from.clone(), e.to.clone()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("gateway".to_string(), "auth".to_string()),
                ("gateway".to_string(), "billing".to_string()),
            ]
        );
        assert!(callers["charge"].contains("gateway"));
        assert!(!callers.contains_key("new"));
    }

    fn idx_empty_test_called(
        def_services: BTreeMap<String, BTreeSet<String>>,
        calls: Vec<OwnedCall>,
    ) -> Indexes {
        Indexes {
            def_services,
            calls,
            test_called: BTreeSet::new(),
        }
    }

    #[test]
    fn closure_walks_reverse_edges_transitively() {
        let edge = |from: &str, to: &str| ServiceEdge {
            from: from.into(),
            to: to.into(),
            declared: false,
            symbols: vec![],
        };
        // web -> api -> core; worker -> core; unrelated -> web is upstream only
        let edges = vec![edge("web", "api"), edge("api", "core"), edge("worker", "core")];
        let changed: BTreeSet<String> = ["core".to_string()].into();
        let impact = impact_closure(&changed, &edges);
        let names: Vec<&str> = impact.iter().map(|i| i.service.as_str()).collect();
        assert_eq!(names, vec!["api", "core", "web", "worker"]);
        let web = impact.iter().find(|i| i.service == "web").unwrap();
        assert_eq!(web.reason, "dependency");
        assert_eq!(web.path, vec!["core", "api", "web"]);
    }

    // I'm not aware of any other conventions around testing for typescript,
    // python or go - if there are; please create a PR to expand this coverage <3
    #[test]
    fn test_paths_by_convention() {
        for p in [
            "tests/integration.rs",
            "pkg/store/store_test.go",
            "app/test_models.py",
            "web/src/__tests__/app.tsx",
            "web/src/app.test.ts",
            "web/src/app.spec.ts",
        ] {
            assert!(is_test_path(p), "{p} should be a test path");
        }
        for p in ["src/main.rs", "app/models.py", "protest/march.rs", "attest.go"] {
            assert!(!is_test_path(p), "{p} should NOT be a test path");
        }
    }

    #[test]
    fn qualifier_segment_matching() {
        assert!(qualifier_names_service("billing", "billing"));
        assert!(qualifier_names_service("crate::billing", "billing"));
        assert!(qualifier_names_service("self.billing", "billing"));
        assert!(qualifier_names_service("&billing", "billing"));
        assert!(!qualifier_names_service("billingx", "billing"));
        assert!(!qualifier_names_service("self", "billing"));
    }

    fn run(dir: &Path, cmd: &[&str]) {
        let out = Command::new(cmd[0])
            .args(&cmd[1..])
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("running {cmd:?}: {e}"));
        assert!(
            out.status.success(),
            "{cmd:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn commit_all(dir: &Path, msg: &str) {
        run(dir, &["git", "add", "-A"]);
        run(
            dir,
            &[
                "git",
                "-c", "user.name=surf-test",
                "-c", "user.email=surf@test",
                "-c", "commit.gpgsign=false",
                "commit", "-q", "-m", msg,
            ],
        );
    }

    fn rev_head(dir: &Path) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write_files(dir: &Path, files: &[(&str, &str)]) {
        for (path, content) in files {
            let to = dir.join(path);
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::write(&to, content).unwrap();
        }
    }

    // build a throwaway repo: commit `base` files, overlay `head` files as a
    // second commit, and surf the branch against the base commit
    // NOTE: This fixture was generated by a LLM
    fn surf_fixture(base: &[(&str, &str)], head: &[(&str, &str)], tag: &str) -> SurfReport {
        let dir = std::env::temp_dir().join(format!("ccc-surf-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        write_files(&dir, base);
        run(&dir, &["git", "init", "-q"]);
        commit_all(&dir, "base");
        let base_sha = rev_head(&dir);
        write_files(&dir, head);
        commit_all(&dir, "branch work");
        let opts = SurfOptions {
            base: Some(base_sha),
            service_flags: vec![],
        };
        let report = surf(&dir, ".", &opts).unwrap_or_else(|e| panic!("{tag}: {e}"));
        let _ = fs::remove_dir_all(&dir);
        report
    }

    const PAIR_SURF_JSON: &str = r#"{ "services": { "api": ["api/**"], "lib": ["lib/**"] } }"#;

    struct PairFixture {
        lang: &'static str,
        main_fn: &'static str,
        helper: &'static str,
        base: &'static [(&'static str, &'static str)],
        head: &'static [(&'static str, &'static str)],
    }

    // Every language `ccc scan` supports has a pair fixture telling the same
    // story: api calls lib's charge; the branch makes charge call a new
    // untested fee helper. Asserting the exact report keeps the languages and
    // the implementation in lock-step.
    // NOTE: These fixtures were generated by a LLM
    #[test]
    fn surf_language_pair_fixtures() {
        const LANGS: &[PairFixture] = &[
            PairFixture {
                lang: "python",
                main_fn: "charge",
                helper: "fee",
                base: &[
                    (
                        "api/api.py",
                        "from lib.money import charge\n\n\ndef handle():\n    return charge(100)\n",
                    ),
                    (
                        "lib/money.py",
                        "def charge(cents):\n    \"\"\"Charge an amount in cents.\"\"\"\n    return cents\n",
                    ),
                    (
                        "lib/tests/test_money.py",
                        "from lib.money import charge\n\n\ndef test_charge():\n    assert charge(1) >= 1\n",
                    ),
                ],
                head: &[(
                    "lib/money.py",
                    "def charge(cents):\n    \"\"\"Charge an amount in cents, now with a fee.\"\"\"\n    return cents + fee()\n\n\ndef fee():\n    \"\"\"Flat fee.\"\"\"\n    return 30\n",
                )],
            },
            PairFixture {
                lang: "javascript",
                main_fn: "charge",
                helper: "fee",
                base: &[
                    (
                        "api/api.js",
                        "import { charge } from \"../lib/pay.js\";\n\nexport function handle() { return charge(100); }\n",
                    ),
                    (
                        "lib/pay.js",
                        "// Charge an amount in cents.\nexport function charge(cents) { return cents; }\n",
                    ),
                    (
                        "lib/pay.test.js",
                        "import { charge } from \"./pay.js\";\n\ntest(\"charge\", () => { expect(charge(1)).toBeGreaterThan(0); });\n",
                    ),
                ],
                head: &[(
                    "lib/pay.js",
                    "// Charge an amount in cents, now with a fee.\nexport function charge(cents) { return cents + fee(); }\n\n// flat fee\nfunction fee() { return 30; }\n",
                )],
            },
            PairFixture {
                lang: "typescript",
                main_fn: "charge",
                helper: "fee",
                base: &[
                    (
                        "api/api.ts",
                        "import { charge } from \"../lib/pay\";\n\nexport function handle(): number { return charge(100); }\n",
                    ),
                    (
                        "api/view.tsx",
                        "import { charge } from \"../lib/pay\";\n\nexport function Price() { return <span>{charge(100)}</span>; }\n",
                    ),
                    (
                        "lib/pay.ts",
                        "// Charge an amount in cents.\nexport function charge(cents: number): number { return cents; }\n",
                    ),
                    (
                        "lib/pay.test.ts",
                        "import { charge } from \"./pay\";\n\ntest(\"charge\", () => { expect(charge(1)).toBeGreaterThan(0); });\n",
                    ),
                ],
                head: &[(
                    "lib/pay.ts",
                    "// Charge an amount in cents, now with a fee.\nexport function charge(cents: number): number { return cents + fee(); }\n\n// flat fee\nfunction fee(): number { return 30; }\n",
                )],
            },
            PairFixture {
                lang: "go",
                main_fn: "Charge",
                helper: "fee",
                base: &[
                    (
                        "api/main.go",
                        "package main\n\nfunc handle() int { return money.Charge(100) }\n",
                    ),
                    (
                        "lib/money.go",
                        "package money\n\n// Charge an amount in cents.\nfunc Charge(cents int) int { return cents }\n",
                    ),
                    (
                        "lib/money_test.go",
                        "package money\n\nimport \"testing\"\n\nfunc TestCharge(t *testing.T) {\n\tif Charge(1) < 1 {\n\t\tt.Fail()\n\t}\n}\n",
                    ),
                ],
                head: &[(
                    "lib/money.go",
                    "package money\n\n// Charge an amount in cents, now with a fee.\nfunc Charge(cents int) int { return cents + fee() }\n\n// flat fee.\nfunc fee() int { return 30 }\n",
                )],
            },
            PairFixture {
                lang: "cpp",
                main_fn: "charge",
                helper: "fee",
                base: &[
                    (
                        "api/api.cpp",
                        "double handle() { return billing::charge(100.0); }\n",
                    ),
                    (
                        "lib/charge.cpp",
                        "namespace billing {\n// Charge an amount in cents.\ndouble charge(double cents) { return cents; }\n}\n",
                    ),
                    (
                        "lib/charge_test.cpp",
                        "bool charge_works() { return billing::charge(1.0) >= 1.0; }\n",
                    ),
                ],
                head: &[(
                    "lib/charge.cpp",
                    "namespace billing {\n// Charge an amount in cents, now with a fee.\ndouble charge(double cents) { return cents + fee(); }\n// Flat fee.\ndouble fee() { return 30.0; }\n}\n",
                )],
            },
        ];
        for fx in LANGS {
            let (lang, main_fn, helper) = (fx.lang, fx.main_fn, fx.helper);
            let mut base = vec![(".ccc/surf.json", PAIR_SURF_JSON)];
            base.extend_from_slice(fx.base);
            let report = surf_fixture(&base, fx.head, lang);

            assert_eq!(report.services_to_test, vec!["api", "lib"], "{lang}");
            let edge = report
                .edges
                .iter()
                .find(|e| e.from == "api" && e.to == "lib")
                .unwrap_or_else(|| panic!("{lang}: api -> lib edge missing"));
            assert!(
                edge.symbols.iter().any(|s| s.symbol == main_fn),
                "{lang}: edge evidence should name {main_fn}"
            );
            let main = report
                .changed_functions
                .iter()
                .find(|f| f.function == main_fn)
                .unwrap_or_else(|| panic!("{lang}: {main_fn} not in changed functions"));
            assert!(main.tested, "{lang}: {main_fn} is referenced from a test");
            assert_eq!(main.called_from, vec!["api"], "{lang}");
            let untested: Vec<&str> = report
                .untested
                .iter()
                .map(|f| f.function.as_str())
                .collect();
            assert_eq!(untested, vec![helper], "{lang}");
        }
    }

    // Three-service story: gateway calls billing's charge/refund and declares
    // a dependency on auth; the branch makes charge call a new untested fee
    // helper. Asserts the exact report end-to-end.
    // NOTE: These fixtures were also generated by a LLM
    #[test]
    fn surf_three_services_fixture() {
        const SURF_JSON: &str = r#"{
  "services": {
    "auth":    ["auth/**"],
    "billing": ["billing/**"],
    "gateway": ["gateway/**"]
  },
  "deps": { "gateway": ["auth"] }
}"#;
        let base: &[(&str, &str)] = &[
            (".ccc/surf.json", SURF_JSON),
            (
                "gateway/src/main.rs",
                "fn handle() -> u64 { charge(100) + billing::refund(5) }\n",
            ),
            (
                "auth/src/lib.rs",
                "pub fn verify_token(t: &str) -> bool { !t.is_empty() }\n",
            ),
            (
                "billing/src/charge.rs",
                "/// Charge an amount in cents.\npub fn charge(cents: u64) -> u64 { cents }\n/// Refund an amount.\npub fn refund(cents: u64) -> u64 { cents }\n",
            ),
        ];
        let head: &[(&str, &str)] = &[(
            "billing/src/charge.rs",
            "/// Charge an amount in cents, now with a fee.\npub fn charge(cents: u64) -> u64 { cents + fee() }\n/// Refund an amount.\npub fn refund(cents: u64) -> u64 { cents }\n/// Flat fee.\nfn fee() -> u64 { 30 }\n",
        )];
        let report = surf_fixture(base, head, "rust-demo");

        assert_eq!(report.services, vec!["auth", "billing", "gateway"]);

        // gateway -> billing detected with evidence; gateway -> auth declared
        assert_eq!(report.edges.len(), 2);
        let detected = report
            .edges
            .iter()
            .find(|e| e.from == "gateway" && e.to == "billing")
            .expect("gateway -> billing edge");
        assert!(!detected.declared);
        let symbols: Vec<&str> = detected.symbols.iter().map(|s| s.symbol.as_str()).collect();
        assert_eq!(symbols, vec!["charge", "refund"]);
        let declared = report
            .edges
            .iter()
            .find(|e| e.from == "gateway" && e.to == "auth")
            .expect("gateway -> auth edge");
        assert!(declared.declared);
        assert!(declared.symbols.is_empty());

        // billing changed; gateway calls it; auth calls nobody -> excluded
        assert_eq!(report.services_to_test, vec!["billing", "gateway"]);
        let gw = report
            .impact
            .iter()
            .find(|i| i.service == "gateway")
            .expect("gateway impact");
        assert_eq!(gw.reason, "dependency");
        assert_eq!(gw.path, vec!["billing", "gateway"]);

        // function granularity: charge + new fee changed, refund untouched
        let names: Vec<&str> = report
            .changed_functions
            .iter()
            .map(|f| f.function.as_str())
            .collect();
        assert_eq!(names, vec!["charge", "fee"]);
        let charge = &report.changed_functions[0];
        assert_eq!(charge.called_from, vec!["gateway"]);
        assert!(!charge.tested);
        assert_eq!(report.counts.untested, 2);
        assert!(report.unassigned_files.is_empty());
    }

    #[test]
    fn surf_end_to_end_git() {
        let dir = std::env::temp_dir().join(format!("ccc-surf-e2e-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("billing/src")).unwrap();
        fs::create_dir_all(dir.join("gateway/src")).unwrap();
        fs::create_dir_all(dir.join(".ccc")).unwrap();

        fs::write(
            dir.join(".ccc/surf.json"),
            r#"{ "services": { "billing": ["billing/**"], "gateway": ["gateway/**"] } }"#,
        )
        .unwrap();
        fs::write(
            dir.join("billing/src/charge.rs"),
            "pub fn charge(cents: u64) -> u64 { cents }\n",
        )
        .unwrap();
        fs::write(
            dir.join("gateway/src/main.rs"),
            "fn handle() -> u64 { charge(100) }\n",
        )
        .unwrap();

        run(&dir, &["git", "init", "-q"]);
        commit_all(&dir, "base");
        let base_sha = rev_head(&dir);

        // change the cross-called function on the "branch"
        fs::write(
            dir.join("billing/src/charge.rs"),
            "pub fn charge(cents: u64) -> u64 { cents + 30 }\n",
        )
        .unwrap();
        commit_all(&dir, "add fee");

        let opts = SurfOptions {
            base: Some(base_sha.clone()),
            service_flags: vec![],
        };
        let report = surf(&dir, ".", &opts).unwrap();

        assert_eq!(report.services, vec!["billing", "gateway"]);
        assert_eq!(report.services_to_test, vec!["billing", "gateway"]);
        let edge = report
            .edges
            .iter()
            .find(|e| e.from == "gateway" && e.to == "billing")
            .expect("gateway -> billing edge");
        assert!(edge.symbols.iter().any(|s| s.symbol == "charge"));
        let f = report
            .changed_functions
            .iter()
            .find(|f| f.function == "charge")
            .expect("charge should be a changed function");
        assert!(!f.tested, "no tests exist yet");
        assert_eq!(f.called_from, vec!["gateway"]);
        assert_eq!(report.untested.len(), 1);

        // add a test referencing charge - untested drains
        fs::create_dir_all(dir.join("billing/tests")).unwrap();
        fs::write(
            dir.join("billing/tests/charge_test.rs"),
            "#[test]\nfn charges() { assert_eq!(billing::charge(1), 31); }\n",
        )
        .unwrap();
        commit_all(&dir, "test charge");
        let report2 = surf(&dir, ".", &opts).unwrap();
        let f2 = report2
            .changed_functions
            .iter()
            .find(|f| f.function == "charge")
            .expect("charge still changed vs base");
        assert!(f2.tested, "test reference should mark it tested");
        assert!(report2.untested.iter().all(|f| f.function != "charge"));

        let _ = fs::remove_dir_all(&dir);
    }

    // keep the helper used (constructing Indexes without test_called noise)
    #[test]
    fn edge_rule_ignores_unknown_symbols() {
        let idx = idx_empty_test_called(
            BTreeMap::new(),
            vec![OwnedCall {
                service: "a".into(),
                file: "a/x.rs".into(),
                line: 1,
                name: "nowhere".into(),
                qualifier: None,
            }],
        );
        let (edges, callers) = detect_edges(&idx, &BTreeMap::new());
        assert!(edges.is_empty());
        assert!(callers.is_empty());
    }
}
