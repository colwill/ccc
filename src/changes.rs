//! `ccc changes` - surface branch changes to a continuous-testing suite.
//!
//! Groups source files into named services (from `.ccc/map.json` and/or
//! `--service` flags), diffs the branch against a base ref.
// ccc:skip

use crate::contracts::ContractIndex;
use crate::coverage;
use crate::extract::BDD_REGISTRARS;
use crate::model::{Boundary, FileCache};
use crate::scan;
use anyhow::{anyhow, bail, Context, Result};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SCHEMA: &str = "ccc-changes/1";
// service map config, under `.ccc/`
pub const CONFIG_NAME: &str = "map.json";
// prior config map names
pub const LEGACY_CONFIG_NAMES: &[&str] = &["surf.json"];
const MAX_EDGE_SYMBOLS: usize = 100;


#[derive(Debug, Default)]
pub struct ChangesOptions {
    // base ref to diff against; None resolves to (origin/main)
    pub base: Option<String>,
    // extra `NAME=GLOB` service definitions merged over `.ccc/map.json`
    pub service_flags: Vec<(String, String)>,
    // Diff the base against the *working tree* rather than HEAD, so
    // uncommitted edits and untracked files count as changes. CI wants the
    // committed view (the default); an engineer wants this one.
    pub worktree: bool,
    // Off by default: it walks transcripts a CI run has no use for
    pub prompts: bool,
    // Work out what this branch did to the dependency tree. `ccc changes`
    // always asks for it
    pub deps: bool,
}


