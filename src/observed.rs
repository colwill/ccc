//! What a test run observed: which functions each fixture actually ran, read
//! from `ccc-observed/1` records and tied to the map's own definitions.

use crate::changes::path_str;
use crate::coverage::{DefId, TestSite};
use crate::languages::Language;
use crate::model::{FileCache, Func};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const SCHEMA: &str = "ccc-observed/1";
// where a run on this machine leaves its records when nothing else is named
pub const LOCAL_DIR: &str = ".ccc/observed";
// a fetched record set is reused this long - a nightly run does not change by the minute
const FETCH_TTL: Duration = Duration::from_secs(600);
// the records of a large suite run to hundreds of megabytes
const MAX_RECORD_BYTES: u64 = 512 * 1024 * 1024;
// what a fixture with no recorded duration is taken to cost
const DEFAULT_MS: u64 = 1000;
// search steps the exact cover may take before the greedy one stands in
const EXACT_BUDGET: usize = 200_000;
// malformed lines named in the report - the rest are only counted
const MAX_ERRORS: usize = 20;

// sources named on the command line, read on top of the project's own
static EXTRA: OnceLock<Vec<String>> = OnceLock::new();
// record sets by url, each with when it was fetched
type Fetched = Mutex<BTreeMap<String, (Instant, Arc<String>)>>;
// fetched urls - a server re-analysing every few seconds must not refetch a nightly run
static FETCHED: OnceLock<Fetched> = OnceLock::new();

// read these as well as `.ccc/map.json`'s `observed` - the binary passes `--observed` here
pub fn add_sources(sources: Vec<String>) {
    let _ = EXTRA.set(sources);
}

// what a fixture is, which decides how dear running it is
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Unit,
    Contract,
    Interop,
    Integration,
    Smoke,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Unit => "unit",
            Kind::Contract => "contract",
            Kind::Interop => "interop",
            Kind::Integration => "integration",
            Kind::Smoke => "smoke",
        }
    }

    // how much dearer a run of this kind is than a unit test of the same length
    fn weight(self) -> u64 {
        match self {
            Kind::Unit => 1,
            Kind::Contract => 2,
            Kind::Interop => 3,
            Kind::Integration => 4,
            Kind::Smoke => 5,
        }
    }
}

// one place records are read from, as `.ccc/map.json` names it - a file, a directory or a url
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Source {
    At(String),
    // `auth` is `env:NAME` - the variable holding a bearer token for a private url
    Authed {
        from: String,
        #[serde(default)]
        auth: Option<String>,
    },
}

impl Source {
    fn parts(&self) -> (&str, Option<&str>) {
        match self {
            Source::At(at) => (at, None),
            Source::Authed { from, auth } => (from, auth.as_deref()),
        }
    }
}

// one line of a record file - one fixture as one service saw it
#[derive(Deserialize)]
struct Record {
    schema: String,
    fixture: FixtureIn,
    #[serde(default)]
    run: Option<String>,
    #[serde(default)]
    service: Option<ServiceIn>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    files: Vec<FileIn>,
}

#[derive(Deserialize)]
struct FixtureIn {
    // what the runner selects it by - `TestRefund`, `test_partial_refund`, `Billing.Refunds`
    name: String,
    kind: Kind,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    language: Option<String>,
}

#[derive(Deserialize)]
struct ServiceIn {
    #[serde(default)]
    name: Option<String>,
    // the commit the service was built from - line numbers hold only against it
    #[serde(default)]
    commit: Option<String>,
}

#[derive(Deserialize)]
struct FileIn {
    path: String,
    #[serde(default)]
    functions: Vec<FuncIn>,
    // ranges of lines that ran, for a probe that sees lines rather than functions
    #[serde(default)]
    lines: Vec<[usize; 2]>,
}

#[derive(Deserialize)]
struct FuncIn {
    // bare or qualified - `charge`, `Billing.charge`, `(*Server).Charge`
    name: String,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    line: Option<usize>,
}

// one fixture across every record that names it
#[derive(Debug, Clone)]
pub struct Fixture {
    pub name: String,
    pub file: Option<String>,
    // where the map defines it - 0 when the map does not hold the fixture
    pub line: usize,
    pub language: Option<String>,
    pub kind: Kind,
    // the longest any record gave it
    pub duration_ms: Option<u64>,
    pub runs: BTreeSet<String>,
    pub services: BTreeSet<String>,
}

impl Fixture {
    // what running it costs - its length weighted by its kind
    pub fn cost(&self) -> u64 {
        self.duration_ms.unwrap_or(DEFAULT_MS).max(1) * self.kind.weight()
    }

    // the fixture as a test a runner can select
    pub fn site(&self) -> TestSite {
        let language = self.language.as_deref().unwrap_or_default();
        TestSite {
            name: self.name.clone(),
            file: self.file.clone().unwrap_or_default(),
            line: self.line,
            language: Language::ALL.iter().map(|l| l.as_str()).find(|l| *l == language).unwrap_or("other"),
        }
    }
}

#[derive(Debug, Default)]
pub struct Observed {
    // every place records were looked for
    pub sources: Vec<String>,
    // sources that could not be read, and the first lines that were not records
    pub errors: Vec<String>,
    pub bad_lines: usize,
    pub records: usize,
    pub fixtures: Vec<Fixture>,
    // the fixtures that ran each definition
    pub ran: BTreeMap<DefId, BTreeSet<usize>>,
    // record paths no file in the map holds - another repository's, usually
    pub unmatched_files: BTreeSet<String>,
    // functions and line ranges a record names that the map no longer has
    pub unmatched: usize,
}

// the fixtures chosen to run, what they cost together, and whether no cheaper cover exists
#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub fixtures: Vec<usize>,
    pub cost: u64,
    pub exact: bool,
}

impl Observed {
    // whether any source was looked at - with none, every report reads as it did before records existed
    pub fn configured(&self) -> bool {
        !self.sources.is_empty()
    }

    // the fixture a test function is, when a record names it
    pub fn fixture_at(&self, file: &str, name: &str) -> Option<usize> {
        self.fixtures.iter().position(|f| f.file.as_deref() == Some(file) && f.name == name)
    }

    pub fn cost_of(&self, fixtures: &[usize]) -> u64 {
        fixtures.iter().map(|&f| self.fixtures[f].cost()).sum()
    }

    // every fixture that ran any of `defs`
    pub fn safe(&self, defs: &[DefId]) -> Selection {
        let chosen: Vec<usize> = defs
            .iter()
            .filter_map(|d| self.ran.get(d))
            .flatten()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        Selection { cost: self.cost_of(&chosen), fixtures: chosen, exact: true }
    }

    // the cheapest fixtures that between them run each of `defs` at least once
    pub fn minimal(&self, defs: &[DefId]) -> Selection {
        let universe: Vec<&BTreeSet<usize>> =
            defs.iter().filter_map(|d| self.ran.get(d)).filter(|s| !s.is_empty()).collect();
        let candidates: Vec<usize> = universe.iter().copied().flatten().copied().collect::<BTreeSet<_>>().into_iter().collect();
        // what each candidate covers, as positions in `universe`
        let covers: Vec<BTreeSet<usize>> = candidates
            .iter()
            .map(|c| (0..universe.len()).filter(|&u| universe[u].contains(c)).collect())
            .collect();
        let costs: Vec<u64> = candidates.iter().map(|&c| self.fixtures[c].cost()).collect();
        let exact = (universe.len() <= 128).then(|| exact_cover(&covers, &costs, universe.len())).flatten();
        let found = exact.is_some();
        let picked = exact.unwrap_or_else(|| greedy_cover(&covers, &costs, universe.len()));
        let mut chosen: Vec<usize> = picked.into_iter().map(|p| candidates[p]).collect();
        chosen.sort_unstable();
        Selection { cost: self.cost_of(&chosen), fixtures: chosen, exact: found }
    }
}