// hand-maintained service map, `.ccc/map.json`, will probably add
// best guess support if low impact.
#[derive(Debug, Default, Deserialize)]
pub struct ChangesConfig {
    #[serde(default)]
    pub services: BTreeMap<String, Vec<String>>,
    // Declared relationships
    #[serde(default, alias = "deps")]
    pub relatives: BTreeMap<String, Vec<String>>,
    // peer repositories: another checkout, or a published surface. Their names
    // are service names too, so `deps` may point at them.
    #[serde(default)]
    pub externals: BTreeMap<String, crate::externals::ExternalRepo>,
    // `.proto` schemas kept outside the project
    #[serde(default)]
    pub contracts: Vec<String>,
    // what replays may do - pushing one to a remote anyone can read is allowed
    // here, in the repository, where the whole team sees it, or not at all
    #[serde(default)]
    pub replays: ReplaysConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct ReplaysConfig {
    // push replays to a remote that answers without signing in - for a
    // company's own instance where every repository is readable company-wide.
    // A replay carries prompts and code, so this is never on by default.
    #[serde(default)]
    pub allow_public_remote: bool,
    // encrypt every replay to a runccc project's key, so only its team opens
    // one - wherever it is pushed. Set here, never on one machine, so no
    // teammate's ccc saves a replay in the clear.
    #[serde(default)]
    pub encrypt: Option<EncryptConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EncryptConfig {
    // the runccc project replays are sealed to - an id, not a secret
    pub project: String,
    // the key service's address - runccc's own when left out; `RUNCCC_URL` overrides both
    #[serde(default)]
    pub service: Option<String>,
}

impl ChangesConfig {
    pub fn load(root: &Path) -> Result<ChangesConfig> {
        let Some(path) = Self::path(root) else {
            return Ok(ChangesConfig::default());
        };
        let raw =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        warn_legacy_deps_key(&path, &raw);
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    // the config actually in use, current name first
    pub fn path(root: &Path) -> Option<PathBuf> {
        std::iter::once(&CONFIG_NAME)
            .chain(LEGACY_CONFIG_NAMES)
            .map(|n| root.join(".ccc").join(n))
            .find(|p| p.is_file())
    }
}


// `relatives` was called `deps` back when it could only name a service
fn warn_legacy_deps_key(path: &Path, raw: &str) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    let uses_deps = serde_json::from_str::<Value>(raw)
        .ok()
        .is_some_and(|v| v.get("deps").is_some());
    if !uses_deps {
        return;
    }
    WARNED.call_once(|| {
        eprintln!(
            "warning: {} uses `deps`, which is now `relatives` - it may name a peer \
             repository from `externals`, not only a service in this repo. `deps` is \
             still read and will be removed in 2.0.0.",
            path.display()
        );
    });
}

#[derive(Debug, Serialize, Clone)]
pub struct ChangedFile {
    pub path: String,
    // added | modified | deleted | renamed | copied | type_changed
    pub status: String,
    pub services: Vec<String>,
    // changed relative to HEAD as well as to the base: not committed yet
    pub uncommitted: bool,
    // the requests sent to claude/copilot that produced this file's changes,
    // strongest evidence first. Only populated with `prompts`
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prompted_by: Vec<crate::prompts::PromptRef>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ChangedFunction {
    pub services: Vec<String>,
    pub file: String,
    pub function: String,
    pub lines: [usize; 2],
    pub tested: bool,
    // named test functions that cover this one, capped at MAX_TESTED_BY. Empty
    // while `tested` is true means the only test references came from code
    // outside any named test function (e.g. file-level setup).
    pub tested_by: Vec<String>,
    // the same tests, addressed rather than named: where each one is defined
    // and what tied it to this function. A name alone cannot be joined back to
    // a test without guessing, which is how a reader ends up looking at a
    // same-named test in another language.
    pub tested_by_sites: Vec<TestedBySite>,
    pub called_from: Vec<String>,
    // the requests behind this function's changes: the same references the
    // file carries, narrowed to the ones whose evidence reaches this span
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prompted_by: Vec<crate::prompts::PromptRef>,
}

#[derive(Debug, Serialize, Clone)]
pub struct TestedBySite {
    pub test: String,
    pub file: String,
    pub line: usize,
    pub language: String,
    // receiver-type | same-file | import | qualifier | name-only, weakest last
    pub evidence: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct EdgeSymbol {
    pub symbol: String,
    pub file: String,
    pub line: usize,
    // how the target was established: receiver-type | qualifier | project |
    // import | type-reference | name-only. Anything but `name-only` is
    // positive evidence; `name-only` is the untyped-language fallback.
    pub via: String,
    // call | type
    pub kind: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct ServiceEdge {
    pub from: String,
    pub to: String,
    // listed in `.ccc/map.json` `deps`
    pub declared: bool,
    // resolution found evidence for this edge too. Independent of `declared`:
    // declaring a dependency never skips the analysis, so an edge can be both,
    // and a declared edge with `detected: false` means the analysis ran and
    // found nothing - the expected shape for an HTTP/RPC/queue link.
    pub detected: bool,
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
pub struct ChangesCounts {
    pub services_to_test: usize,
    pub changed_files: usize,
    pub changed_functions: usize,
    pub untested: usize,
    pub unresolved_calls: usize,
    // calls from test code that leave the project (`std::fs::write`) and so
    // cover nothing here. Reported rather than dropped in silence: it is the
    // difference between "no test covers this" and "the analyser discarded the
    // evidence", and the two want different reactions.
    pub external_test_calls: usize,
    pub externals: usize,
    pub crossings: usize,
    // crossings whose key nothing answers: a typo, or a peer not configured
    pub unmatched_crossings: usize,
    // changed functions a request accounts for
    pub prompted_functions: usize,
    pub unattributed_files: usize,
    // telemetric definitions this branch moved
    pub telemetry_changes: usize,
    pub telemetry_breaking: usize,
    // lines the branch adds that look like a credential
    pub secrets: usize,
}

#[derive(Debug, Serialize)]
pub struct ChangesReport {
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
    // calls that matched a definition in another service but carried no
    // evidence naming it; surfaced so a missing edge can be explained
    pub unresolved_calls: Vec<UnresolvedCall>,
    // peer repositories named in `.ccc/map.json`, and whether each resolved
    pub externals: Vec<Value>,
    // `ccc:calls` / `ccc:serves` pairs, including the ones that leave the repo
    pub crossings: Vec<Value>,
    // requests sent to an agent
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<crate::prompts::Turn>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unattributed: Vec<String>,
    // What this branch did to the dependency tree
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deps: Option<crate::deps::DepsReport>,
    // what this branch did to the OpenTelemetry metrics
    pub telemetry: crate::telemetry::TelemetryReport,
    // what this branch adds that looks like a credential - said before it is pushed
    pub secrets: crate::secrets::Report,
    pub counts: ChangesCounts,
}

// Analyze `root` and build the changes report, parsing the tree first.
pub fn changes(root: &Path, root_label: &str, opts: &ChangesOptions) -> Result<ChangesReport> {
    let files = scan::collect_files(root)?;
    let caches = scan::build_caches(root, &files);
    changes_with_caches(root, root_label, opts, &caches)
}

// Same analysis against an already-parsed map, so a caller that holds one
// (`ccc run`) does not pay to walk and re-parse the tree.
pub fn changes_with_caches(
    root: &Path,
    root_label: &str,
    opts: &ChangesOptions,
    caches: &[FileCache],
) -> Result<ChangesReport> {
    let mut config = ChangesConfig::load(root)?;
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
    // A peer repository is a service too - it owns no files here, but
    // `relatives` may name it and edges may end at it.
    for (from, tos) in &config.relatives {
        for t in std::iter::once(from).chain(tos) {
            if config.services.contains_key(t) || config.externals.contains_key(t) {
                continue;
            }
            let mut known: Vec<String> = config.services.keys().cloned().collect();
            known.extend(config.externals.keys().cloned());
            bail!(
                "map.json relatives mention unknown service '{t}' \
                 (known: {})",
                known.join(", ")
            );
        }
    }
    for name in config.externals.keys() {
        if config.services.contains_key(name) {
            bail!(
                "map.json names '{name}' as both a service and an external; \
                 a name is either code in this repo or code in another one"
            );
        }
    }
    let matchers = build_matchers(&config.services)?;
    let service_names: Vec<String> = config.services.keys().cloned().collect();
    // the rpcs every schema declares, and the code in any language tied to them
    let schemas = crate::contracts::load_schemas(root, &config.contracts);
    let contracts = ContractIndex::build(caches, &schemas);
    let externals = crate::externals::resolve_all(root, &config.externals, &contracts.schemas);

    let (base_label, base_sha) = resolve_base(root, opts.base.as_deref())?;
    let head_sha = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    // With `worktree`, omitting the second ref makes git diff the base against
    // the working tree, so staged and unstaged edits are included.
    let mut diff_refs: Vec<&str> = vec![&base_sha];
    if !opts.worktree {
        diff_refs.push("HEAD");
    }
    let mut name_status_args = vec!["diff", "--relative", "--name-status", "-z", "-M"];
    name_status_args.extend(&diff_refs);
    let statuses = parse_name_status(&git_bytes(root, &name_status_args)?);
    let mut hunk_args = vec!["diff", "--relative", "--unified=0", "-M"];
    hunk_args.extend(&diff_refs);
    let mut hunks = parse_hunks(&git(root, &hunk_args)?);

    // Files not yet known to git are changes too - a brand new source file is
    // the most likely thing to need a test. `git diff` cannot see them.
    let mut statuses = statuses;
    let mut untracked: BTreeSet<String> = BTreeSet::new();
    if opts.worktree {
        for path in git_bytes(root, &["ls-files", "--others", "--exclude-standard", "-z"])?
            .split(|&b| b == 0)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .filter(|s| !s.is_empty())
        {
            // the whole file is new, so every function in it is changed
            hunks.entry(path.clone()).or_insert_with(|| vec![(1, usize::MAX)]);
            statuses.push(("added".to_string(), path.clone()));
            untracked.insert(path);
        }
    }
    // which of those changes are not committed yet
    let uncommitted: BTreeSet<String> = if opts.worktree {
        let mut set = untracked.clone();
        for (_, p) in parse_name_status(&git_bytes(
            root,
            &["diff", "--relative", "--name-status", "-z", "-M", "HEAD"],
        )?) {
            set.insert(p);
        }
        set
    } else {
        BTreeSet::new()
    };

    // What the branch did to the dependency tree
    let deps = opts.deps.then(|| {
        crate::deps::analyse(
            root,
            &crate::deps::DepsOptions {
                base_sha: &base_sha,
                head_sha: &head_sha,
                worktree: opts.worktree,
                touched: &statuses,
                offline: false,
            },
        )
    });

    // What the branch did to the metrics the source emits
    let telemetry = crate::telemetry::analyse(
        root,
        &crate::telemetry::TelemetryOptions {
            base_sha: &base_sha,
            head_sha: &head_sha,
            worktree: opts.worktree,
        },
    );

    // What the branch adds that looks like a credential, while it can still be taken out
    let secrets = crate::secrets::against(root, &base_label, &base_sha, opts.worktree);

    // Which request produced which change
    let (turns, attribution) = if opts.prompts {
        let (turns, _) = crate::prompts::collect(root, &crate::prompts::PromptsOptions::default());
        let times = write_times(root, &base_sha, &hunks, &uncommitted);
        let attribution = crate::prompts::attribute(root, &turns, &hunks, &times);
        (turns, attribution)
    } else {
        (Vec::new(), crate::prompts::Attribution::default())
    };

    let idx = build_indexes(root, caches, &matchers);
    // Which tests reach which definitions. One relation, shared with
    // `insights`, so the two reports cannot disagree about what is covered.
    let project_ids: BTreeSet<String> =
        manifest_identities(root).into_iter().map(|(id, _)| id).collect();
    let cov = coverage::build(caches, &project_ids, &contracts);

    // cross-service edges + per-symbol caller map
    let (mut edges, mut symbol_callers, mut unresolved_calls) = detect_edges(&idx, &config.relatives);
    merge_rpc_edges(
        &mut edges,
        &mut symbol_callers,
        &mut unresolved_calls,
        caches,
        &matchers,
        &contracts,
    );

    // boundary crossings: the calls that leave the process, which the call
    // graph cannot see - named by an author, or derived from a schema
    let crossings = detect_crossings(caches, &matchers, &externals, &contracts);
    merge_crossings(&mut edges, &crossings);

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
            uncommitted: uncommitted.contains(path),
            prompted_by: attribution.by_file.get(path).cloned().unwrap_or_default(),
        });
    }
    changed_files.sort_by(|a, b| a.path.cmp(&b.path));
    changed_files.dedup_by(|a, b| a.path == b.path && a.status == b.status);

    // changed functions: hunk ranges vs function spans
    let mut changed_functions = Vec::new();
    for (fi, cache) in caches.iter().enumerate() {
        let rel = path_str(&cache.rel_path);
        let Some(ranges) = hunks.get(&rel) else { continue };
        let services = assign(&matchers, &rel);
        let file_is_test = is_test_path(&rel);
        for (ki, f) in cache.funcs.iter().enumerate() {
            let touched = ranges
                .iter()
                .any(|&(s, e)| s <= f.end_line && f.start_line <= e);
            if !touched {
                continue;
            }
            let is_test_code = file_is_test || f.test_ctx;
            // Coverage is looked up by *definition*, not by name: two functions
            // sharing a name - in another module, or another language - are two
            // different things, and only one of them is the one that changed.
            let covering = cov.covering((fi, ki));
            let tested = is_test_code || cov.is_covered((fi, ki));
            let tested_by: Vec<String> =
                covering.iter().map(|r| r.site.name.clone()).collect();
            let tested_by_sites: Vec<TestedBySite> = covering
                .iter()
                .map(|r| TestedBySite {
                    test: r.site.name.clone(),
                    file: r.site.file.clone(),
                    line: r.site.line,
                    language: r.site.language.to_string(),
                    evidence: r.evidence.label().to_string(),
                })
                .collect();
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
            // A file-wide reference explains every span in it
            let prompted_by: Vec<crate::prompts::PromptRef> = attribution
                .by_file
                .get(&rel)
                .map(|refs| {
                    refs.iter()
                        .filter(|r| r.covers(f.start_line, f.end_line))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            changed_functions.push(ChangedFunction {
                services: services.clone(),
                file: rel.clone(),
                function: f.name.clone(),
                lines: [f.start_line, f.end_line],
                prompted_by,
                tested,
                tested_by,
                tested_by_sites,
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

    let unmatched_crossings = crossings.iter().filter(|c| c.remote.is_none()).count();
    let counts = ChangesCounts {
        services_to_test: services_to_test.len(),
        changed_files: changed_files.len(),
        changed_functions: changed_functions.len(),
        untested: untested.len(),
        unresolved_calls: unresolved_calls.len(),
        external_test_calls: cov.external_calls(),
        externals: externals.len(),
        crossings: crossings.len(),
        unmatched_crossings,
        prompted_functions: changed_functions
            .iter()
            .filter(|f| !f.prompted_by.is_empty())
            .count(),
        unattributed_files: attribution.unattributed.len(),
        telemetry_changes: telemetry.changes.len(),
        telemetry_breaking: telemetry.counts.breaking,
        secrets: secrets.findings.len(),
    };

    Ok(ChangesReport {
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
        unresolved_calls,
        externals: externals.iter().map(|e| e.json()).collect(),
        crossings: crossings.iter().map(crossing_json).collect(),
        turns,
        unattributed: attribution.unattributed,
        deps,
        telemetry,
        secrets,
        counts,
    })
}

// The dependency delta on its own
pub fn deps_report(
    root: &Path,
    base: Option<&str>,
    worktree: bool,
) -> Result<(String, crate::deps::DepsReport)> {
    let (base_label, base_sha) = resolve_base(root, base)?;
    let head_sha = git(root, &["rev-parse", "HEAD"])?.trim().to_string();
    let mut diff_refs: Vec<&str> = vec![&base_sha];
    if !worktree {
        diff_refs.push("HEAD");
    }
    let mut args = vec!["diff", "--relative", "--name-status", "-z", "-M"];
    args.extend(&diff_refs);
    let mut touched = parse_name_status(&git_bytes(root, &args)?);
    // a lockfile git has never seen is still a lockfile this branch added
    if worktree {
        for path in git_bytes(root, &["ls-files", "--others", "--exclude-standard", "-z"])?
            .split(|&b| b == 0)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .filter(|s| !s.is_empty())
        {
            touched.push(("added".to_string(), path));
        }
    }
    let report = crate::deps::analyse(
        root,
        &crate::deps::DepsOptions {
            base_sha: &base_sha,
            head_sha: &head_sha,
            worktree,
            touched: &touched,
            offline: false,
        },
    );
    Ok((base_label, report))
}

fn crossing_json(c: &crate::externals::Crossing) -> Value {
    serde_json::json!({
        "key": c.key,
        "transport": c.transport,
        "from": c.from,
        "to": c.to,
        "file": c.file,
        "line": c.line,
        "function": c.function,
        "external": c.external,
        // `rpc` when a schema tied it, `annotation` when a `ccc:` comment did
        "via": if c.rpc { crate::contracts::VIA } else { "annotation" },
        // absent when nothing answers this key
        "remote": c.remote.as_ref().map(|r| serde_json::json!({
            "function": r.function,
            "file": r.file,
            "line": r.line,
            "service": r.service,
        })),
    })
}

// scaffold a starter `.ccc/map.json`
pub fn init_config(root: &Path) -> Result<PathBuf> {
    if let Some(existing) = ChangesConfig::path(root) {
        bail!(
            "{} already exists; edit or remove it first",
            existing.display()
        );
    }
    let path = root.join(".ccc").join(CONFIG_NAME);
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
    let out = serde_json::json!({ "services": services, "relatives": {} });
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
    // declared type of the receiver, when the extractor could resolve one
    recv_type: Option<String>,
    // the language declares types the syntax tree can read, so this call is
    // resolved by evidence only - never by a bare name match
    typed: bool,
}

// a project-defined type named in a signature: a dependency the call map
// cannot see, because using a type is not calling a function
struct TypeRef {
    service: String,
    file: String,
    line: usize,
    type_name: String,
}

// a call that matched a definition somewhere but could not be attributed to
// one service with evidence; reported so exclusions are auditable
#[derive(Debug, Serialize, Clone)]
pub struct UnresolvedCall {
    pub symbol: String,
    pub file: String,
    pub line: usize,
    pub from: String,
    // ambiguous | no-evidence
    pub reason: String,
    pub candidates: Vec<String>,
}

struct Indexes {
    def_services: BTreeMap<String, BTreeSet<String>>,
    // (owning type, method) -> services. The precise address of a method, so a
    // call through a typed receiver resolves without guessing.
    method_services: BTreeMap<(String, String), BTreeSet<String>>,
    // type name -> services defining it
    type_services: BTreeMap<String, BTreeSet<String>>,
    // module identity a qualifier can name: a go `package`, a c++ `namespace`,
    // a rust `mod`, a file stem, a facade directory
    module_services: BTreeMap<String, BTreeSet<String>>,
    // project identity from a manifest (crate name, go module path, npm name)
    project_services: BTreeMap<String, BTreeSet<String>>,
    // file -> imported name -> services that name was imported from
    imports: BTreeMap<String, BTreeMap<String, BTreeSet<String>>>,
    wildcard_imports: BTreeMap<String, BTreeSet<String>>,
    calls: Vec<OwnedCall>,
    type_refs: Vec<TypeRef>,
}

fn build_indexes(
    root: &Path,
    caches: &[FileCache],
    matchers: &[(String, GlobSet)],
) -> Indexes {
    let mut idx = Indexes {
        def_services: BTreeMap::new(),
        method_services: BTreeMap::new(),
        type_services: BTreeMap::new(),
        module_services: BTreeMap::new(),
        project_services: project_identities(root, matchers),
        imports: BTreeMap::new(),
        calls: Vec::new(),
        type_refs: Vec::new(),
        wildcard_imports: BTreeMap::new(),
    };

    // pass 1: definitions, types and module identities
    for cache in caches {
        let rel = path_str(&cache.rel_path);
        let services = assign(matchers, &rel);
        for s in &services {
            for f in &cache.funcs {
                idx.def_services.entry(f.name.clone()).or_default().insert(s.clone());
                if let Some(owner) = &f.owner {
                    idx.method_services
                        .entry((owner.clone(), f.name.clone()))
                        .or_default()
                        .insert(s.clone());
                }
            }
            for t in &cache.types {
                idx.type_services.entry(t.name.clone()).or_default().insert(s.clone());
                // a type name is also a qualifier (`Client::new`)
                idx.module_services.entry(t.name.clone()).or_default().insert(s.clone());
            }
            for m in &cache.modules {
                idx.module_services.entry(m.clone()).or_default().insert(s.clone());
            }
            // the file stem, and the directory a facade file stands for
            if let Some(stem) = cache.rel_path.file_stem().and_then(|x| x.to_str()) {
                idx.module_services.entry(stem.to_string()).or_default().insert(s.clone());
                if matches!(stem, "__init__" | "index" | "mod" | "lib") {
                    if let Some(dir) = cache
                        .rel_path
                        .parent()
                        .and_then(|p| p.file_name())
                        .and_then(|d| d.to_str())
                    {
                        idx.module_services.entry(dir.to_string()).or_default().insert(s.clone());
                    }
                }
            }
        }
    }

    // pass 2: imports, calls, type references
    for cache in caches {
        let rel = path_str(&cache.rel_path);
        let services = assign(matchers, &rel);
        let typed = cache.language.is_typed();

        // which services each imported name could have come from
        let mut wildcard: BTreeSet<String> = BTreeSet::new();
        let bound = idx.imports.entry(rel.clone()).or_default();
        for imp in &cache.imports {
            let mut from: BTreeSet<String> = BTreeSet::new();
            for seg in module_segments(&imp.module) {
                if let Some(svcs) = idx.module_services.get(seg) {
                    from.extend(svcs.iter().cloned());
                }
            }
            for (id, svcs) in &idx.project_services {
                if names_project(&imp.module, id) {
                    from.extend(svcs.iter().cloned());
                }
            }
            // `use crate::{scan, render}` binds names that are themselves modules
            for n in &imp.names {
                if let Some(svcs) = idx.module_services.get(n.as_str()) {
                    from.extend(svcs.iter().cloned());
                }
            }
            if from.is_empty() {
                continue;
            }
            if imp.names.is_empty() {
                wildcard.extend(from.iter().cloned());
            }
            for n in &imp.names {
                bound.entry(n.clone()).or_default().extend(from.iter().cloned());
            }
        }

        if !wildcard.is_empty() {
            idx.wildcard_imports.entry(rel.clone()).or_default().extend(wildcard);
        }

        for c in &cache.calls {
            for s in &services {
                idx.calls.push(OwnedCall {
                    service: s.clone(),
                    file: rel.clone(),
                    line: c.line,
                    name: c.name.clone(),
                    qualifier: c.qualifier.clone(),
                    recv_type: c.recv_type.clone(),
                    typed,
                });
            }
        }

        // a signature that mentions another service's type depends on it even
        // if it never calls into it
        if typed {
            for f in &cache.funcs {
                let mentioned = f
                    .param_types
                    .iter()
                    .cloned()
                    .chain(f.ret.iter().map(|r| crate::extract::normalize_type(r)))
                    .chain(f.owner.iter().cloned());
                for t in mentioned.filter(|t| !t.is_empty()) {
                    for s in &services {
                        idx.type_refs.push(TypeRef {
                            service: s.clone(),
                            file: rel.clone(),
                            line: f.line,
                            type_name: t.clone(),
                        });
                    }
                }
            }
        }
    }
    idx
}

// identifier-ish segments of an import path
pub(crate) fn module_segments(module: &str) -> impl Iterator<Item = &str> {
    module
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .filter(|s| !s.is_empty())
}

// Does an import path name a project? Matches a go module path by prefix
// (`github.com/acme/billing/pkg/money` under `github.com/acme/billing`) and a
// crate / npm package by its first segment.
pub(crate) fn names_project(module: &str, id: &str) -> bool {
    if module == id || module.starts_with(&format!("{id}/")) {
        return true;
    }
    // crate names appear underscored in code (`my-crate` -> `my_crate`)
    let normalized = id.replace('-', "_");
    module_segments(module).any(|seg| seg == id || seg == normalized)
}

// Project identities declared by manifests, mapped to the services that own
// them. This is what makes a cross-*project* import resolvable: the name in
// the import statement is not a path in this repo, it is a package name.
fn project_identities(
    root: &Path,
    matchers: &[(String, GlobSet)],
) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (id, rel_dir) in manifest_identities(root) {
        // the manifest's directory decides which services own the project
        let probe = if rel_dir.is_empty() {
            "src/lib".to_string()
        } else {
            format!("{rel_dir}/src/lib")
        };
        let services = assign(matchers, &probe);
        let services = if services.is_empty() {
            assign(matchers, &format!("{rel_dir}/x"))
        } else {
            services
        };
        if !services.is_empty() {
            out.entry(id).or_default().extend(services);
        }
    }
    out
}

// Every project identity a manifest declares, with the directory it governs.
// `coverage` wants the names alone - a qualifier naming the crate is a call
// staying inside the project - while `project_identities` maps them to
// services, so the walk is shared rather than written twice.
pub(crate) fn manifest_identities(root: &Path) -> Vec<(String, String)> {
    const MANIFESTS: &[&str] = &["Cargo.toml", "go.mod", "package.json"];
    const SKIP: &[&str] = &[
        ".git", "target", "node_modules", "dist", "build", "out", "vendor", ".ccc",
    ];
    let mut out: Vec<(String, String)> = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    // bounded walk: manifests live near the top of a tree, not deep inside it
    let mut budget = 2_000usize;
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if budget == 0 {
                return out;
            }
            budget -= 1;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !name.starts_with('.') && !SKIP.contains(&name.as_str()) {
                    dirs.push(path);
                }
                continue;
            }
            if !MANIFESTS.contains(&name.as_str()) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            let Some(id) = manifest_identity(&name, &text) else { continue };
            let rel_dir = path
                .parent()
                .and_then(|p| p.strip_prefix(root).ok())
                .map(path_str)
                .unwrap_or_default();
            out.push((id, rel_dir));
        }
    }
    out
}

// package name from a manifest, parsed narrowly enough not to need a toml or
// json dependency
fn manifest_identity(file: &str, text: &str) -> Option<String> {
    match file {
        "go.mod" => text
            .lines()
            .map(str::trim)
            .find_map(|l| l.strip_prefix("module "))
            .map(|m| m.trim().to_string()),
        "Cargo.toml" => {
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
        "package.json" => {
            let v: Value = serde_json::from_str(text).ok()?;
            v.get("name")?.as_str().map(str::to_string)
        }
        _ => None,
    }
}

// How a cross-service edge was established. Ordered strongest first: a typed
// receiver is the only one that identifies a target without any inference.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Via {
    // an author wrote `ccc:calls` - a stated fact, not an inference, and the
    // only evidence that can cross a process boundary
    Annotation,
    ReceiverType,
    Qualifier,
    Import,
    Project,
    TypeReference,
    NameOnly,
}

impl Via {
    fn label(self) -> &'static str {
        match self {
            Via::Annotation => "annotation",
            Via::ReceiverType => "receiver-type",
            Via::Qualifier => "qualifier",
            Via::Import => "import",
            Via::Project => "project",
            Via::TypeReference => "type-reference",
            Via::NameOnly => "name-only",
        }
    }
}

// Attribute one call to a target service, with the evidence that did it.
//
// For a typed language every route requires positive evidence: the receiver's
// declared type, a qualifier naming the target's module/type/service/project,
// or an import binding the callee. A bare name that merely happens to be
// defined in exactly one other service is *not* evidence - that rule invented
// edges for stdlib method names (`.ok()`, `.parse()`) that collided with
// project functions. Untyped languages keep it as a last resort, flagged
// `name-only`, because they have nothing better.
fn resolve_call(call: &OwnedCall, idx: &Indexes) -> Result<(String, Via), (String, Vec<String>)> {
    let definers = idx.def_services.get(&call.name);
    let others = |set: &BTreeSet<String>| -> Vec<String> {
        set.iter().filter(|s| **s != call.service).cloned().collect()
    };

    // the receiver's declared type addresses the method exactly
    if let Some(ty) = &call.recv_type {
        if let Some(svcs) = idx.method_services.get(&(ty.clone(), call.name.clone())) {
            let cands = others(svcs);
            match cands.len() {
                1 => return Ok((cands[0].clone(), Via::ReceiverType)),
                0 => {}
                _ => return Err(("ambiguous".into(), cands)),
            }
        }
    }

    let Some(definers) = definers else {
        return Err(("undefined".into(), Vec::new()));
    };
    let defined_elsewhere: BTreeSet<String> = others(definers).into_iter().collect();
    if defined_elsewhere.is_empty() {
        return Err(("local".into(), Vec::new()));
    }

    // a qualifier naming the target's module, type, service or project
    if let Some(q) = call.qualifier.as_deref() {
        let mut named: BTreeSet<String> = BTreeSet::new();
        for seg in module_segments(q) {
            if let Some(svcs) = idx.module_services.get(seg) {
                named.extend(svcs.iter().cloned());
            }
            for s in &defined_elsewhere {
                if qualifier_names_service(seg, s) {
                    named.insert(s.clone());
                }
            }
        }
        let mut via = Via::Qualifier;
        for (id, svcs) in &idx.project_services {
            if names_project(q, id) {
                named.extend(svcs.iter().cloned());
                via = Via::Project;
            }
        }
        let cands: Vec<String> = defined_elsewhere.intersection(&named).cloned().collect();
        match cands.len() {
            1 => return Ok((cands[0].clone(), via)),
            0 => {}
            _ => return Err(("ambiguous".into(), cands)),
        }
    }

    // the callee was imported from the target
    if let Some(from) = idx.imports.get(&call.file).and_then(|m| m.get(&call.name)) {
        let cands: Vec<String> = defined_elsewhere.intersection(from).cloned().collect();
        match cands.len() {
            1 => return Ok((cands[0].clone(), Via::Import)),
            0 => {}
            _ => return Err(("ambiguous".into(), cands)),
        }
    }

    // the whole of a target was made available, and it defines the callee
    if let Some(from) = idx.wildcard_imports.get(&call.file) {
        let cands: Vec<String> = defined_elsewhere.intersection(from).cloned().collect();
        match cands.len() {
            1 => return Ok((cands[0].clone(), Via::Import)),
            0 => {}
            _ => return Err(("ambiguous".into(), cands)),
        }
    }

    // untyped languages only: a single definer, on the name alone
    if !call.typed && defined_elsewhere.len() == 1 {
        return Ok((
            defined_elsewhere.iter().next().unwrap().clone(),
            Via::NameOnly,
        ));
    }
    Err((
        if defined_elsewhere.len() > 1 { "ambiguous" } else { "no-evidence" }.into(),
        defined_elsewhere.into_iter().collect(),
    ))
}

// Detect cross-service edges, then overlay declared deps.
fn detect_edges(
    idx: &Indexes,
    declared: &BTreeMap<String, Vec<String>>,
) -> (
    Vec<ServiceEdge>,
    BTreeMap<String, BTreeSet<String>>,
    Vec<UnresolvedCall>,
) {
    let mut edge_map: BTreeMap<(String, String), Vec<EdgeSymbol>> = BTreeMap::new();
    let mut symbol_callers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut unresolved: Vec<UnresolvedCall> = Vec::new();
    let mut seen_unresolved: BTreeSet<(String, String)> = BTreeSet::new();

    let record = |from: &str, to: &str, sym: EdgeSymbol, map: &mut BTreeMap<_, Vec<EdgeSymbol>>| {
        let symbols: &mut Vec<EdgeSymbol> = map
            .entry((from.to_string(), to.to_string()))
            .or_default();
        if let Some(existing) = symbols.iter_mut().find(|s| s.symbol == sym.symbol) {
            // keep the strongest evidence we saw for this symbol
            if via_rank(&sym.via) < via_rank(&existing.via) {
                *existing = sym;
            }
        } else if symbols.len() < MAX_EDGE_SYMBOLS {
            symbols.push(sym);
        }
    };

    for call in &idx.calls {
        match resolve_call(call, idx) {
            Ok((target, via)) => {
                symbol_callers
                    .entry(call.name.clone())
                    .or_default()
                    .insert(call.service.clone());
                record(
                    &call.service,
                    &target,
                    EdgeSymbol {
                        symbol: call.name.clone(),
                        file: call.file.clone(),
                        line: call.line,
                        via: via.label().to_string(),
                        kind: "call".to_string(),
                    },
                    &mut edge_map,
                );
            }
            Err((reason, candidates)) => {
                // `local` and `undefined` are not project dependencies at all
                if !matches!(reason.as_str(), "ambiguous" | "no-evidence") {
                    continue;
                }
                let key = (call.service.clone(), call.name.clone());
                if seen_unresolved.insert(key) && unresolved.len() < MAX_EDGE_SYMBOLS {
                    unresolved.push(UnresolvedCall {
                        symbol: call.name.clone(),
                        file: call.file.clone(),
                        line: call.line,
                        from: call.service.clone(),
                        reason,
                        candidates,
                    });
                }
            }
        }
    }

    // using another service's type is a dependency even with no call
    for t in &idx.type_refs {
        let Some(svcs) = idx.type_services.get(&t.type_name) else { continue };
        let targets: Vec<&String> = svcs.iter().filter(|s| **s != t.service).collect();
        if let [target] = targets[..] {
            record(
                &t.service,
                target,
                EdgeSymbol {
                    symbol: t.type_name.clone(),
                    file: t.file.clone(),
                    line: t.line,
                    via: Via::TypeReference.label().to_string(),
                    kind: "type".to_string(),
                },
                &mut edge_map,
            );
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
                detected: !symbols.is_empty(),
                from,
                to,
                symbols,
            }
        })
        .collect();
    unresolved.sort_by(|a, b| (&a.from, &a.symbol).cmp(&(&b.from, &b.symbol)));
    (edges, symbol_callers, unresolved)
}

// Join `ccc:calls` here to `ccc:serves` there, on the key both sides wrote.
//
// Three shapes fall out of one rule. Both ends in this repo but in different
// services is a local HTTP or queue hop. One end here and one in a peer's
// surface is a cross-repository call, and it does not matter whether that peer
// is a sibling checkout, another corner of a monorepo, or a published surface
// from a repo in a language we cannot even parse.
pub(crate) fn detect_crossings(
    caches: &[FileCache],
    matchers: &[(String, GlobSet)],
    externals: &[crate::externals::ExternalService],
    contracts: &ContractIndex,
) -> Vec<crate::externals::Crossing> {
    use crate::externals::{norm_key, Crossing, Endpoint};

    // every boundary endpoint here
    let mut local: Vec<(Boundary, Endpoint)> = Vec::new();
    for cache in caches {
        let file = path_str(&cache.rel_path);
        for ann in &cache.annotations {
            let endpoint = Endpoint {
                key: ann.key.clone(),
                transport: ann.transport.clone(),
                function: ann.function.clone(),
                file: file.clone(),
                line: ann.line,
                service: None,
                via: None,
            };
            local.push((ann.boundary, endpoint));
        }
    }
    let (rpc_provides, rpc_consumes) = contracts.endpoints(caches);
    local.extend(rpc_provides.into_iter().map(|e| (Boundary::Serves, e)));
    local.extend(rpc_consumes.into_iter().map(|e| (Boundary::Calls, e)));

    // every handler this repo publishes, by key
    let mut local_handlers: BTreeMap<String, Vec<(String, Endpoint)>> = BTreeMap::new();
    for (boundary, endpoint) in &local {
        if *boundary != Boundary::Serves {
            continue;
        }
        let services = assign(matchers, &endpoint.file);
        let mut endpoint = endpoint.clone();
        endpoint.service = services.first().cloned();
        let owners = if services.is_empty() { vec![String::new()] } else { services };
        for service in owners {
            local_handlers
                .entry(norm_key(&endpoint.key))
                .or_default()
                .push((service, endpoint.clone()));
        }
    }

    let mut out = Vec::new();

    // outbound: a call here, matched against peers first, then against this
    // repo's own handlers
    for (boundary, call) in &local {
        if *boundary != Boundary::Calls {
            continue;
        }
        let rpc = call.via.is_some();
        let key = norm_key(&call.key);
        let from = assign(matchers, &call.file).first().cloned().unwrap_or_default();

        let mut matched = false;
        for external in externals {
            let Some(surface) = &external.surface else {
                continue;
            };
            for endpoint in surface.provides.iter().filter(|e| norm_key(&e.key) == key) {
                matched = true;
                out.push(Crossing {
                    key: call.key.clone(),
                    transport: pick_transport(&call.transport, &endpoint.transport),
                    from: from.clone(),
                    to: external.name.clone(),
                    file: call.file.clone(),
                    line: call.line,
                    function: call.function.clone(),
                    remote: Some(endpoint.clone()),
                    external: true,
                    rpc: rpc || endpoint.via.is_some(),
                });
            }
        }

        // this repo's own rpc handlers are reached through the call graph
        // already, as `rpc` service edges
        if !rpc {
            for (service, endpoint) in local_handlers.get(&key).into_iter().flatten() {
                // a handler in the same service is an internal detail, not a
                // boundary crossing
                if service == &from {
                    continue;
                }
                matched = true;
                out.push(Crossing {
                    key: call.key.clone(),
                    transport: pick_transport(&call.transport, &endpoint.transport),
                    from: from.clone(),
                    to: service.clone(),
                    file: call.file.clone(),
                    line: call.line,
                    function: call.function.clone(),
                    remote: Some(endpoint.clone()),
                    external: false,
                    rpc: false,
                });
            }
        }

        // A call naming a key nobody answers is worth reporting: it is either
        // a typo at one end, or a peer that was never configured. An rpc with
        // no handler anywhere in view is more often a third-party API, which
        // is not a mistake to report.
        if !matched && !rpc {
            out.push(Crossing {
                key: call.key.clone(),
                transport: call.transport.clone(),
                from,
                to: String::new(),
                file: call.file.clone(),
                line: call.line,
                function: call.function.clone(),
                remote: None,
                external: false,
                rpc: false,
            });
        }
    }

    // inbound: a peer says it calls a key this repo serves. Nothing local can
    // observe that, so it only exists because the peer published it.
    for external in externals {
        let Some(surface) = &external.surface else {
            continue;
        };
        for consumed in &surface.consumes {
            let key = norm_key(&consumed.key);
            for (service, endpoint) in local_handlers.get(&key).into_iter().flatten() {
                out.push(Crossing {
                    key: consumed.key.clone(),
                    transport: pick_transport(&consumed.transport, &endpoint.transport),
                    from: external.name.clone(),
                    to: service.clone(),
                    file: endpoint.file.clone(),
                    line: endpoint.line,
                    function: endpoint.function.clone(),
                    remote: Some(consumed.clone()),
                    external: true,
                    rpc: endpoint.via.is_some() || consumed.via.is_some(),
                });
            }
        }
    }

    out.sort_by(|a, b| {
        (&a.from, &a.to, &a.key, &a.file, a.line).cmp(&(&b.from, &b.to, &b.key, &b.file, b.line))
    });
    out
}

// Service edges an rpc makes: the caller's service depends on each handler's,
// and both depend on the service holding the schema - so a changed `.proto`
// reaches everything generated from it, in every language.
fn merge_rpc_edges(
    edges: &mut Vec<ServiceEdge>,
    symbol_callers: &mut BTreeMap<String, BTreeSet<String>>,
    unresolved: &mut Vec<UnresolvedCall>,
    caches: &[FileCache],
    matchers: &[(String, GlobSet)],
    contracts: &ContractIndex,
) {
    let services_of = |fi: usize| assign(matchers, &path_str(&caches[fi].rel_path));
    let symbol = |key: &str, file: &str, line: usize, kind: &str| EdgeSymbol {
        symbol: key.to_string(),
        file: file.to_string(),
        line,
        via: crate::contracts::VIA.to_string(),
        kind: kind.to_string(),
    };
    let mut linked: BTreeSet<(String, usize)> = BTreeSet::new();
    for link in &contracts.callers {
        let c = &contracts.contracts[link.contract];
        let file = path_str(&caches[link.file].rel_path);
        let line = caches[link.file].calls[link.call].line;
        linked.insert((file.clone(), line));
        for from in services_of(link.file) {
            for h in contracts.handlers_of(link.contract) {
                for to in services_of(h.def.0).into_iter().filter(|to| *to != from) {
                    add_edge_symbol(edges, &from, &to, symbol(&c.key, &file, line, "call"));
                    symbol_callers
                        .entry(caches[h.def.0].funcs[h.def.1].name.clone())
                        .or_default()
                        .insert(from.clone());
                }
            }
            if let Some((schema, _)) = c.def {
                for to in services_of(schema).into_iter().filter(|to| *to != from) {
                    add_edge_symbol(edges, &from, &to, symbol(&c.key, &file, line, "type"));
                }
            }
        }
    }
    // a handler depends on its schema just as a caller does
    for h in &contracts.handlers {
        let c = &contracts.contracts[h.contract];
        let Some((schema, _)) = c.def else { continue };
        let file = path_str(&caches[h.def.0].rel_path);
        let line = caches[h.def.0].funcs[h.def.1].line;
        for from in services_of(h.def.0) {
            for to in services_of(schema).into_iter().filter(|to| *to != from) {
                add_edge_symbol(edges, &from, &to, symbol(&c.key, &file, line, "type"));
            }
        }
    }
    // a call the schema accounts for is not an unresolved one
    unresolved.retain(|u| !linked.contains(&(u.file.clone(), u.line)));
    edges.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));
}

// one piece of evidence on the edge `from -> to`, creating the edge if needed
fn add_edge_symbol(edges: &mut Vec<ServiceEdge>, from: &str, to: &str, symbol: EdgeSymbol) {
    match edges.iter_mut().find(|e| e.from == from && e.to == to) {
        Some(edge) => {
            let seen = edge
                .symbols
                .iter()
                .any(|s| s.symbol == symbol.symbol && s.line == symbol.line && s.file == symbol.file);
            if !seen {
                edge.symbols.push(symbol);
            }
            edge.detected = true;
        }
        None => edges.push(ServiceEdge {
            from: from.to_string(),
            to: to.to_string(),
            declared: false,
            detected: true,
            symbols: vec![symbol],
        }),
    }
}

// One side may name a transport and the other leave it out; prefer whichever
// actually said something.
fn pick_transport(a: &str, b: &str) -> String {
    if a != "unspecified" {
        a.to_string()
    } else {
        b.to_string()
    }
}

// Fold crossings into the service edges, so a cross-repo call is an edge of
// the same graph as every other dependency rather than a separate report.
fn merge_crossings(edges: &mut Vec<ServiceEdge>, crossings: &[crate::externals::Crossing]) {
    for crossing in crossings {
        if crossing.from.is_empty() || crossing.to.is_empty() {
            continue;
        }
        let via = if crossing.rpc {
            crate::contracts::VIA
        } else {
            Via::Annotation.label()
        };
        let symbol = EdgeSymbol {
            symbol: crossing.key.clone(),
            file: crossing.file.clone(),
            line: crossing.line,
            via: via.to_string(),
            kind: crossing.transport.clone(),
        };
        add_edge_symbol(edges, &crossing.from, &crossing.to, symbol);
    }
    edges.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));
}