// the cheapest cover, branching on the first element left uncovered - `None` past the step budget
fn exact_cover(covers: &[BTreeSet<usize>], costs: &[u64], size: usize) -> Option<Vec<usize>> {
    struct Search<'a> {
        masks: &'a [u128],
        costs: &'a [u64],
        by_elem: &'a [Vec<usize>],
        full: u128,
        best: Option<(u64, Vec<usize>)>,
        steps: usize,
    }
    // false once the budget runs out
    fn go(s: &mut Search, covered: u128, cost: u64, chosen: &mut Vec<usize>) -> bool {
        s.steps += 1;
        if s.steps > EXACT_BUDGET {
            return false;
        }
        if s.best.as_ref().is_some_and(|(best, _)| cost >= *best) {
            return true;
        }
        if covered == s.full {
            s.best = Some((cost, chosen.clone()));
            return true;
        }
        let first = (!covered & s.full).trailing_zeros() as usize;
        let (by_elem, masks, costs) = (s.by_elem, s.masks, s.costs);
        for &c in &by_elem[first] {
            chosen.push(c);
            let finished = go(s, covered | masks[c], cost + costs[c], chosen);
            chosen.pop();
            if !finished {
                return false;
            }
        }
        true
    }
    let masks: Vec<u128> = covers.iter().map(|c| c.iter().fold(0u128, |m, &u| m | 1 << u)).collect();
    let mut by_elem: Vec<Vec<usize>> = vec![Vec::new(); size];
    for (c, cover) in covers.iter().enumerate() {
        for &u in cover {
            by_elem[u].push(c);
        }
    }
    // cheapest first, so the first cover found is already a tight bound
    for list in &mut by_elem {
        list.sort_by_key(|&c| (costs[c], c));
    }
    let full = if size == 128 { u128::MAX } else { (1u128 << size) - 1 };
    let mut search = Search { masks: &masks, costs, by_elem: &by_elem, full, best: None, steps: 0 };
    let finished = go(&mut search, 0, 0, &mut Vec::new());
    finished.then(|| search.best.map(|(_, chosen)| chosen).unwrap_or_default())
}

// a cover built by taking whichever candidate buys the most uncovered elements per unit of cost - the wider one on a tie
fn greedy_cover(covers: &[BTreeSet<usize>], costs: &[u64], size: usize) -> Vec<usize> {
    let mut left: BTreeSet<usize> = (0..size).collect();
    let mut picked = Vec::new();
    while !left.is_empty() {
        let best = (0..covers.len())
            .map(|c| (c, covers[c].intersection(&left).count()))
            .filter(|&(_, gain)| gain > 0)
            .max_by(|&(a, ga), &(b, gb)| {
                (ga as u128 * costs[b] as u128)
                    .cmp(&(gb as u128 * costs[a] as u128))
                    .then(ga.cmp(&gb))
                    .then(b.cmp(&a))
            });
        let Some((c, _)) = best else { break };
        left.retain(|u| !covers[c].contains(u));
        picked.push(c);
    }
    picked
}

// every record the project's sources hold, tied to the map's definitions
pub fn load(root: &Path, configured: &[Source], caches: &[FileCache]) -> Observed {
    let mut wanted: Vec<(String, Option<String>)> = configured
        .iter()
        .map(|s| {
            let (at, auth) = s.parts();
            (at.to_string(), auth.map(str::to_string))
        })
        .collect();
    wanted.extend(EXTRA.get().into_iter().flatten().map(|at| (at.clone(), None)));
    if wanted.is_empty() && root.join(LOCAL_DIR).is_dir() {
        wanted.push((LOCAL_DIR.to_string(), None));
    }
    let mut out = Observed::default();
    let mut records = Vec::new();
    for (at, auth) in wanted {
        match read(root, &at, auth.as_deref()) {
            Ok(texts) => {
                for (origin, text) in &texts {
                    parse(origin, text, &mut records, &mut out);
                }
            }
            Err(e) => out.errors.push(format!("{at}: {e:#}")),
        }
        out.sources.push(at);
    }
    out.records = records.len();
    tie(root, caches, &records, &mut out);
    out
}

// the texts one source holds, each with where it came from
fn read(root: &Path, at: &str, auth: Option<&str>) -> Result<Vec<(String, Arc<String>)>> {
    if at.starts_with("https://") || at.starts_with("http://") {
        return Ok(vec![(at.to_string(), fetched(at, auth)?)]);
    }
    let path = root.join(at.trim_start_matches("file://"));
    if !path.is_dir() {
        return Ok(vec![(path.display().to_string(), Arc::new(read_file(&path)?))]);
    }
    let mut files: Vec<_> = std::fs::read_dir(&path)
        .with_context(|| format!("reading {}", path.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "jsonl" || x == "json"))
        .collect();
    files.sort();
    files.iter().map(|p| Ok((p.display().to_string(), Arc::new(read_file(p)?)))).collect()
}