// strongest evidence first, so an edge reports the best reason it has
fn via_rank(via: &str) -> usize {
    match via {
        "annotation" => 0,
        "receiver-type" | "rpc" => 1,
        "qualifier" => 2,
        "project" => 3,
        "import" => 4,
        "type-reference" => 5,
        _ => 6,
    }
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
pub(crate) fn resolve_base(root: &Path, base: Option<&str>) -> Result<(String, String)> {
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

// The changed line ranges per file, plus when each of those files was last
// written. `prompts` needs both: the ranges say what moved, the times say
// which request was in flight when it moved.
pub fn changed_line_times(
    root: &Path,
    base: Option<&str>,
    worktree: bool,
) -> Result<(String, BTreeMap<String, Vec<(usize, usize)>>, BTreeMap<String, i64>)> {
    let (base_label, base_sha) = resolve_base(root, base)?;
    let mut diff_refs: Vec<&str> = vec![&base_sha];
    if !worktree {
        diff_refs.push("HEAD");
    }
    let mut hunk_args = vec!["diff", "--relative", "--unified=0", "-M"];
    hunk_args.extend(&diff_refs);
    let mut hunks = parse_hunks(&git(root, &hunk_args)?);
    let mut uncommitted = BTreeSet::new();
    if worktree {
        for path in git_bytes(root, &["ls-files", "--others", "--exclude-standard", "-z"])?
            .split(|&b| b == 0)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .filter(|s| !s.is_empty())
        {
            hunks.entry(path.clone()).or_insert_with(|| vec![(1, usize::MAX)]);
            uncommitted.insert(path);
        }
        for (_, p) in parse_name_status(&git_bytes(
            root,
            &["diff", "--relative", "--name-status", "-z", "-M", "HEAD"],
        )?) {
            uncommitted.insert(p);
        }
    }
    let times = write_times(root, &base_sha, &hunks, &uncommitted);
    Ok((base_label, hunks, times))
}

// When each changed file was last written, in epoch seconds
pub(crate) fn write_times(
    root: &Path,
    base_sha: &str,
    hunks: &BTreeMap<String, Vec<(usize, usize)>>,
    uncommitted: &BTreeSet<String>,
) -> BTreeMap<String, i64> {
    let mut out = BTreeMap::new();
    let range = format!("{base_sha}..HEAD");
    if let Ok(log) = git(
        root,
        &["log", "--format=@%ct", "--name-only", "--relative", &range],
    ) {
        // newest commit first, so the first mention of a path is its latest
        let mut stamp = 0i64;
        for line in log.lines() {
            match line.strip_prefix('@') {
                Some(t) => stamp = t.trim().parse().unwrap_or(0),
                None if hunks.contains_key(line) => {
                    out.entry(line.to_string()).or_insert(stamp);
                }
                None => {}
            }
        }
    }
    for path in uncommitted {
        let Ok(mtime) = fs::metadata(root.join(path))
            .and_then(|m| m.modified())
            .and_then(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })
        else {
            continue;
        };
        out.insert(path.clone(), mtime.as_secs() as i64);
    }
    out
}

// build one matcher per service; bare pattern with no glob is a dir prefix
pub(crate) fn build_matchers(services: &BTreeMap<String, Vec<String>>) -> Result<Vec<(String, GlobSet)>> {
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
pub(crate) fn assign(matchers: &[(String, GlobSet)], path: &str) -> Vec<String> {
    matchers
        .iter()
        .filter(|(_, set)| set.is_match(path))
        .map(|(name, _)| name.clone())
        .collect()
}

// test functions by naming convention: `test_charge`/`TestCharge` (python,
// rust, go), `BenchmarkX`/`ExampleX`/`FuzzX` (go), `charge_test`/`charge_spec`,
// and the `test("...")` labels `extract` synthesises for BDD callbacks. Used so
// a test living outside a test-named file still counts.
// NOTE: This test was generated by a LLM
pub(crate) fn is_test_fn_name(name: &str) -> bool {
    let n = name.trim_start_matches('_');
    let lower = n.to_ascii_lowercase();
    if lower.starts_with("test_") || lower.ends_with("_test") || lower.ends_with("_spec") {
        return true;
    }
    // `TestCharge`, `BenchmarkCharge`, ... - the prefix must end at a word
    // boundary so `testing`/`Benchmarking` do not match
    for p in ["Test", "Benchmark", "Example", "Fuzz"] {
        if let Some(rest) = n.strip_prefix(p) {
            if rest.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                return true;
            }
        }
    }
    // synthesised BDD label, e.g. `it("charges a fee")`
    n.split_once('(')
        .is_some_and(|(head, _)| BDD_REGISTRARS.contains(&head))
}

// test files by path conventions that I am aware of
pub(crate) fn is_test_path(path: &str) -> bool {
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

pub fn path_str(p: &Path) -> String {
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

    fn svc_map(pairs: &[(&str, &[&str])]) -> BTreeMap<String, BTreeSet<String>> {
        pairs
            .iter()
            .map(|(n, s)| ((*n).into(), s.iter().map(|x| x.to_string()).collect()))
            .collect()
    }

    fn test_idx(def_services: BTreeMap<String, BTreeSet<String>>, calls: Vec<OwnedCall>) -> Indexes {
        Indexes {
            def_services,
            method_services: BTreeMap::new(),
            type_services: BTreeMap::new(),
            module_services: BTreeMap::new(),
            project_services: BTreeMap::new(),
            imports: BTreeMap::new(),
            wildcard_imports: BTreeMap::new(),
            calls,
            type_refs: Vec::new(),
        }
    }

    fn call(svc: &str, name: &str, q: Option<&str>, typed: bool) -> OwnedCall {
        OwnedCall {
            service: svc.into(),
            file: format!("{svc}/main.rs"),
            line: 1,
            name: name.into(),
            qualifier: q.map(|s| s.into()),
            recv_type: None,
            typed,
        }
    }

    // In a typed language a bare name is never evidence. The old rule made an
    // edge whenever exactly one other service defined the name, which is how
    // stdlib method names (`.ok()`, `.parse()`) got attributed to project
    // functions that happened to share them.
    #[test]
    fn typed_calls_need_evidence_not_just_a_matching_name() {
        let defs = svc_map(&[
            ("charge", &["billing"]),
            ("new", &["billing", "auth", "gateway"]),
            ("verify", &["auth", "billing"]),
        ]);
        let idx = test_idx(
            defs,
            vec![
                call("gateway", "charge", None, true),        // sole definer, no evidence
                call("gateway", "new", None, true),           // ambiguous
                call("gateway", "verify", Some("crate::auth"), true), // qualified
                call("billing", "charge", None, true),        // its own symbol
            ],
        );
        let (edges, callers, unresolved) = detect_edges(&idx, &BTreeMap::new());
        let pairs: Vec<(&str, &str)> = edges.iter().map(|e| (e.from.as_str(), e.to.as_str())).collect();
        // only the qualified call survives
        assert_eq!(pairs, vec![("gateway", "auth")]);
        assert_eq!(edges[0].symbols[0].via, "qualifier");
        assert!(!callers.contains_key("charge"), "no evidence, so no caller");

        // and the ones that were dropped are reported, not silently lost
        let reasons: Vec<(&str, &str)> = unresolved
            .iter()
            .map(|u| (u.symbol.as_str(), u.reason.as_str()))
            .collect();
        assert_eq!(reasons, vec![("charge", "no-evidence"), ("new", "ambiguous")]);
    }

    // untyped languages have nothing better, so they keep the old fallback -
    // flagged `name-only` so a reader can tell it apart from real evidence
    #[test]
    fn untyped_calls_keep_the_single_definer_fallback() {
        let idx = test_idx(
            svc_map(&[("charge", &["billing"])]),
            vec![call("gateway", "charge", None, false)],
        );
        let (edges, _, unresolved) = detect_edges(&idx, &BTreeMap::new());
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].symbols[0].via, "name-only");
        assert!(unresolved.is_empty());
    }

    // the strongest route: the receiver's declared type addresses the method
    #[test]
    fn receiver_type_resolves_a_method_call() {
        let mut idx = test_idx(
            svc_map(&[("charge", &["billing", "auth"])]),
            vec![OwnedCall {
                service: "gateway".into(),
                file: "gateway/main.rs".into(),
                line: 3,
                name: "charge".into(),
                qualifier: Some("client".into()),
                recv_type: Some("Ledger".into()),
                typed: true,
            }],
        );
        // `Ledger::charge` exists only in billing, so the ambiguity of the bare
        // name `charge` does not matter
        idx.method_services.insert(
            ("Ledger".into(), "charge".into()),
            ["billing".to_string()].into(),
        );
        let (edges, _, unresolved) = detect_edges(&idx, &BTreeMap::new());
        assert_eq!(edges.len(), 1);
        assert_eq!((edges[0].from.as_str(), edges[0].to.as_str()), ("gateway", "billing"));
        assert_eq!(edges[0].symbols[0].via, "receiver-type");
        assert!(unresolved.is_empty());
    }

    // naming another service's type in a signature is a dependency, even with
    // no call between them
    #[test]
    fn type_references_in_signatures_are_edges() {
        let mut idx = test_idx(BTreeMap::new(), Vec::new());
        idx.type_services.insert("Invoice".into(), ["billing".to_string()].into());
        idx.type_refs.push(TypeRef {
            service: "gateway".into(),
            file: "gateway/main.rs".into(),
            line: 7,
            type_name: "Invoice".into(),
        });
        let (edges, _, _) = detect_edges(&idx, &BTreeMap::new());
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].symbols[0].kind, "type");
        assert_eq!(edges[0].symbols[0].via, "type-reference");
    }

    fn fixture(lang: crate::languages::Language, rel: &str, src: &str) -> FileCache {
        let ex = crate::extract::extract(lang, src).expect("parse");
        FileCache {
            rel_path: PathBuf::from(rel),
            cache_name: rel.replace('/', "-"),
            display_name: rel.to_string(),
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
        }
    }

    fn two_service_matchers() -> Vec<(String, GlobSet)> {
        let mut services = BTreeMap::new();
        services.insert("gateway".to_string(), vec!["gateway/**".to_string()]);
        services.insert("shared".to_string(), vec!["shared/**".to_string()]);
        build_matchers(&services).expect("globs")
    }

    const BILLING_PROTO: &str = "syntax = \"proto3\";\n\
        package acme.billing.v1;\n\
        message CreateInvoiceRequest { string customer = 1; }\n\
        message Invoice { string id = 1; }\n\
        service Billing { rpc CreateInvoice(CreateInvoiceRequest) returns (Invoice); }\n";

    // a typescript client and a go server, with the schema in a third service
    fn rpc_repo() -> (Vec<FileCache>, Vec<(String, GlobSet)>) {
        use crate::languages::Language;
        let caches = vec![
            fixture(
                Language::TypeScript,
                "gateway/charge.ts",
                "import { BillingClient } from \"../gen/acme/billing/v1/billing_grpc_pb\";\n\
                 export function charge() {\n\
                 \x20 const client = new BillingClient(\"addr\");\n\
                 \x20 client.createInvoice({});\n\
                 }\n",
            ),
            fixture(
                Language::Go,
                "billing/server.go",
                "package billing\n\
                 import billingv1 \"github.com/acme/gen/acme/billing/v1\"\n\
                 type server struct{}\n\
                 func (s *server) CreateInvoice(ctx context.Context, req *billingv1.CreateInvoiceRequest) (*billingv1.Invoice, error) {\n\
                 \treturn nil, nil\n\
                 }\n",
            ),
            fixture(Language::Proto, "shared/billing.proto", BILLING_PROTO),
        ];
        let mut services = BTreeMap::new();
        for s in ["gateway", "billing", "shared"] {
            services.insert(s.to_string(), vec![format!("{s}/**")]);
        }
        (caches, build_matchers(&services).expect("globs"))
    }

    // no comment at either end: the schema is what joins them
    #[test]
    fn an_rpc_joins_a_client_to_its_handler_across_languages() {
        let (caches, matchers) = rpc_repo();
        let contracts = ContractIndex::build(&caches, &[]);
        let mut edges = Vec::new();
        let mut callers = BTreeMap::new();
        let mut unresolved = vec![UnresolvedCall {
            symbol: "createInvoice".into(),
            file: "gateway/charge.ts".into(),
            line: 4,
            from: "gateway".into(),
            reason: "no-evidence".into(),
            candidates: Vec::new(),
        }];
        merge_rpc_edges(&mut edges, &mut callers, &mut unresolved, &caches, &matchers, &contracts);

        let found: Vec<(&str, &str, &str)> = edges
            .iter()
            .flat_map(|e| e.symbols.iter().map(move |s| (e.from.as_str(), e.to.as_str(), s.kind.as_str())))
            .collect();
        assert_eq!(
            found,
            vec![
                // the handler depends on the schema it implements
                ("billing", "shared", "type"),
                ("gateway", "billing", "call"),
                ("gateway", "shared", "type"),
            ]
        );
        assert!(edges.iter().flat_map(|e| &e.symbols).all(|s| s.via == "rpc"
            && s.symbol == "acme.billing.v1.Billing/CreateInvoice"));
        // the go handler's report names the typescript service that calls it
        assert_eq!(callers["CreateInvoice"], BTreeSet::from(["gateway".to_string()]));
        // and the call the name-based resolver gave up on is accounted for
        assert!(unresolved.is_empty(), "{unresolved:?}");

        // a changed schema reaches both ends
        let impact = impact_closure(&BTreeSet::from(["shared".to_string()]), &edges);
        let hit: BTreeSet<&str> = impact.iter().map(|i| i.service.as_str()).collect();
        assert_eq!(hit, BTreeSet::from(["shared", "billing", "gateway"]));
    }

    // Two repositories: the client here, the server in a peer that published
    // a surface. The rpc key is the same string at both ends because both were
    // derived from the same schema.
    #[test]
    fn an_rpc_reaches_a_handler_in_a_peer_repository() {
        let (mut caches, matchers) = rpc_repo();
        caches.retain(|c| !c.rel_path.starts_with("billing"));
        let contracts = ContractIndex::build(&caches, &[]);
        let peer = crate::externals::ExternalService {
            name: "billing".to_string(),
            config: Default::default(),
            source: "surface".to_string(),
            surface: Some(crate::externals::Surface {
                schema: crate::externals::SURFACE_SCHEMA.to_string(),
                name: "billing".to_string(),
                generated: "t".to_string(),
                repo: None,
                languages: vec!["go".to_string()],
                provides: vec![crate::externals::Endpoint {
                    key: "acme.billing.v1.Billing/CreateInvoice".to_string(),
                    transport: "grpc".to_string(),
                    function: "CreateInvoice".to_string(),
                    file: "svc/server.go".to_string(),
                    line: 4,
                    service: None,
                    via: Some("rpc".to_string()),
                }],
                consumes: Vec::new(),
                rpc_endpoints: true,
            }),
            error: None,
        };
        let crossings = detect_crossings(&caches, &matchers, &[peer], &contracts);
        assert_eq!(crossings.len(), 1, "{crossings:?}");
        let c = &crossings[0];
        assert_eq!((c.from.as_str(), c.to.as_str()), ("gateway", "billing"));
        assert!(c.rpc && c.external);
        assert_eq!(c.remote.as_ref().unwrap().file, "svc/server.go");

        // with no peer serving it, an rpc is not reported as a dangling key
        let crossings = detect_crossings(&caches, &matchers, &[], &contracts);
        assert!(crossings.is_empty(), "{crossings:?}");
    }

    // The monorepo case: two directories in one repo, joined by a key rather
    // than by a call the parser could ever follow.
    #[test]
    fn a_key_joins_two_services_in_one_repo() {
        let caches = vec![
            fixture(
                crate::languages::Language::Rust,
                "gateway/main.rs",
                "pub fn emit() {\n    // ccc:calls queue audit.events\n    let _ = 1;\n}\n",
            ),
            fixture(
                crate::languages::Language::Rust,
                "shared/audit.rs",
                "// ccc:serves queue audit.events\npub fn record(e: &str) -> usize { e.len() }\n",
            ),
        ];
        let crossings = detect_crossings(&caches, &two_service_matchers(), &[], &ContractIndex::default());
        assert_eq!(crossings.len(), 1, "{crossings:?}");
        let c = &crossings[0];
        assert_eq!((c.from.as_str(), c.to.as_str()), ("gateway", "shared"));
        assert_eq!(c.transport, "queue");
        assert_eq!(c.file, "gateway/main.rs");
        assert_eq!(c.line, 2, "the call site is the comment, not the function");
        assert!(!c.external);
        let remote = c.remote.as_ref().expect("handler");
        assert_eq!((remote.file.as_str(), remote.function.as_str()), ("shared/audit.rs", "record"));
    }

    // The cross-repo case: the far end is a surface, in another language, and
    // no source from it is ever parsed here.
    #[test]
    fn a_key_joins_a_call_here_to_a_handler_in_a_peer_repo() {
        let caches = vec![fixture(
            crate::languages::Language::Rust,
            "gateway/main.rs",
            "pub fn checkout() {\n    // ccc:calls grpc billing.v1.Charge\n    let _ = 1;\n}\n",
        )];
        let peer = crate::externals::ExternalService {
            name: "billing".to_string(),
            config: Default::default(),
            source: "surface test".to_string(),
            surface: Some(crate::externals::Surface {
                schema: crate::externals::SURFACE_SCHEMA.to_string(),
                name: "billing".to_string(),
                generated: "t".to_string(),
                repo: Some("acme/billing".to_string()),
                languages: vec!["go".to_string()],
                provides: vec![crate::externals::Endpoint {
                    key: "billing.v1.Charge".to_string(),
                    transport: "grpc".to_string(),
                    function: "Charge".to_string(),
                    file: "svc/charge.go".to_string(),
                    line: 42,
                    service: None,
                    via: None,
                }],
                consumes: Vec::new(),
                rpc_endpoints: false,
            }),
            error: None,
        };
        let crossings = detect_crossings(&caches, &two_service_matchers(), &[peer], &ContractIndex::default());
        assert_eq!(crossings.len(), 1, "{crossings:?}");
        let c = &crossings[0];
        assert_eq!(c.to, "billing");
        assert!(c.external, "the far end is another repository");
        let remote = c.remote.as_ref().expect("handler");
        assert_eq!((remote.file.as_str(), remote.line), ("svc/charge.go", 42));
    }

    // Keys are written by hand at both ends; case and padding are not identity.
    #[test]
    fn key_matching_ignores_case_and_padding() {
        let caches = vec![
            fixture(
                crate::languages::Language::Rust,
                "gateway/main.rs",
                "pub fn a() {\n    // ccc:calls grpc Billing.V1.Charge\n}\n",
            ),
            fixture(
                crate::languages::Language::Go,
                "shared/b.go",
                "package b\n\n// ccc:serves grpc billing.v1.charge\nfunc Charge() {}\n",
            ),
        ];
        let crossings = detect_crossings(&caches, &two_service_matchers(), &[], &ContractIndex::default());
        assert_eq!(crossings.len(), 1, "{crossings:?}");
        assert!(crossings[0].remote.is_some());
    }

    // A call nobody answers is a finding, not silence: it is a typo at one end
    // or a peer nobody configured.
    #[test]
    fn a_key_nothing_serves_is_reported_unmatched() {
        let caches = vec![fixture(
            crate::languages::Language::Rust,
            "gateway/main.rs",
            "pub fn a() {\n    // ccc:calls grpc nobody.Answers\n}\n",
        )];
        let crossings = detect_crossings(&caches, &two_service_matchers(), &[], &ContractIndex::default());
        assert_eq!(crossings.len(), 1);
        assert!(crossings[0].remote.is_none());
        assert_eq!(crossings[0].to, "");
    }

    // Calling a handler in your own service is an ordinary call, not a hop.
    #[test]
    fn a_key_served_inside_the_same_service_is_not_a_crossing() {
        let caches = vec![fixture(
            crate::languages::Language::Rust,
            "gateway/main.rs",
            "// ccc:serves queue x.y\npub fn h() {}\n\npub fn a() {\n    // ccc:calls queue x.y\n}\n",
        )];
        let crossings = detect_crossings(&caches, &two_service_matchers(), &[], &ContractIndex::default());
        assert!(crossings.iter().all(|c| c.remote.is_none()), "{crossings:?}");
    }

    // A peer publishes what it consumes, which is the only way this repo can
    // learn that something out there calls in.
    #[test]
    fn a_peer_consuming_our_key_is_an_inbound_crossing() {
        let caches = vec![fixture(
            crate::languages::Language::Rust,
            "shared/audit.rs",
            "// ccc:serves grpc audit.v1.Record\npub fn record() {}\n",
        )];
        let peer = crate::externals::ExternalService {
            name: "billing".to_string(),
            config: Default::default(),
            source: "surface test".to_string(),
            surface: Some(crate::externals::Surface {
                schema: crate::externals::SURFACE_SCHEMA.to_string(),
                name: "billing".to_string(),
                generated: "t".to_string(),
                repo: None,
                languages: vec!["go".to_string()],
                provides: Vec::new(),
                consumes: vec![crate::externals::Endpoint {
                    key: "audit.v1.Record".to_string(),
                    transport: "grpc".to_string(),
                    function: "Charge".to_string(),
                    file: "svc/charge.go".to_string(),
                    line: 7,
                    service: None,
                    via: None,
                }],
                rpc_endpoints: false,
            }),
            error: None,
        };
        let crossings = detect_crossings(&caches, &two_service_matchers(), &[peer], &ContractIndex::default());
        assert_eq!(crossings.len(), 1, "{crossings:?}");
        let c = &crossings[0];
        assert_eq!((c.from.as_str(), c.to.as_str()), ("billing", "shared"));
        assert_eq!(c.file, "shared/audit.rs", "the local handler carries the hint");
    }

    // A surface round-trips: what one repo exports is what another reads.
    #[test]
    fn a_surface_round_trips_through_json() {
        let caches = vec![fixture(
            crate::languages::Language::Go,
            "svc/charge.go",
            "package svc\n\n// ccc:serves grpc billing.v1.Charge\nfunc Charge() {}\n\nfunc C() {\n\t// ccc:calls grpc ledger.v1.Write\n}\n",
        )];
        let surface = crate::externals::Surface::from_caches("billing", "t", &caches, &ContractIndex::default());
        assert_eq!(surface.provides.len(), 1);
        assert_eq!(surface.consumes.len(), 1);
        assert_eq!(surface.languages, vec!["go".to_string()]);
        let json = serde_json::to_string(&surface).expect("serialize");
        let back: crate::externals::Surface = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.provides[0].key, "billing.v1.Charge");
        assert_eq!(back.provides[0].function, "Charge");
    }

    // `deps` may name a peer, but a name cannot be both a local service and a
    // repository somewhere else.
    // The rename has to be invisible to a map.json written before it, and
    // loud enough that the file gets fixed.
    #[test]
    fn the_old_deps_spelling_is_still_read_and_reported() {
        let dir = std::env::temp_dir().join(format!("ccc-relatives-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".ccc")).unwrap();
        let write = |body: &str| fs::write(dir.join(".ccc").join(CONFIG_NAME), body).unwrap();

        write(r#"{"services":{"gateway":["gateway/**"]},"deps":{"gateway":["auth"]}}"#);
        let cfg = ChangesConfig::load(&dir).expect("legacy map.json still loads");
        assert_eq!(cfg.relatives["gateway"], vec!["auth".to_string()]);

        // both spellings in one file is a contradiction, not a merge
        write(r#"{"services":{},"deps":{"a":[]},"relatives":{"a":["b"]}}"#);
        let err = format!("{:#}", ChangesConfig::load(&dir).expect_err("both keys"));
        assert!(err.contains("duplicate field"), "{err}");

        write(r#"{"services":{},"relatives":{"gateway":["auth"]}}"#);
        assert!(ChangesConfig::load(&dir).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dep_may_name_an_external_but_a_name_cannot_be_both() {
        let cfg: ChangesConfig = serde_json::from_str(
            r#"{"services":{"gateway":["gateway/**"]},
                "relatives":{"gateway":["billing"]},
                "externals":{"billing":{"path":"../billing"}}}"#,
        )
        .expect("parse");
        assert!(cfg.externals.contains_key("billing"));
        assert_eq!(cfg.externals["billing"].path.as_deref(), Some("../billing"));
        // unknown keys stay ignored, so an older ccc reads a newer map.json
        let old: ChangesConfig =
            serde_json::from_str(r#"{"services":{},"relatives":{},"future_field":42}"#).expect("parse");
        assert!(old.services.is_empty());
        // `relatives` was `deps` when it could only name a service in this
        // repo. A map.json written then still declares what it declared.
        let legacy: ChangesConfig =
            serde_json::from_str(r#"{"services":{},"deps":{"gateway":["auth"]}}"#).expect("parse");
        assert_eq!(legacy.relatives["gateway"], vec!["auth".to_string()]);
    }

    #[test]
    fn manifests_identify_projects_for_cross_project_imports() {
        assert_eq!(
            manifest_identity("go.mod", "module github.com/acme/billing\n\ngo 1.22\n").as_deref(),
            Some("github.com/acme/billing")
        );
        assert_eq!(
            manifest_identity("Cargo.toml", "[package]\nname = \"billing-core\"\n").as_deref(),
            Some("billing-core")
        );
        assert_eq!(
            manifest_identity("package.json", r#"{"name":"@acme/billing"}"#).as_deref(),
            Some("@acme/billing")
        );
        // a go import inside the module resolves by path prefix
        assert!(names_project(
            "github.com/acme/billing/pkg/money",
            "github.com/acme/billing"
        ));
        assert!(!names_project("github.com/acme/other/pkg", "github.com/acme/billing"));
        // a hyphenated crate is written underscored in code
        assert!(names_project("billing_core::charge", "billing-core"));
    }

    #[test]
    fn closure_walks_reverse_edges_transitively() {
        let edge = |from: &str, to: &str| ServiceEdge {
            from: from.into(),
            to: to.into(),
            declared: false,
            detected: true,
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
    fn test_function_names_by_convention() {
        for n in [
            "test_charge",
            "TestCharge",
            "BenchmarkCharge",
            "FuzzCharge",
            "charge_test",
            "charge_spec",
            "__test_charge",
            "test(\"charge\")",
            "it(\"charges a fee\")",
        ] {
            assert!(is_test_fn_name(n), "{n} should read as a test function");
        }
        for n in ["charge", "testing", "Benchmarking", "attest", "contest", "<top>"] {
            assert!(!is_test_fn_name(n), "{n} should NOT read as a test function");
        }
    }

    // a call in test context whose enclosing scope is the file itself still
    // counts as a test reference, it just cannot name a test function
    #[test]
    fn top_level_test_calls_mark_tested_without_naming() {
        let base: &[(&str, &str)] = &[
            ("lib/pay.py", "def charge(cents):\n    return cents\n"),
            ("lib/test_smoke.py", "from lib.pay import charge\n\ncharge(1)\n"),
        ];
        let head: &[(&str, &str)] = &[(
            "lib/pay.py",
            "def charge(cents):\n    return cents + 1\n",
        )];
        let report = changes_fixture(base, head, "top-level-test");
        let charge = report
            .changed_functions
            .iter()
            .find(|f| f.function == "charge")
            .expect("charge changed");
        assert!(charge.tested, "a top-level call in a test file is a reference");
        assert!(charge.tested_by.is_empty(), "but there is no test fn to name");
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

    // The service map config was named `surf.json` shipped, now
    // `map.json`. A checkout carrying an old one must keep
    // its service map - falling back to directory-derived services would rename
    // every service in the report and quietly change what CI tests.
    #[test]
    fn every_previous_config_name_still_loads() {
        let dir = std::env::temp_dir().join(format!("ccc-legacy-cfg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".ccc")).unwrap();
        let cfg = r#"{ "services": { "billing": ["billing/**"] }, "relatives": { "billing": [] } }"#;

        // every name this file has had still resolves, one at a time
        for old in LEGACY_CONFIG_NAMES {
            let at = dir.join(".ccc").join(old);
            fs::write(&at, cfg).unwrap();
            let loaded = ChangesConfig::load(&dir).unwrap();
            assert_eq!(
                loaded.services.keys().collect::<Vec<_>>(),
                vec!["billing"],
                "`.ccc/{old}` was ignored"
            );
            // and `--init` must not clobber it
            assert!(init_config(&dir).is_err(), "--init overwrote .ccc/{old}");
            fs::remove_file(&at).unwrap();
        }

        // the current name wins over every legacy one
        for old in LEGACY_CONFIG_NAMES {
            fs::write(dir.join(".ccc").join(old), cfg).unwrap();
        }
        fs::write(
            dir.join(".ccc").join(CONFIG_NAME),
            r#"{ "services": { "payments": ["payments/**"] } }"#,
        )
        .unwrap();
        let loaded = ChangesConfig::load(&dir).unwrap();
        assert_eq!(loaded.services.keys().collect::<Vec<_>>(), vec!["payments"]);
        assert!(init_config(&dir).is_err());
        let _ = fs::remove_dir_all(&dir);
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
                "-c", "user.name=changes-test",
                "-c", "user.email=changes@test",
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
    // second commit, and diff the branch against the base commit
    // NOTE: This fixture was generated by a LLM
    fn changes_fixture(base: &[(&str, &str)], head: &[(&str, &str)], tag: &str) -> ChangesReport {
        let dir = std::env::temp_dir().join(format!("ccc-changes-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        write_files(&dir, base);
        run(&dir, &["git", "init", "-q"]);
        commit_all(&dir, "base");
        let base_sha = rev_head(&dir);
        write_files(&dir, head);
        commit_all(&dir, "branch work");
        let opts = ChangesOptions {
            base: Some(base_sha),
            service_flags: vec![],
            worktree: false,
            prompts: false,
            deps: false,
        };
        let report = changes(&dir, ".", &opts).unwrap_or_else(|e| panic!("{tag}: {e}"));
        let _ = fs::remove_dir_all(&dir);
        report
    }

    const PAIR_MAP_JSON: &str = r#"{ "services": { "api": ["api/**"], "lib": ["lib/**"] } }"#;

    struct PairFixture {
        lang: &'static str,
        main_fn: &'static str,
        helper: &'static str,
        // the test function `main_fn` should be attributed to
        test_fn: &'static str,
        base: &'static [(&'static str, &'static str)],
        head: &'static [(&'static str, &'static str)],
    }

    // Every language `ccc scan` supports has a pair fixture telling the same
    // story: api calls lib's charge; the branch makes charge call a new
    // untested fee helper. Asserting the exact report keeps the languages and
    // the implementation in lock-step.
    // NOTE: These fixtures were generated by a LLM
    #[test]
    fn changes_language_pair_fixtures() {
        const LANGS: &[PairFixture] = &[
            PairFixture {
                lang: "python",
                main_fn: "charge",
                helper: "fee",
                test_fn: "test_charge",
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
                test_fn: "test(\"charge\")",
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
                test_fn: "test(\"charge\")",
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
                test_fn: "TestCharge",
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
                test_fn: "charge_works",
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
            let mut base = vec![(".ccc/map.json", PAIR_MAP_JSON)];
            base.extend_from_slice(fx.base);
            let report = changes_fixture(&base, fx.head, lang);

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
            assert_eq!(
                main.tested_by,
                vec![fx.test_fn],
                "{lang}: {main_fn} should name the test that exercises it"
            );
            assert_eq!(main.called_from, vec!["api"], "{lang}");
            let untested: Vec<&str> = report
                .untested
                .iter()
                .map(|f| f.function.as_str())
                .collect();
            assert_eq!(untested, vec![helper], "{lang}");
            assert!(
                report.untested.iter().all(|f| f.tested_by.is_empty()),
                "{lang}: untested functions cannot name a test"
            );
        }
    }

    // Three-service story: gateway calls billing's charge/refund and declares
    // a dependency on auth; the branch makes charge call a new untested fee
    // helper. Asserts the exact report end-to-end.
    // NOTE: These fixtures were also generated by a LLM
    #[test]
    fn changes_three_services_fixture() {
        const MAP_JSON: &str = r#"{
  "services": {
    "auth":    ["auth/**"],
    "billing": ["billing/**"],
    "gateway": ["gateway/**"]
  },
  "relatives": { "gateway": ["auth"] }
}"#;
        let base: &[(&str, &str)] = &[
            (".ccc/map.json", MAP_JSON),
            (
                "gateway/src/main.rs",
                // both calls are qualified: bare `charge(100)` would not
                // compile here, and must not resolve either
                "fn handle() -> u64 { billing::charge(100) + billing::refund(5) }\n",
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
        let report = changes_fixture(base, head, "rust-demo");

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
        // nothing in gateway calls auth here, so the analysis ran and found
        // nothing - `detected` says so rather than the edge being unexplained
        assert!(!declared.detected);
        assert!(declared.symbols.is_empty());
        // the detected edge is not marked declared, and vice versa - they are
        // independent facts, not alternatives
        assert!(detected.detected && !detected.declared);

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
        // every edge here is carried by real evidence
        assert!(detected.symbols.iter().all(|s| s.via == "qualifier"), "{:?}",
            detected.symbols.iter().map(|s| &s.via).collect::<Vec<_>>());
        assert!(report.unresolved_calls.is_empty());
    }

    // End to end, in a real repo: a typed call with no evidence must not
    // become an edge, and must be reported so the gap is visible.
    #[test]
    fn typed_bare_call_is_reported_not_invented() {
        const CFG: &str = r#"{ "services": { "billing": ["billing/**"], "gateway": ["gateway/**"] } }"#;
        let base: &[(&str, &str)] = &[
            (".ccc/map.json", CFG),
            ("billing/src/charge.rs", "pub fn charge(c: u64) -> u64 { c }\n"),
            // no `use`, no qualifier: this does not compile, and the old rule
            // still produced a gateway -> billing edge from the name alone
            ("gateway/src/main.rs", "fn handle() -> u64 { charge(100) }\n"),
        ];
        let head: &[(&str, &str)] = &[(
            "billing/src/charge.rs",
            "pub fn charge(c: u64) -> u64 { c + 1 }\n",
        )];
        let report = changes_fixture(base, head, "bare-call");
        assert!(
            report.edges.is_empty(),
            "a bare name is not evidence: {:?}",
            report.edges.iter().map(|e| (&e.from, &e.to)).collect::<Vec<_>>()
        );
        let u = report
            .unresolved_calls
            .iter()
            .find(|u| u.symbol == "charge")
            .expect("the dropped call must be reported");
        assert_eq!(u.from, "gateway");
        assert_eq!(u.reason, "no-evidence");
        assert_eq!(u.candidates, vec!["billing"]);
        // and gateway is not dragged into the test set on a phantom edge
        assert_eq!(report.services_to_test, vec!["billing"]);
    }

    // The typed path: the receiver's declared type picks the right service
    // even when the method name is defined in several of them.
    #[test]
    fn receiver_type_disambiguates_across_services() {
        const CFG: &str = r#"{ "services": { "billing": ["billing/**"], "auth": ["auth/**"], "gateway": ["gateway/**"] } }"#;
        let base: &[(&str, &str)] = &[
            (".ccc/map.json", CFG),
            (
                "billing/ledger.rs",
                "pub struct Ledger;\nimpl Ledger { pub fn post(&self, n: u64) -> u64 { n } }\n",
            ),
            // `post` also exists in auth, so the bare name is ambiguous
            (
                "auth/session.rs",
                "pub struct Session;\nimpl Session { pub fn post(&self, n: u64) -> u64 { n } }\n",
            ),
            (
                "gateway/main.rs",
                "fn handle(led: &Ledger) -> u64 { led.post(1) }\n",
            ),
        ];
        let head: &[(&str, &str)] = &[(
            "billing/ledger.rs",
            "pub struct Ledger;\nimpl Ledger { pub fn post(&self, n: u64) -> u64 { n + 1 } }\n",
        )];
        let report = changes_fixture(base, head, "recv-type");

        let edge = report
            .edges
            .iter()
            .find(|e| e.from == "gateway" && e.to == "billing")
            .expect("gateway -> billing via the receiver's type");
        // resolved through `Ledger`, not through the ambiguous name `post`
        let post = edge.symbols.iter().find(|s| s.symbol == "post").expect("post");
        assert_eq!(post.via, "receiver-type");
        // and no edge to auth, which defines the same method name
        assert!(!report.edges.iter().any(|e| e.to == "auth"), "auth must not match");
        // the `&Ledger` parameter is itself a typed dependency
        assert!(edge.symbols.iter().any(|s| s.kind == "type" && s.symbol == "Ledger"));
    }

    // Go, C++ and TypeScript resolve through their own module systems.
    #[test]
    fn typed_languages_resolve_by_module_and_receiver() {
        const CFG: &str = r#"{ "services": { "lib": ["lib/**"], "api": ["api/**"] } }"#;

        // go: the qualifier names the `package`, not the service or the file
        let report = changes_fixture(
            &[
                (".ccc/map.json", CFG),
                ("lib/money.go", "package money\n\ntype Ledger struct{}\n\nfunc (l *Ledger) Charge(c int) int { return c }\n"),
                ("api/main.go", "package main\n\nimport \"acme/lib/money\"\n\nfunc handle() int {\n\tvar led money.Ledger\n\treturn led.Charge(1)\n}\n"),
            ],
            &[("lib/money.go", "package money\n\ntype Ledger struct{}\n\nfunc (l *Ledger) Charge(c int) int { return c + 1 }\n")],
            "go-typed",
        );
        let go_edge = report.edges.iter().find(|e| e.from == "api" && e.to == "lib")
            .expect("go api -> lib");
        assert!(go_edge.symbols.iter().any(|s| s.symbol == "Charge"), "{:?}",
            go_edge.symbols.iter().map(|s| &s.symbol).collect::<Vec<_>>());

        // typescript: `new Gateway()` types the receiver
        let report = changes_fixture(
            &[
                (".ccc/map.json", CFG),
                ("lib/wire.ts", "export class Gateway { send(n: number): number { return n; } }\n"),
                ("api/app.ts", "import { Gateway } from \"../lib/wire\";\nexport function go(): number {\n  const g = new Gateway();\n  return g.send(1);\n}\n"),
            ],
            &[("lib/wire.ts", "export class Gateway { send(n: number): number { return n + 1; } }\n")],
            "ts-typed",
        );
        let ts_edge = report.edges.iter().find(|e| e.from == "api" && e.to == "lib")
            .expect("ts api -> lib");
        assert!(ts_edge.symbols.iter().any(|s| s.symbol == "send" && s.via == "receiver-type"),
            "{:?}", ts_edge.symbols.iter().map(|s| (&s.symbol, &s.via)).collect::<Vec<_>>());

        // c++: the qualifier names a namespace
        let report = changes_fixture(
            &[
                (".ccc/map.json", CFG),
                ("lib/acct.cpp", "namespace billing {\ndouble debit(double a) { return a; }\n}\n"),
                ("api/main.cpp", "double handle() { return billing::debit(1.0); }\n"),
            ],
            &[("lib/acct.cpp", "namespace billing {\ndouble debit(double a) { return a + 1; }\n}\n")],
            "cpp-typed",
        );
        let cpp_edge = report.edges.iter().find(|e| e.from == "api" && e.to == "lib")
            .expect("cpp api -> lib");
        assert!(cpp_edge.symbols.iter().any(|s| s.symbol == "debit" && s.via == "qualifier"));
    }

    #[test]
    fn changes_end_to_end_git() {
        let dir = std::env::temp_dir().join(format!("ccc-changes-e2e-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("billing/src")).unwrap();
        fs::create_dir_all(dir.join("gateway/src")).unwrap();
        fs::create_dir_all(dir.join(".ccc")).unwrap();

        fs::write(
            dir.join(".ccc/map.json"),
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
            "fn handle() -> u64 { billing::charge(100) }\n",
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

        let opts = ChangesOptions {
            base: Some(base_sha.clone()),
            service_flags: vec![],
            worktree: false,
            prompts: false,
            deps: false,
        };
        let report = changes(&dir, ".", &opts).unwrap();

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
        let report2 = changes(&dir, ".", &opts).unwrap();
        let f2 = report2
            .changed_functions
            .iter()
            .find(|f| f.function == "charge")
            .expect("charge still changed vs base");
        assert!(f2.tested, "test reference should mark it tested");
        assert_eq!(f2.tested_by, vec!["charges"], "the test fn should be named");
        assert!(report2.untested.iter().all(|f| f.function != "charge"));

        let _ = fs::remove_dir_all(&dir);
    }

    // keep the helper used (constructing Indexes without test_called noise)
    #[test]
    fn edge_rule_ignores_unknown_symbols() {
        let idx = test_idx(
            BTreeMap::new(),
            vec![call("a", "nowhere", None, true)],
        );
        let (edges, callers, unresolved) = detect_edges(&idx, &BTreeMap::new());
        assert!(unresolved.is_empty(), "an unknown symbol is not a dependency");
        assert!(edges.is_empty());
        assert!(callers.is_empty());
    }
}