fn read_file(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if meta.len() > MAX_RECORD_BYTES {
        bail!("{} is {} bytes, over the {MAX_RECORD_BYTES}-byte record limit", path.display(), meta.len());
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

// a url's records, fetched at most once per `FETCH_TTL`
fn fetched(url: &str, auth: Option<&str>) -> Result<Arc<String>> {
    let cache = FETCHED.get_or_init(Default::default);
    if let Some((at, text)) = cache.lock().unwrap_or_else(|p| p.into_inner()).get(url) {
        if at.elapsed() < FETCH_TTL {
            return Ok(text.clone());
        }
    }
    let text = Arc::new(crate::externals::fetch_limited(url, auth, MAX_RECORD_BYTES)?);
    cache.lock().unwrap_or_else(|p| p.into_inner()).insert(url.to_string(), (Instant::now(), text.clone()));
    Ok(text)
}

// each line of `text` that is a record - the rest are counted and the first few named
fn parse(origin: &str, text: &str, records: &mut Vec<Record>, out: &mut Observed) {
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let problem = match serde_json::from_str::<Record>(line) {
            Ok(r) if r.schema == SCHEMA => {
                records.push(r);
                continue;
            }
            Ok(r) => format!("schema '{}', expected '{SCHEMA}'", r.schema),
            Err(e) => e.to_string(),
        };
        out.bad_lines += 1;
        if out.errors.len() < MAX_ERRORS {
            out.errors.push(format!("{origin}:{}: {problem}", n + 1));
        }
    }
}

// each record's functions as definitions in the map, and its fixture merged with every other record naming it
fn tie(root: &Path, caches: &[FileCache], records: &[Record], out: &mut Observed) {
    let paths: Vec<String> = caches.iter().map(|c| path_str(&c.rel_path)).collect();
    let by_path: BTreeMap<&str, usize> = paths.iter().enumerate().map(|(i, p)| (p.as_str(), i)).collect();
    let mut located: BTreeMap<&str, Option<usize>> = BTreeMap::new();
    let mut moved: BTreeMap<&str, Option<BTreeSet<String>>> = BTreeMap::new();
    let mut keys: BTreeMap<(Option<&str>, Option<&str>, &str), usize> = BTreeMap::new();
    for r in records {
        let fx = &r.fixture;
        let key = (fx.language.as_deref(), fx.file.as_deref(), fx.name.as_str());
        let at = *keys.entry(key).or_insert_with(|| {
            let line = fx
                .file
                .as_deref()
                .and_then(|f| by_path.get(f))
                .and_then(|&i| caches[i].funcs.iter().find(|f| f.name == fx.name))
                .map_or(0, |f| f.line);
            out.fixtures.push(Fixture {
                name: fx.name.clone(),
                file: fx.file.clone(),
                line,
                language: fx.language.clone(),
                kind: fx.kind,
                duration_ms: None,
                runs: BTreeSet::new(),
                services: BTreeSet::new(),
            });
            out.fixtures.len() - 1
        });
        let fixture = &mut out.fixtures[at];
        fixture.duration_ms = fixture.duration_ms.max(r.duration_ms);
        fixture.runs.extend(r.run.clone());
        fixture.services.extend(r.service.as_ref().and_then(|s| s.name.clone()));

        // the files changed since the recording, where its line numbers no longer hold
        let commit = r.service.as_ref().and_then(|s| s.commit.as_deref());
        let changed = commit.map(|c| &*moved.entry(c).or_insert_with(|| changed_since(root, c)));
        for file in &r.files {
            let Some(fi) = *located.entry(file.path.as_str()).or_insert_with(|| locate(&by_path, &file.path)) else {
                out.unmatched_files.insert(file.path.clone());
                continue;
            };
            let lines_hold = match changed {
                None => true,
                Some(Some(since)) => !since.contains(&paths[fi]),
                Some(None) => false,
            };
            let funcs = &caches[fi].funcs;
            let mut hits: BTreeSet<usize> = BTreeSet::new();
            for want in &file.functions {
                let found = function(funcs, want, lines_hold);
                out.unmatched += usize::from(found.is_empty());
                hits.extend(found);
            }
            // line ranges stand in for functions only when the record names none
            if file.functions.is_empty() {
                for &[a, b] in &file.lines {
                    let found: Vec<usize> = if lines_hold { [a, b].iter().filter_map(|&l| innermost(funcs, l)).collect() } else { Vec::new() };
                    out.unmatched += usize::from(found.is_empty());
                    hits.extend(found);
                }
            }
            for k in hits {
                out.ran.entry((fi, k)).or_default().insert(at);
            }
        }
    }
}

// the definitions in one file a record's function names - by name, then owner, then nearest line
fn function(funcs: &[Func], want: &FuncIn, lines_hold: bool) -> Vec<usize> {
    let (hinted, bare) = split_name(&want.name);
    let owner = want.owner.as_deref().map(bare_owner).or(hinted);
    let named: Vec<usize> = (0..funcs.len()).filter(|&k| funcs[k].name == bare).collect();
    let owns = |k: &usize| funcs[*k].owner.as_deref().map(bare_owner) == owner;
    let owned: Vec<usize> = if owner.is_some() && named.iter().any(owns) {
        named.into_iter().filter(owns).collect()
    } else {
        named
    };
    match (owned.len(), want.line) {
        (0, Some(line)) if lines_hold => innermost(funcs, line).into_iter().collect(),
        (n, Some(line)) if n > 1 => owned.into_iter().min_by_key(|&k| funcs[k].start_line.abs_diff(line)).into_iter().collect(),
        _ => owned,
    }
}

// `Billing.charge`, `acme::Server::charge`, `(*Server).Charge` - the owner a qualified name carries, and the bare name
fn split_name(name: &str) -> (Option<&str>, &str) {
    match name.rfind(['.', ':']) {
        Some(at) => {
            let owner = bare_owner(name[..at].trim_end_matches([':', '.']));
            ((!owner.is_empty()).then_some(owner), &name[at + 1..])
        }
        None => (None, name),
    }
}

// `pkg.(*Server)` and `acme::Server` both own as `Server`
fn bare_owner(owner: &str) -> &str {
    let last = owner.rsplit(['.', ':']).find(|s| !s.is_empty()).unwrap_or(owner);
    last.trim_matches(|c: char| matches!(c, '(' | ')' | '*' | '&'))
}

// the smallest definition whose span holds `line`
fn innermost(funcs: &[Func], line: usize) -> Option<usize> {
    (0..funcs.len())
        .filter(|&k| funcs[k].start_line <= line && line <= funcs[k].end_line)
        .min_by_key(|&k| funcs[k].end_line - funcs[k].start_line)
}

// the map's file a record path names - a relative path as written, an absolute one by the tail inside this checkout
fn locate(by_path: &BTreeMap<&str, usize>, path: &str) -> Option<usize> {
    let norm = path.replace('\\', "/");
    let mut rest = norm.trim_start_matches("./");
    if let Some(&i) = by_path.get(rest) {
        return Some(i);
    }
    // only an absolute path is another checkout of this repository - a relative one that misses is another repository's
    if !(norm.starts_with('/') || norm.as_bytes().get(1) == Some(&b':')) {
        return None;
    }
    loop {
        rest = rest.split_once('/')?.1;
        if let Some(&i) = by_path.get(rest) {
            return Some(i);
        }
    }
}

// the files that differ between `commit` and the working tree - `None` when git does not know the commit
fn changed_since(root: &Path, commit: &str) -> Option<BTreeSet<String>> {
    // a record may come from anywhere, so only a hex id ever reaches git's command line
    if !(7..=64).contains(&commit.len()) || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let out = Command::new("git").args(["diff", "--name-only", "--no-renames", commit, "--"]).current_dir(root).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;
    use std::path::PathBuf;

    // a throwaway directory that cleans up on drop
    struct Dir(PathBuf);
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn project(tag: &str, files: &[(&str, &str)]) -> (Dir, Vec<FileCache>) {
        let dir = Dir(std::env::temp_dir().join(format!("ccc-observed-{tag}-{}", std::process::id())));
        let _ = std::fs::remove_dir_all(&dir.0);
        for (name, body) in files {
            let to = dir.0.join(name);
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::write(&to, body).unwrap();
        }
        let found = scan::collect_files(&dir.0).unwrap();
        let caches = scan::build_caches(&dir.0, &found);
        (dir, caches)
    }

    fn def(caches: &[FileCache], file: &str, name: &str) -> DefId {
        let fi = caches.iter().position(|c| path_str(&c.rel_path) == file).unwrap();
        (fi, caches[fi].funcs.iter().position(|f| f.name == name).unwrap())
    }

    fn observed(root: &Path, caches: &[FileCache], lines: &[&str]) -> Observed {
        let mut out = Observed::default();
        let mut records = Vec::new();
        parse("test", &lines.join("\n"), &mut records, &mut out);
        out.records = records.len();
        tie(root, caches, &records, &mut out);
        out
    }

    fn names(o: &Observed, sel: &Selection) -> Vec<String> {
        sel.fixtures.iter().map(|&f| o.fixtures[f].name.clone()).collect()
    }

    const BILLING: &str = "package billing\n\
        type Server struct{}\n\
        type Ledger struct{}\n\
        func (s *Server) Charge(n int) int { return n + fee() }\n\
        func (l *Ledger) Charge(n int) int { return n }\n\
        func fee() int { return 30 }\n";

    #[test]
    fn a_record_ties_each_function_to_its_definition_by_name_and_owner() {
        let (dir, caches) = project("tie", &[("billing/server.go", BILLING)]);
        let o = observed(&dir.0, &caches, &[
            r#"{"schema":"ccc-observed/1","fixture":{"name":"TestRefund","kind":"contract","language":"go"},"run":"ci-1","files":[{"path":"billing/server.go","functions":[{"name":"(*Server).Charge"},{"name":"fee"}]}]}"#,
        ]);
        assert_eq!(o.records, 1);
        assert_eq!(o.unmatched, 0);
        let ran: BTreeSet<DefId> = o.ran.keys().copied().collect();
        let server = caches[0].funcs.iter().position(|f| f.name == "Charge" && f.owner.as_deref() == Some("Server")).unwrap();
        assert_eq!(ran, BTreeSet::from([(0, server), def(&caches, "billing/server.go", "fee")]), "the ledger's Charge never ran");
    }

    #[test]
    fn a_ci_checkout_path_is_found_by_its_tail_and_another_repos_path_is_not() {
        let (dir, caches) = project("paths", &[("billing/server.go", BILLING)]);
        let o = observed(&dir.0, &caches, &[
            r#"{"schema":"ccc-observed/1","fixture":{"name":"TestFee","kind":"unit"},"files":[{"path":"/builds/acme/billing/billing/server.go","functions":[{"name":"fee"}]},{"path":"payments/billing/server.go","functions":[{"name":"fee"}]}]}"#,
        ]);
        assert!(o.ran.contains_key(&def(&caches, "billing/server.go", "fee")));
        assert_eq!(o.unmatched_files, BTreeSet::from(["payments/billing/server.go".to_string()]));
    }

    #[test]
    fn lines_stand_in_for_functions_only_where_the_code_has_not_moved() {
        let (dir, caches) = project("lines", &[("billing/server.go", BILLING)]);
        // no commit, so the lines are taken as they are
        let o = observed(&dir.0, &caches, &[
            r#"{"schema":"ccc-observed/1","fixture":{"name":"TestFee","kind":"unit"},"files":[{"path":"billing/server.go","lines":[[6,6]]}]}"#,
        ]);
        assert_eq!(o.ran.keys().copied().collect::<Vec<_>>(), vec![def(&caches, "billing/server.go", "fee")]);
        // a commit git cannot place might have numbered every line differently
        let o = observed(&dir.0, &caches, &[
            r#"{"schema":"ccc-observed/1","fixture":{"name":"TestFee","kind":"unit"},"service":{"commit":"deadbeef"},"files":[{"path":"billing/server.go","lines":[[6,6]]}]}"#,
        ]);
        assert!(o.ran.is_empty());
        assert_eq!(o.unmatched, 1);
    }

    #[test]
    fn a_commit_that_is_not_an_object_id_never_reaches_git() {
        assert!(changed_since(Path::new("."), "--output=/tmp/x").is_none());
        assert!(changed_since(Path::new("."), "HEAD").is_none());
    }

    #[test]
    fn records_of_one_fixture_merge_and_lines_that_are_not_records_are_counted() {
        let (dir, caches) = project("merge", &[("billing/server.go", BILLING)]);
        let o = observed(&dir.0, &caches, &[
            r#"{"schema":"ccc-observed/1","fixture":{"name":"TestRefund","kind":"integration","language":"go"},"run":"ci-1","service":{"name":"billing"},"duration_ms":900,"files":[{"path":"billing/server.go","functions":[{"name":"fee"}]}]}"#,
            r#"{"schema":"ccc-observed/1","fixture":{"name":"TestRefund","kind":"integration","language":"go"},"run":"ci-1","service":{"name":"ledger"},"duration_ms":1200,"files":[]}"#,
            r#"{"schema":"ccc-observed/2","fixture":{"name":"TestOther","kind":"unit"}}"#,
            "not json",
        ]);
        assert_eq!(o.records, 2);
        assert_eq!(o.bad_lines, 2);
        assert_eq!(o.fixtures.len(), 1);
        let f = &o.fixtures[0];
        assert_eq!(f.duration_ms, Some(1200));
        assert_eq!(f.services, BTreeSet::from(["billing".to_string(), "ledger".to_string()]));
        assert_eq!(f.cost(), 1200 * 4);
    }

    #[test]
    fn safe_takes_every_fixture_that_ran_the_change_and_minimal_the_cheapest_cover() {
        let (dir, caches) = project("select", &[("billing/server.go", BILLING)]);
        let fee = def(&caches, "billing/server.go", "fee");
        let charge = caches[0].funcs.iter().position(|f| f.name == "Charge" && f.owner.as_deref() == Some("Server")).unwrap();
        let rec = |name: &str, kind: &str, ms: u64, funcs: &str| {
            format!(r#"{{"schema":"ccc-observed/1","fixture":{{"name":"{name}","kind":"{kind}","language":"go"}},"duration_ms":{ms},"files":[{{"path":"billing/server.go","functions":[{funcs}]}}]}}"#)
        };
        let lines = [
            // runs both changed functions, but as a smoke test that takes a minute
            rec("TestSmoke", "smoke", 60_000, r#"{"name":"fee"},{"name":"(*Server).Charge"}"#),
            rec("TestFee", "unit", 50, r#"{"name":"fee"}"#),
            rec("TestCharge", "contract", 400, r#"{"name":"(*Server).Charge"}"#),
            // ran nothing that changed
            rec("TestLedger", "unit", 10, r#"{"name":"(*Ledger).Charge"}"#),
        ];
        let o = observed(&dir.0, &caches, &lines.iter().map(String::as_str).collect::<Vec<_>>());
        let changed = [fee, (0, charge)];

        let safe = o.safe(&changed);
        assert_eq!(names(&o, &safe), vec!["TestSmoke", "TestFee", "TestCharge"]);

        let minimal = o.minimal(&changed);
        assert!(minimal.exact);
        assert_eq!(names(&o, &minimal), vec!["TestFee", "TestCharge"]);
        assert_eq!(minimal.cost, 50 + 400 * 2);
    }

    #[test]
    fn the_greedy_cover_still_covers_everything() {
        // three elements - one candidate covers two cheaply, the rest one each
        let covers: Vec<BTreeSet<usize>> = vec![BTreeSet::from([0, 1]), BTreeSet::from([1]), BTreeSet::from([2])];
        let picked = greedy_cover(&covers, &[2, 1, 5], 3);
        let covered: BTreeSet<usize> = picked.iter().flat_map(|&c| covers[c].iter().copied()).collect();
        assert_eq!(covered, BTreeSet::from([0, 1, 2]));
        assert_eq!(picked, vec![0, 2]);
        assert_eq!(exact_cover(&covers, &[2, 1, 5], 3), Some(vec![0, 2]));
    }
}
