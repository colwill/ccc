//! Structural insights over the in-memory map, for the `/insights` web UI.
//!
//! Everything here is derived from the syntax tree: the call graph, per-function
//! metrics (`model::FuncMetrics`), and the service globs `changes` already uses.
//! There is no type inference, no data flow and no runtime profile, so the
//! findings are *heuristics with evidence attached* - each one names the file,
//! line and measurement it came from so a reader can check it. The UI is
//! labelled accordingly; do not present these as proofs.

use crate::extract::TOP_LEVEL;
use crate::languages::Language;
use crate::model::{FileCache, Func, FuncMetrics};
use crate::changes::{self, ChangesConfig};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::time::Instant;

pub const SCHEMA: &str = "ccc-insights/v1";

// bounds so a large repo cannot produce an unbounded page; every cap that
// actually bites is reported in the payload as `truncated`
const FLAME_NODES: usize = 1200;
const FLAME_DEPTH: usize = 14;
const TOP_N: usize = 25;
const MAX_LINTS: usize = 400;
// per-service flame graphs, so a wide service map cannot blow up the page
const MAX_FLAME_GROUPS: usize = 12;
// call sites listed per service edge in the explorer
const MAX_EDGE_SITES: usize = 60;
// recommended test targets kept in the payload
const MAX_TARGETS: usize = 60;
// how many facades an import is chased through (`__main__` -> package
// `__init__` -> the module that defines the name). Bounded so a barrel that
// re-exports a barrel cannot walk the whole tree.
const MAX_FACADE_HOPS: usize = 3;
// file stems that stand for their directory rather than for themselves
const FACADE_STEMS: &[&str] = &["__init__", "index", "mod"];

// one function definition, addressed by (file, index into that file's funcs).
// An index one past the end of that slice addresses the file's module scope
// instead - see `Graph::module_frames`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct NodeId(usize, usize);

struct Graph<'a> {
    caches: &'a [FileCache],
    nodes: Vec<NodeId>,
    // synthetic frames for code that runs at a file's top level
    module_frames: BTreeMap<usize, Func>,
    // adjacency over positions in `nodes`
    out: Vec<BTreeSet<usize>>,
    into: Vec<BTreeSet<usize>>,
    // call sites counted per target, so a function called twice from one place
    // still ranks above one called once
    call_sites: Vec<usize>,
}

impl<'a> Graph<'a> {
    fn name(&self, i: usize) -> &str {
        &self.func(i).name
    }
    fn file(&self, i: usize) -> String {
        changes::path_str(&self.caches[self.nodes[i].0].rel_path)
    }
    fn func(&self, i: usize) -> &Func {
        let NodeId(f, k) = self.nodes[i];
        self.caches[f].funcs.get(k).unwrap_or_else(|| &self.module_frames[&f])
    }
    fn lang(&self, i: usize) -> Language {
        self.caches[self.nodes[i].0].language
    }
    fn node_file(&self, i: usize) -> usize {
        self.nodes[i].0
    }
    // nothing but itself calls this: an entry point
    fn is_root(&self, i: usize) -> bool {
        self.into[i].iter().all(|&c| c == i)
    }
    fn is_test(&self, i: usize) -> bool {
        self.func(i).test_ctx || changes::is_test_path(&self.file(i))
    }
    // a file's module scope rather than a function someone defined
    fn is_module(&self, i: usize) -> bool {
        let NodeId(f, k) = self.nodes[i];
        k >= self.caches[f].funcs.len()
    }
}

// Resolve calls to function definitions.
//
// Same-file calls come from `FileCache.refs`, which the extractor already
// resolved exactly. Cross-file calls apply the same evidence rule as
// `serve::q_dependencies` - the qualifier names the target file's module, or
// the callee is imported from it - but at function granularity rather than
// file granularity. A call with no evidence, or with evidence for more than
// one target, produces no edge: an absent edge is better than a wrong one.
fn build_graph<'a>(caches: &'a [FileCache]) -> Graph<'a> {
    let mut nodes = Vec::new();
    // (file, name) -> every node with that name, in definition order. A name
    // is not unique within a file: overloads share one, and so does an
    // interface method and the class method implementing it.
    let mut by_file_name: BTreeMap<(usize, &str), Vec<usize>> = BTreeMap::new();
    let mut by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();

    for (fi, c) in caches.iter().enumerate() {
        for (ki, f) in c.funcs.iter().enumerate() {
            let pos = nodes.len();
            nodes.push(NodeId(fi, ki));
            by_file_name.entry((fi, f.name.as_str())).or_default().push(pos);
            by_name.entry(f.name.as_str()).or_default().push(pos);
        }
    }

    // One frame per file that calls anything from its top level
    let mut module_frames: BTreeMap<usize, Func> = BTreeMap::new();
    for (fi, c) in caches.iter().enumerate() {
        if !c.calls.iter().any(|call| call.caller == TOP_LEVEL) {
            continue;
        }
        by_file_name.insert((fi, TOP_LEVEL), vec![nodes.len()]);
        nodes.push(NodeId(fi, c.funcs.len()));
        module_frames.insert(
            fi,
            Func {
                line: 1,
                col: 1,
                name: TOP_LEVEL.to_string(),
                ret: None,
                comment: None,
                start_line: 1,
                end_line: c.lines,
                test_ctx: false,
                owner: None,
                param_types: Vec::new(),
                // nobody wrote this frame, so it carries no measurements of its
                // own; the calls it makes are the whole of what it contributes
                metrics: FuncMetrics::default(),
            },
        );
    }

    let n = nodes.len();
    let mut g = Graph {
        caches,
        nodes,
        module_frames,
        out: vec![BTreeSet::new(); n],
        into: vec![BTreeSet::new(); n],
        call_sites: vec![0; n],
    };

    // Every name a qualifier could use to reach a file: its stem, the modules
    // it declares (go `package`, c++ `namespace`, rust `mod`), the types it
    // defines, and its own directory names. Generic path segments are excluded
    // so `src::foo` cannot match every file in the tree.
    const GENERIC_DIRS: &[&str] = &[
        "src", "pkg", "internal", "cmd", "app", "lib", "test", "tests", "include",
    ];
    let stems: Vec<&str> = caches
        .iter()
        .map(|c| c.rel_path.file_stem().and_then(|s| s.to_str()).unwrap_or(""))
        .collect();
    let aliases: Vec<BTreeSet<String>> = caches
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut set: BTreeSet<String> = BTreeSet::new();
            set.insert(stems[i].to_string());
            set.extend(c.modules.iter().cloned());
            set.extend(c.types.iter().map(|t| t.name.clone()));
            // directory names, so a qualifier can name the service/package dir
            if let Some(parent) = c.rel_path.parent() {
                for comp in parent.components() {
                    if let Some(d) = comp.as_os_str().to_str() {
                        if !GENERIC_DIRS.contains(&d) {
                            set.insert(d.to_string());
                        }
                    }
                }
            }
            set.remove("");
            set
        })
        .collect();
    // (owning type, method) -> nodes, for receiver-typed calls
    let mut by_owner: BTreeMap<(&str, &str), Vec<usize>> = BTreeMap::new();
    for (pos, NodeId(fi, ki)) in g.nodes.iter().copied().enumerate() {
        // module frames are past the end of `funcs` and own no method
        let Some(f) = caches[fi].funcs.get(ki) else { continue };
        if let Some(owner) = f.owner.as_deref() {
            by_owner.entry((owner, f.name.as_str())).or_default().push(pos);
        }
    }
    // per file: which name was imported from which files
    let mut imported: Vec<BTreeMap<&str, BTreeSet<usize>>> = vec![BTreeMap::new(); caches.len()];
    let mut stem_files: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, s) in stems.iter().enumerate() {
        stem_files.entry(s).or_default().push(i);
    }
    // a facade is imported under its directory
    for (i, c) in caches.iter().enumerate() {
        if !FACADE_STEMS.contains(&stems[i]) {
            continue;
        }
        if let Some(dir) = c.rel_path.parent().and_then(Path::file_name).and_then(|d| d.to_str()) {
            stem_files.entry(dir).or_default().push(i);
        }
    }
    for (a, c) in caches.iter().enumerate() {
        let files_named = |s: &str| {
            stem_files
                .get(s)
                .into_iter()
                .flatten()
                .copied()
                .filter(|&b| b != a)
                .collect::<Vec<usize>>()
        };
        for imp in &c.imports {
            let segs: Vec<&str> = imp
                .module
                .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
                .filter(|s| !s.is_empty())
                .collect();
            // The last segment names the module the statement actually reaches;
            // the ones before it are the packages on the way there. Preferring
            // it keeps `from mypkg.cli import main` pointed at `cli.py` even
            // when the package root defines a `main` of its own. Earlier
            // segments still get their say when the last matches nothing, which
            // is what carries a C++ `#include "foo/bar.h"` to `bar`.
            let mut targets: BTreeSet<usize> = segs
                .last()
                .map(|s| files_named(s))
                .unwrap_or_default()
                .into_iter()
                .collect();
            if targets.is_empty() {
                targets.extend(segs.iter().flat_map(|s| files_named(s)));
            }
            // `from pkg import cli` binds a name that is itself a module
            targets.extend(imp.names.iter().flat_map(|n| files_named(n)));
            for name in &imp.names {
                imported[a].entry(name.as_str()).or_default().extend(&targets);
            }
            // An import that binds no names is not empty of meaning - it makes
            // a whole file's surface available instead of picking from it. A C
            // or C++ `#include` is the case that matters most, since the
            // language has no other import form, and without this every call
            // into another translation unit is unresolvable. A Rust `use m::*`
            // and a plain C# `using Lib;` say the same thing and are treated
            // the same way. The single-candidate rule below still applies, so
            // widening what is available cannot invent an ambiguous edge.
            if imp.names.is_empty() {
                for &b in &targets {
                    for f in &caches[b].funcs {
                        imported[a].entry(f.name.as_str()).or_default().insert(b);
                    }
                }
            }
        }
    }
    // Chase each binding through the facades it passes: the name `__main__.py`
    // imported from `mypkg` is one `mypkg/__init__.py` imported from
    // `mypkg/cli.py`, and the definition is in the latter. Only files that
    // import the same name are followed, and the evidence test below still
    // requires the file it lands on to define that name - so this can widen the
    // search without loosening what counts as proof.
    for _ in 0..MAX_FACADE_HOPS {
        let mut grew = false;
        for a in 0..caches.len() {
            for name in imported[a].keys().copied().collect::<Vec<&str>>() {
                let hops: BTreeSet<usize> = imported[a][name]
                    .iter()
                    .filter_map(|&b| imported[b].get(name))
                    .flatten()
                    .copied()
                    .filter(|&b| b != a)
                    .collect();
                let reached = imported[a].entry(name).or_default();
                let before = reached.len();
                reached.extend(hops);
                grew |= reached.len() != before;
            }
        }
        if !grew {
            break;
        }
    }

    // Which definition of `name` in file `fi` a call on `line` belongs to.
    // With one candidate this is the old behaviour; with several - an overload
    // set, or an interface method beside the class method implementing it -
    // taking the first would credit the wrong one, so the innermost definition
    // whose body actually spans the call wins.
    let owner_of = |fi: usize, name: &str, line: usize| -> Option<usize> {
        let cands = by_file_name.get(&(fi, name))?;
        if let [only] = cands[..] {
            return Some(only);
        }
        let spans = |&p: &usize| -> Option<(usize, usize)> {
            let NodeId(f, k) = g.nodes[p];
            let d = caches[f].funcs.get(k)?;
            Some((d.start_line, d.end_line))
        };
        cands
            .iter()
            .filter(|p| spans(p).is_some_and(|(s, e)| s <= line && line <= e))
            .max_by_key(|p| spans(p).map(|(s, _)| s))
            .copied()
            .or_else(|| cands.first().copied())
    };

    // same-file edges, already resolved by the extractor
    for (fi, c) in caches.iter().enumerate() {
        for r in &c.refs {
            let (Some(from), Some(to)) = (
                owner_of(fi, r.caller.as_str(), r.call_line),
                by_file_name.get(&(fi, r.target_name.as_str())).and_then(|v| v.first().copied()),
            ) else {
                continue;
            };
            g.out[from].insert(to);
            g.into[to].insert(from);
            g.call_sites[to] += 1;
        }
    }

    // cross-file edges, with evidence
    for (a, c) in caches.iter().enumerate() {
        for call in &c.calls {
            let Some(from) = owner_of(a, call.caller.as_str(), call.line) else {
                continue;
            };
            let candidates = by_name.get(call.name.as_str());
            if call.recv_type.is_none() && by_file_name.contains_key(&(a, call.name.as_str())) {
                continue; // resolved locally by the refs pass
            }
            let qual: Vec<&str> = call
                .qualifier
                .as_deref()
                .map(|q| {
                    q.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            // The receiver's declared type addresses the method exactly, which
            // beats any name-based guess. Same-file targets count too: the
            // `refs` pass resolves plain calls, but `local.method()` needs the
            // local's type to know which method it means.
            if let Some(ty) = call.recv_type.as_deref() {
                if let Some([to]) = by_owner.get(&(ty, call.name.as_str())).map(|v| &v[..]) {
                    let to = *to;
                    if to != from && g.out[from].insert(to) {
                        g.into[to].insert(from);
                        g.call_sites[to] += 1;
                    }
                    continue;
                }
            }
            let Some(candidates) = candidates else { continue };
            let evidenced: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|&to| {
                    let b = g.nodes[to].0;
                    if b == a {
                        return false;
                    }
                    let by_qualifier = qual.iter().any(|q| aliases[b].contains(*q));
                    let by_import = imported[a]
                        .get(call.name.as_str())
                        .is_some_and(|s| s.contains(&b));
                    by_qualifier || by_import
                })
                .collect();
            if let [to] = evidenced[..] {
                g.out[from].insert(to);
                g.into[to].insert(from);
                g.call_sites[to] += 1;
            }
        }
    }
    g
}

// Expand the call graph into a tree for the flame view. Each node's `value` is
// its own weight plus its children's, which is what gives a flame chart its
// nesting; because this is a static graph and not a profile, `value` counts
// *reachable call sites*, not time. Recursion is cut at the repeat, and the
// whole expansion is bounded by FLAME_NODES / FLAME_DEPTH.
fn flame(
    g: &Graph,
    ctx: &ServiceCtx,
    roots: &[usize],
    budget: &mut usize,
) -> (Vec<Value>, bool) {
    let mut truncated = false;
    let mut out = Vec::new();
    for &r in roots {
        if *budget == 0 {
            truncated = true;
            break;
        }
        let mut path = BTreeSet::new();
        let (node, cut) = flame_node(g, ctx, r, 0, None, &mut path, budget);
        truncated |= cut;
        out.push(node);
    }
    (out, truncated)
}

fn flame_node(
    g: &Graph,
    ctx: &ServiceCtx,
    i: usize,
    depth: usize,
    parent_service: Option<&str>,
    path: &mut BTreeSet<usize>,
    budget: &mut usize,
) -> (Value, bool) {
    *budget = budget.saturating_sub(1);
    let mut truncated = false;
    let recursive = !path.insert(i);
    let mut children = Vec::new();
    let mut value = 1usize;
    let service = ctx.of_node(g, i);

    if !recursive && depth < FLAME_DEPTH {
        for &c in &g.out[i] {
            if *budget == 0 {
                truncated = true;
                break;
            }
            let (child, cut) = flame_node(g, ctx, c, depth + 1, service, path, budget);
            truncated |= cut;
            value += child["value"].as_u64().unwrap_or(1) as usize;
            children.push(child);
        }
    } else if !g.out[i].is_empty() {
        truncated = true;
    }
    if !recursive {
        path.remove(&i);
    }

    // the call into this frame left the caller's service: the frame where a
    // change stops being local
    let crosses = match (parent_service, service) {
        (Some(p), Some(s)) => p != s,
        _ => false,
    };
    let f = g.func(i);
    (
        json!({
            "name": g.name(i),
            "file": g.file(i),
            "line": f.line,
            "value": value,
            "self": 1,
            "lines": f.metrics.body_lines,
            "complexity": f.metrics.complexity(),
            "recursive": recursive || f.metrics.recursive,
            "service": service,
            "crosses": crosses,
            "children": children,
        }),
        truncated,
    )
}

// Longest acyclic call chains, deepest first. "Hot" here is structural: a long
// chain that many callers funnel into is where a change ripples furthest. It is
// not an execution frequency - nothing is measured at runtime.
fn deepest_chains(g: &Graph, roots: &[usize]) -> Vec<Value> {
    let mut chains: Vec<(usize, usize, Vec<usize>)> = Vec::new();
    for &r in roots {
        let mut best = Vec::new();
        let mut path = Vec::new();
        let mut on_path = BTreeSet::new();
        longest(g, r, &mut path, &mut on_path, &mut best, &mut 20_000);
        if best.len() > 1 {
            let weight = best.iter().map(|&i| g.call_sites[i]).sum();
            chains.push((best.len(), weight, best));
        }
    }
    chains.sort_by(|a, b| (b.0, b.1).cmp(&(a.0, a.1)));
    chains.truncate(TOP_N);
    chains
        .into_iter()
        .map(|(depth, weight, nodes)| {
            json!({
                "depth": depth,
                "call_sites": weight,
                "chain": nodes.iter().map(|&i| json!({
                    "name": g.name(i),
                    "file": g.file(i),
                    "line": g.func(i).line,
                    "complexity": g.func(i).metrics.complexity(),
                })).collect::<Vec<_>>(),
            })
        })
        .collect()
}

fn longest(
    g: &Graph,
    i: usize,
    path: &mut Vec<usize>,
    on_path: &mut BTreeSet<usize>,
    best: &mut Vec<usize>,
    steps: &mut usize,
) {
    if *steps == 0 {
        return;
    }
    *steps -= 1;
    if !on_path.insert(i) {
        return; // cycle
    }
    path.push(i);
    if path.len() > best.len() {
        *best = path.clone();
    }
    for &c in &g.out[i] {
        longest(g, c, path, on_path, best, steps);
    }
    path.pop();
    on_path.remove(&i);
}

// Strongly connected components larger than one node: mutual recursion, which
// is where static call trees stop being trees.
fn cycles(g: &Graph) -> Vec<Value> {
    let n = g.nodes.len();
    let mut order = Vec::new();
    let mut seen = vec![false; n];
    for s in 0..n {
        if seen[s] {
            continue;
        }
        // iterative post-order so deep graphs cannot blow the stack
        let mut stack = vec![(s, false)];
        while let Some((v, expanded)) = stack.pop() {
            if expanded {
                order.push(v);
                continue;
            }
            if seen[v] {
                continue;
            }
            seen[v] = true;
            stack.push((v, true));
            for &c in &g.out[v] {
                if !seen[c] {
                    stack.push((c, false));
                }
            }
        }
    }
    let mut comp = vec![usize::MAX; n];
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for &s in order.iter().rev() {
        if comp[s] != usize::MAX {
            continue;
        }
        let id = groups.len();
        let mut group = Vec::new();
        let mut queue = VecDeque::from([s]);
        comp[s] = id;
        while let Some(v) = queue.pop_front() {
            group.push(v);
            for &p in &g.into[v] {
                if comp[p] == usize::MAX {
                    comp[p] = id;
                    queue.push_back(p);
                }
            }
        }
        groups.push(group);
    }
    let mut out: Vec<Value> = groups
        .into_iter()
        .filter(|grp| grp.len() > 1)
        .map(|grp| {
            json!({
                "size": grp.len(),
                "members": grp.iter().take(12).map(|&i| json!({
                    "name": g.name(i), "file": g.file(i), "line": g.func(i).line,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    out.sort_by_key(|v| std::cmp::Reverse(v["size"].as_u64().unwrap_or(0)));
    out.truncate(TOP_N);
    out
}

struct Lint {
    rule: &'static str,
    severity: &'static str,
    file: String,
    line: usize,
    function: String,
    language: &'static str,
    message: String,
    hint: String,
}

impl Lint {
    fn json(&self) -> Value {
        json!({
            "rule": self.rule,
            "severity": self.severity,
            "file": self.file,
            "line": self.line,
            "function": self.function,
            "language": self.language,
            "message": self.message,
            "hint": self.hint,
        })
    }
}

// Rule catalogue, published with the payload so the UI can explain what was
// looked for - including in languages where a rule cannot fire.
pub fn rule_catalogue() -> Value {
    json!([
        {"rule": "inline-candidate", "severity": "info",
         "what": "Small, branch-free function called from several places.",
         "evidence": "body line count, branch count, and caller count from the call graph.",
         "limits": "Cannot see whether the compiler already inlines it."},
        {"rule": "unroll-candidate", "severity": "info",
         "what": "Loop whose trip count is an integer literal small enough to unroll.",
         "evidence": "loop header parsed to a constant bound.",
         "limits": "Only literal bounds; a const-valued bound reads as unknown. Most compilers already unroll these."},
        {"rule": "deep-loop-nest", "severity": "warn",
         "what": "Three or more nested loops in one function.",
         "evidence": "loop nesting depth from the syntax tree.",
         "limits": "Nesting depth is not asymptotic complexity - the bounds may be tiny."},
        {"rule": "leak-risk", "severity": "warn",
         "what": "Resource acquired without a matching release in the same function.",
         "evidence": "acquire/release call-name pairs for the language, balanced per release name so one `Close` discharges an `Open`, an `OpenFile` or a `Dial` alike; `with`/`defer` count as released.",
         "limits": "Name-matched and function-local, with no data flow: the release only has to be called somewhere in the same body, so it is not checked to act on the handle that was acquired, and a release reached through a differently named wrapper (`defer closeIt(f)`), a caller, or a destructor reads as missing."},
        {"rule": "high-complexity", "severity": "warn",
         "what": "Many decision points in one function.",
         "evidence": "1 + branches + loops (cyclomatic-style) from the syntax tree.",
         "limits": "Counts syntax, not paths actually reachable."},
        {"rule": "long-function", "severity": "info",
         "what": "Function definition spanning many lines.",
         "evidence": "definition span.",
         "limits": "Includes comments and nested closures."},
        {"rule": "many-params", "severity": "info",
         "what": "Long parameter list - often past the register-argument budget.",
         "evidence": "parameter-list node count.",
         "limits": "Does not know parameter sizes or the target ABI."},
        {"rule": "no-callers", "severity": "info",
         "what": "Function that nothing in the map calls.",
         "evidence": "in-degree zero in the call graph, and the name appears at no call site.",
         "limits": "Cannot see a function used as a value (`.map(f)`), an entry point, an exported API, a trait/interface impl, or dynamic dispatch - all look uncalled."},
    ])
}

fn lints(g: &Graph) -> (Vec<Value>, bool) {
    let mut out: Vec<Lint> = Vec::new();
    // every call name that appears anywhere, for the `no-callers` rule
    let mut called_names: BTreeSet<&str> = BTreeSet::new();
    for c in g.caches {
        for call in &c.calls {
            called_names.insert(call.name.as_str());
        }
    }

    for i in 0..g.nodes.len() {
        // module scope has no body of its own to measure, and "nobody calls
        // this file's top level" is not a finding
        if g.is_test(i) || g.is_module(i) {
            continue;
        }
        let f = g.func(i);
        let m = &f.metrics;
        let lang = g.lang(i);
        let (file, name) = (g.file(i), f.name.clone());
        let callers = g.into[i].len();
        let mut push = |rule, severity, line, message, hint: String| {
            out.push(Lint {
                rule,
                severity,
                file: file.clone(),
                line,
                function: name.clone(),
                language: lang.as_str(),
                message,
                hint,
            })
        };

        if m.body_lines <= 5 && m.loops.is_empty() && m.branches <= 1 && !m.recursive && callers >= 3
        {
            push(
                "inline-candidate",
                "info",
                f.line,
                format!(
                    "{} lines, {} branch(es), called from {callers} places",
                    m.body_lines, m.branches
                ),
                format!("{}: {}", lang.as_str(), lang.inline_hint()),
            );
        }
        for l in &m.loops {
            // an inner loop is the classic unroll target, so depth is reported
            // rather than filtered on
            if let Some(trip) = l.trip {
                if trip > 0 && trip <= 8 {
                    let nest = if l.depth > 1 {
                        format!(", innermost at nesting depth {}", l.depth)
                    } else {
                        String::new()
                    };
                    push(
                        "unroll-candidate",
                        "info",
                        l.line,
                        format!("{} loop with a constant trip count of {trip}{nest}", l.kind),
                        "small fixed bound; check whether the compiler already unrolls it before hand-unrolling".into(),
                    );
                }
            }
        }
        if m.max_loop_depth() >= 3 {
            let line = m.loops.iter().find(|l| l.depth >= 3).map(|l| l.line).unwrap_or(f.line);
            push(
                "deep-loop-nest",
                "warn",
                line,
                format!("{} nested loop levels", m.max_loop_depth()),
                "hoist the inner loop into its own function, or flatten the iteration".into(),
            );
        }
        // Acquires without a matching release in the same body
        let release_for = |acq: &str| {
            lang.resource_pairs()
                .iter()
                .find(|(a, _)| *a == acq)
                .map(|(_, r)| *r)
        };
        let mut balance: BTreeMap<&str, (usize, usize, usize, BTreeSet<&str>)> = BTreeMap::new();
        for r in &m.resources {
            // a release is keyed by the name actually called; an acquire by the
            // release its pair calls for
            let key = if r.acquire {
                match release_for(r.pair) {
                    Some(k) => k,
                    None => continue,
                }
            } else {
                r.name.as_str()
            };
            let e = balance.entry(key).or_default();
            match (r.acquire, r.guarded) {
                (true, false) => {
                    e.0 += 1;
                    e.3.insert(r.pair);
                }
                (true, true) => e.2 += 1,
                (false, _) => e.1 += 1,
            }
        }
        for (release, (acquires, releases, guarded, acquired_by)) in balance {
            if acquires > releases {
                let line = m
                    .resources
                    .iter()
                    .find(|r| r.acquire && !r.guarded && release_for(r.pair) == Some(release))
                    .map(|r| r.line)
                    .unwrap_or(f.line);
                // which acquires are unmatched cannot be told apart, so name
                // every one that needs this release
                let names = acquired_by.iter().copied().collect::<Vec<_>>().join("/");
                push(
                    "leak-risk",
                    "warn",
                    line,
                    format!(
                        "{} unreleased `{names}` ({releases} matching `{release}`, {guarded} auto-released)",
                        acquires - releases
                    ),
                    format!("pair it with `{release}`, or hand ownership to a caller that does"),
                );
            }
        }
        if m.complexity() >= 15 {
            push(
                "high-complexity",
                "warn",
                f.line,
                format!(
                    "complexity {} ({} branches, {} loops)",
                    m.complexity(),
                    m.branches,
                    m.loops.len()
                ),
                "split the decision points into named helpers".into(),
            );
        }
        if m.body_lines >= 120 {
            push(
                "long-function",
                "info",
                f.line,
                format!("{} lines", m.body_lines),
                "long bodies hide their own structure".into(),
            );
        }
        if m.params >= 6 {
            push(
                "many-params",
                "info",
                f.line,
                format!("{} parameters", m.params),
                "group related parameters into a struct".into(),
            );
        }
        if callers == 0 && !called_names.contains(f.name.as_str()) && f.name != "main" {
            push(
                "no-callers",
                "info",
                f.line,
                "no call site anywhere in the map".into(),
                "dead code - or passed as a function reference, exported, or dispatched dynamically, none of which the call map sees"
                    .into(),
            );
        }
    }

    // worst first, then stable by location
    let rank = |s: &str| if s == "warn" { 0 } else { 1 };
    out.sort_by(|a, b| {
        (rank(a.severity), a.rule, &a.file, a.line).cmp(&(rank(b.severity), b.rule, &b.file, b.line))
    });
    let truncated = out.len() > MAX_LINTS;
    out.truncate(MAX_LINTS);
    (out.iter().map(Lint::json).collect(), truncated)
}

// Roll the file-level call graph up into the service map `changes` already
// understands. With no `.ccc/map.json` the top-level directories stand in, so
// the tab is useful before anyone has configured anything.
// Which service each file belongs to, plus the declared dependency map. Built
// once and shared: the flame view needs it to mark service boundaries, and the
// service tab needs it to group files.
struct ServiceCtx {
    source: String,
    map: BTreeMap<String, Vec<String>>,
    deps: BTreeMap<String, Vec<String>>,
    // per cache index, the services that own that file
    of_file: Vec<Vec<String>>,
    // the grouping degenerated to one unit per file, so "service" means
    // "module" here - not a boundary worth fanning a flame graph out over
    per_file: bool,
}

impl ServiceCtx {
    // the service owning a graph node, if exactly one does
    fn of_node(&self, g: &Graph, i: usize) -> Option<&str> {
        self.of_file
            .get(g.node_file(i))
            .and_then(|v| v.first())
            .map(|s| s.as_str())
    }
}

fn service_ctx(g: &Graph, root: &Path) -> ServiceCtx {
    let cfg = ChangesConfig::load(root).unwrap_or_default();
    let paths: Vec<String> = g.caches.iter().map(|c| changes::path_str(&c.rel_path)).collect();
    let (mut map, mut source) = if cfg.services.is_empty() {
        (BTreeMap::new(), "top-level directories (no .ccc/map.json)")
    } else {
        (cfg.services.clone(), ".ccc/map.json")
    };
    // Without a config, group by the widest directory level that actually
    // splits the tree. A single-crate repo would otherwise collapse into one
    // box, which shows nothing; falling through to per-module gives the reader
    // the internal structure instead.
    if map.is_empty() {
        for depth in [1usize, 2] {
            map.clear();
            for rel in &paths {
                let segs: Vec<&str> = rel.split('/').collect();
                let (name, glob) = if segs.len() > depth {
                    let prefix = segs[..depth].join("/");
                    (prefix.clone(), format!("{prefix}/**"))
                } else {
                    ("root".to_string(), "*".to_string())
                };
                map.entry(name).or_insert_with(|| vec![glob]);
            }
            if map.len() > 1 {
                source = if depth == 1 {
                    "top-level directories (no .ccc/map.json)"
                } else {
                    "second-level directories (no .ccc/map.json)"
                };
                break;
            }
        }
        if map.len() < 2 {
            // flat single-directory project: each file is its own unit
            map.clear();
            for rel in &paths {
                map.insert(rel.clone(), vec![rel.clone()]);
            }
            source = "one unit per file (no .ccc/map.json, and the tree has no sub-directories to group by)";
        }
    }
    let of_file: Vec<Vec<String>> = match changes::build_matchers(&map) {
        Ok(matchers) => g
            .caches
            .iter()
            .map(|c| changes::assign(&matchers, &changes::path_str(&c.rel_path)))
            .collect(),
        Err(_) => vec![Vec::new(); g.caches.len()],
    };
    ServiceCtx {
        per_file: source.starts_with("one unit per file"),
        source: source.to_string(),
        map,
        deps: cfg.deps,
        of_file,
    }
}

// Roll the file-level call graph up into the service map.
fn services(g: &Graph, ctx: &ServiceCtx) -> Value {
    let (map, service_of) = (&ctx.map, &ctx.of_file);

    let mut files: BTreeMap<&String, Vec<String>> = BTreeMap::new();
    let mut funcs: BTreeMap<&String, usize> = BTreeMap::new();
    // files matching no glob: a hole in the service map, worth showing rather
    // than dropping (`changes` reports the same list as `unassigned_files`)
    let mut unassigned: Vec<String> = Vec::new();
    for (fi, c) in g.caches.iter().enumerate() {
        let rel = changes::path_str(&c.rel_path);
        if service_of[fi].is_empty() {
            unassigned.push(rel);
            continue;
        }
        for s in &service_of[fi] {
            files.entry(s).or_default().push(rel.clone());
            *funcs.entry(s).or_default() += c.funcs.len();
        }
    }

    // service -> service edges. Each crossing call is kept with both ends, so
    // the explorer can answer "what does gateway actually invoke in billing".
    #[derive(Default)]
    struct Crossing {
        sites: Vec<Value>,
        symbols: BTreeSet<String>,
    }
    let mut edges: BTreeMap<(String, String), Crossing> = BTreeMap::new();
    for from in 0..g.nodes.len() {
        for &to in &g.out[from] {
            let (a, b) = (g.node_file(from), g.node_file(to));
            for sa in &service_of[a] {
                for sb in &service_of[b] {
                    if sa == sb {
                        continue;
                    }
                    let e = edges.entry((sa.clone(), sb.clone())).or_default();
                    if e.symbols.insert(g.name(to).to_string()) && e.sites.len() < MAX_EDGE_SITES {
                        e.sites.push(json!({
                            "symbol": g.name(to),
                            "target_file": g.file(to),
                            "target_line": g.func(to).line,
                            "caller": g.name(from),
                            "caller_file": g.file(from),
                            "caller_line": g.func(from).line,
                            // what the callee goes on to invoke, so a click can
                            // keep descending without another request
                            "calls_on": g.out[to]
                                .iter()
                                .take(12)
                                .map(|&n| json!({
                                    "name": g.name(n),
                                    "file": g.file(n),
                                    "line": g.func(n).line,
                                    "service": service_of
                                        .get(g.node_file(n))
                                        .and_then(|v| v.first())
                                        .cloned(),
                                }))
                                .collect::<Vec<_>>(),
                        }));
                    }
                }
            }
        }
    }
    // `deps` in map.json declares the links static analysis cannot see (HTTP,
    // RPC, queues). They are real edges of the service graph, so they belong in
    // the same list - flagged `declared` so a reader can tell them from the
    // ones that were detected, exactly as `changes` reports them.
    let mut declared: BTreeSet<(String, String)> = BTreeSet::new();
    for (from, tos) in &ctx.deps {
        for to in tos {
            declared.insert((from.clone(), to.clone()));
            edges.entry((from.clone(), to.clone())).or_default();
        }
    }

    json!({
        "source": ctx.source,
        "declared_deps": ctx.deps,
        "services": map.keys().map(|s| json!({
            "name": s,
            "globs": map[s],
            "files": files.get(s).map(|v| v.len()).unwrap_or(0),
            "funcs": funcs.get(s).copied().unwrap_or(0),
            "paths": files.get(s).map(|v| v.iter().take(50).cloned().collect::<Vec<_>>()).unwrap_or_default(),
        })).collect::<Vec<_>>(),
        "edges": edges.iter().map(|((a, b), e)| json!({
            "from": a, "to": b,
            "declared": declared.contains(&(a.clone(), b.clone())),
            // independent of `declared`: a declared dependency is still
            // resolved, so most declared edges are also detected
            "detected": !e.symbols.is_empty(),
            "symbols": e.symbols.iter().take(12).collect::<Vec<_>>(),
            "count": e.symbols.len(),
            // full detail for the explore view
            "sites": e.sites,
        })).collect::<Vec<_>>(),
        "unassigned_files": unassigned,
    })
}

// Longest downstream call chain from each node. Cycles contribute nothing,
// so a recursive function does not report infinite depth.
fn depth_below(g: &Graph) -> Vec<usize> {
    let n = g.nodes.len();
    let mut memo = vec![usize::MAX; n];
    let mut on_path = vec![false; n];
    fn go(g: &Graph, i: usize, memo: &mut Vec<usize>, on_path: &mut Vec<bool>) -> usize {
        if on_path[i] {
            return 0; // a cycle adds no new depth
        }
        if memo[i] != usize::MAX {
            return memo[i];
        }
        on_path[i] = true;
        let mut best = 0;
        for &c in &g.out[i] {
            best = best.max(1 + go(g, c, memo, on_path));
        }
        on_path[i] = false;
        memo[i] = best;
        best
    }
    for i in 0..n {
        if memo[i] == usize::MAX {
            go(g, i, &mut memo, &mut on_path);
        }
    }
    memo
}

// Which functions any test refers to, by name. Same rule `changes` uses for
// `tested_by`: a call from a test file, a test scope, or a test-named function.
// Name-matched, so it answers "is this exercised at all", not "is its behaviour
// asserted" - the UI says so.
fn test_references(g: &Graph) -> BTreeMap<String, BTreeSet<String>> {
    test_sites(g)
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().map(|t| t.name).take(10).collect()))
        .collect()
}

// A test function, addressed well enough for a runner to select it.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TestSite {
    name: String,
    file: String,
    line: usize,
    language: &'static str,
}

// callee name -> the test functions that call it, with their locations
fn test_sites(g: &Graph) -> BTreeMap<String, BTreeSet<TestSite>> {
    let mut out: BTreeMap<String, BTreeSet<TestSite>> = BTreeMap::new();
    for c in g.caches {
        let path = changes::path_str(&c.rel_path);
        let file_is_test = changes::is_test_path(&path);
        for call in &c.calls {
            if !(file_is_test || call.test_ctx || changes::is_test_fn_name(&call.caller)) {
                continue;
            }
            if call.caller == "<top>" {
                continue; // file-level setup is not a selectable test
            }
            // the line the test is defined on, not the call
            let line = c
                .funcs
                .iter()
                .find(|f| f.name == call.caller)
                .map(|f| f.line)
                .unwrap_or(call.line);
            out.entry(call.name.clone()).or_default().insert(TestSite {
                name: call.caller.clone(),
                file: path.clone(),
                line,
                language: c.language.as_str(),
            });
        }
    }
    out
}

// The kinds of test this recommends, and what each one is for. Published with
// the payload so the tab can explain itself.
pub fn test_kind_rubric() -> Value {
    json!([
        {"kind": "smoke-test", "for": "Does it run at all?",
         "chosen_when": "Nothing stronger applies - typically an entry point or a small helper.",
         "signals": "baseline, doubled when nothing in the map calls it"},
        {"kind": "integration-test", "for": "Does it work with its collaborators?",
         "chosen_when": "It orchestrates several other functions, or sits on top of a deep chain.",
         "signals": "call-outs x4 + call depth x3 + complexity"},
        {"kind": "contract-test", "for": "Does it still honour what its callers expect?",
         "chosen_when": "It is called from another service - a boundary someone else depends on.",
         "signals": "25 per distinct calling service; language semantics sharpen the advice, since untyped callers have no compiler checking the shape"},
        {"kind": "perf-test", "for": "Does its cost grow the way you think?",
         "chosen_when": "Nested loops, or recursion over a deep chain.",
         "signals": "loop depth squared x10 (nested iteration is superlinear), plus call depth if recursive"},
        {"kind": "load-test", "for": "Does it hold up when everything calls it at once?",
         "chosen_when": "A hot spot by call sites that also does real work (loops or acquires resources).",
         "signals": "call sites x4 + loop depth x10, but only when call sites are in the top decile and it loops or acquires resources"},
    ])
}

// recommend tests for the gaps; self explanatory
fn test_targets(g: &Graph, ctx: &ServiceCtx, must_keep: &BTreeSet<(String, String)>) -> Value {
    let n = g.nodes.len();
    let depth = depth_below(g);
    let refs = test_references(g);

    // "hot" is relative to this codebase: the top decile of call sites
    let mut sites: Vec<usize> = (0..n).map(|i| g.call_sites[i]).filter(|&c| c > 0).collect();
    sites.sort_unstable();
    let hot_cut = if sites.is_empty() {
        usize::MAX
    } else {
        sites[sites.len() * 9 / 10].max(3)
    };

    let mut targets: Vec<(usize, bool, Value)> = Vec::new();
    // `i` indexes several parallel structures and the graph accessors, not just
    // one slice, so a range loop reads better than zipping them
    #[allow(clippy::needless_range_loop)]
    for i in 0..n {
        if g.is_test(i) {
            continue; // do not recommend tests for tests
        }
        if g.is_module(i) {
            continue; // there is no module scope to call from a test
        }
        let f = g.func(i);
        let m = &f.metrics;
        let lang = g.lang(i);
        let (callers, call_outs, call_sites) = (g.into[i].len(), g.out[i].len(), g.call_sites[i]);
        let (cx, loop_depth, d) = (m.complexity(), m.max_loop_depth(), depth[i]);
        let own = ctx.of_node(g, i);
        // callers that live in another service: the boundary others depend on
        let cross_callers: Vec<&str> = g.into[i]
            .iter()
            .filter_map(|&c| ctx.of_node(g, c))
            .filter(|s| Some(*s) != own)
            .collect();
        let covered_by: Vec<String> = refs
            .get(&f.name)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default();
        let covered = refs.contains_key(&f.name);

        let pinned = must_keep.contains(&(g.file(i), f.name.clone()));
        // a trivial leaf nobody calls is noise, not a gap - unless the branch
        // just changed it, in which case it is exactly the gap being looked for
        if !pinned && !covered && cx <= 1 && call_outs == 0 && callers == 0 && m.body_lines <= 3 {
            continue;
        }

        let mut why: Vec<Value> = Vec::new();
        let mut note = |factor: &str, value: usize, detail: String| {
            why.push(json!({"factor": factor, "value": value, "detail": detail}));
        };

        // Score every kind and take the strongest, rather than matching in
        // a fixed order: a recursive AST walker with 31 call-outs needs an
        // integration test, even though "recursive" also reads as a perf signal.
        let does_work = loop_depth > 0 || !m.resources.is_empty();
        let hot = call_sites >= hot_cut && does_work;
        let mut svcs: Vec<&str> = cross_callers.clone();
        svcs.sort_unstable();
        svcs.dedup();

        let scores: Vec<(&str, usize)> = vec![
            // nested iteration is superlinear, so depth counts quadratically
            ("perf-test", loop_depth * loop_depth * 10 + if m.recursive { d * 2 } else { 0 }),
            // orchestration: how much runs underneath it, and how many paths
            ("integration-test", call_outs * 4 + d * 3 + cx),
            // a boundary someone else depends on is the most expensive to break
            ("contract-test", svcs.len() * 25),
            ("load-test", if hot { call_sites * 4 + loop_depth * 10 } else { 0 }),
            ("smoke-test", 8 + if callers == 0 { 20 } else { 0 }),
        ];
        let top = scores.iter().map(|(_, v)| *v).max().unwrap_or(0).max(1);
        let mut ranked: Vec<(&str, usize)> =
            scores.into_iter().filter(|(_, v)| *v > 0).collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1));
        let kinds: Vec<&str> = ranked
            .iter()
            // a secondary kind has to be within reach of the primary to be worth naming
            .filter(|(_, v)| *v * 5 >= top * 2)
            .map(|(k, _)| *k)
            .collect();

        // the measurements behind whichever kinds were picked
        if loop_depth >= 2 {
            note("loop depth", loop_depth, format!("{loop_depth} nested loop levels"));
        }
        if m.recursive {
            note("recursion", d, format!("recursive, over a chain {d} deep"));
        }
        if !svcs.is_empty() {
            note("cross-service callers", svcs.len(), format!("called from {}", svcs.join(", ")));
        }
        if call_outs >= 3 || d >= 3 {
            note("call depth", d, format!("{call_outs} direct call-out(s), chain {d} deep"));
        }
        if hot {
            note("hot spot", call_sites,
                 format!("{call_sites} call sites from {callers} caller(s), and it loops or acquires resources"));
        }
        if callers == 0 {
            note("entry point", 0, "nothing in the map calls it".into());
        }
        if cx >= 10 {
            note("complexity", cx, format!("{cx} decision points ({} branches, {} loops)", m.branches, m.loops.len()));
        }

        let mut semantics: Vec<String> = Vec::new();
        let ret = f.ret.clone().unwrap_or_default();
        if ret.contains("Result") || ret.contains("error") || ret.contains("Error") {
            semantics.push("returns a fallible type - cover the error path, not just the happy one".into());
        }
        if ret.contains("Option") || ret.contains("null") || ret.contains("undefined") {
            semantics.push("can return nothing - assert the empty case".into());
        }
        if !m.resources.is_empty() {
            semantics.push(format!(
                "acquires {} resource(s) - assert they are released on both the success and failure path",
                m.resources.iter().filter(|r| r.acquire).count()
            ));
        }
        if !lang.is_typed() {
            semantics.push(format!(
                "{} has no compiler-checked signature, so a contract test is the only thing pinning the shape of its arguments",
                lang.as_str()
            ));
        } else if !f.param_types.is_empty() {
            semantics.push(format!(
                "typed signature ({}) - the compiler covers shape, so assert behaviour instead",
                f.param_types.join(", ")
            ));
        }
        if f.owner.is_some() {
            semantics.push("a method - construct its receiver in a fixture rather than calling it free".into());
        }

        let downstream: Vec<&str> = g.out[i].iter().take(4).map(|&c| g.name(c)).collect();
        let upstream: Vec<&str> = g.into[i].iter().take(4).map(|&c| g.name(c)).collect();
        let primary = *kinds.first().unwrap_or(&"smoke-test");
        let suggest = match primary {
            "load-test" => format!(
                "Drive `{}` at volume: {call_sites} call sites across {callers} caller(s), and it does real work per call.",
                f.name),
            "perf-test" => format!(
                "Benchmark `{}` across input sizes - {} nested loop level(s){}. Assert the growth curve, not a wall-clock number.",
                f.name, loop_depth,
                if m.recursive { ", and it recurses" } else { "" }),
            "contract-test" => format!(
                "Pin what `{}` promises: {} depend{} on it{}.",
                f.name,
                { let mut v = cross_callers.clone(); v.sort_unstable(); v.dedup(); v.join(", ") },
                // agree with the number of distinct services, not call sites
                if svcs.len() == 1 { "s" } else { "" },
                if upstream.is_empty() { String::new() } else { format!(" (via {})", upstream.join(", ")) }),
            "integration-test" => format!(
                "Exercise `{}` with its collaborators live{} - a {}-deep chain runs underneath it.",
                f.name,
                if downstream.is_empty() { String::new() } else { format!(" ({})", downstream.join(", ")) },
                d),
            _ => format!(
                "Call `{}` once with representative input and assert it completes - nothing covers it today.",
                f.name),
        };

        let mut score = cx * 3 + loop_depth * 8 + d * 4 + call_sites * 2 + call_outs * 3
            + cross_callers.len() * 10;
        if !covered {
            score += 40; // an untested function is the gap being looked for
        }
        if m.recursive {
            score += 6;
        }
        targets.push((
            score,
            pinned,
            json!({
                // stable handle: `test_triggers` cites these instead of
                // restating the recommendation
                "id": target_id(&g.file(i), &f.name),
                "function": f.name,
                "file": g.file(i),
                "line": f.line,
                "language": lang.as_str(),
                "service": own,
                "kind": primary,
                "also": kinds.iter().skip(1).collect::<Vec<_>>(),
                "priority": score,
                "covered": covered,
                "covered_by": covered_by,
                "suggest": suggest,
                "why": why,
                "semantics": semantics,
                "signals": {
                    "complexity": cx,
                    "call_depth": d,
                    "loop_depth": loop_depth,
                    "call_sites": call_sites,
                    "call_outs": call_outs,
                    "callers": callers,
                    "lines": m.body_lines,
                    "recursive": m.recursive,
                },
            }),
        ));
    }

    targets.sort_by(|a, b| b.0.cmp(&a.0));
    let total = targets.len();
    let uncovered = targets
        .iter()
        .filter(|(_, _, v)| v["covered"] == false)
        .count();
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for (_, _, v) in &targets {
        *by_kind
            .entry(v["kind"].as_str().unwrap_or("").to_string())
            .or_default() += 1;
    }
    let truncated = total > MAX_TARGETS;
    // rank order decides the cut, but a pinned row is kept wherever it landed
    let mut kept = 0usize;
    let rows: Vec<Value> = targets
        .into_iter()
        .filter(|(_, pinned, _)| {
            if *pinned {
                return true;
            }
            kept += 1;
            kept <= MAX_TARGETS
        })
        .map(|(_, _, v)| v)
        .collect();

    json!({
        "targets": rows,
        "truncated": truncated,
        "summary": {"functions": total, "untested": uncovered, "by_kind": by_kind},
        "rubric": test_kind_rubric(),
        "note": "Ranked by structural risk, then by whether any test mentions the function. \
                 `covered` means a test refers to it by name - not that its behaviour is \
                 asserted, and not that the recommended *kind* of test exists.",
    })
}

// One representation per fact: a recommendation is written once, in
// `test_targets`, and referred to everywhere else by this handle.
fn target_id(file: &str, function: &str) -> String {
    format!("{file}::{function}")
}

// The branch's change set, diffed against `base`, over the map already parsed.
//
// The working-tree view: an engineer's uncommitted edit has to count, and CI
// re-runs this against the committed tree anyway.
fn change_set(
    g: &Graph,
    root: &Path,
    root_label: &str,
    base: Option<&str>,
) -> Result<changes::ChangesReport, String> {
    let opts = changes::ChangesOptions {
        // `ccc insights --base` passes one, so a page built on the default
        // branch can diff against a tag instead of against itself
        base: base.map(str::to_string),
        service_flags: Vec::new(),
        worktree: true,
    };
    changes::changes_with_caches(root, root_label, &opts, g.caches).map_err(|e| format!("{e:#}"))
}

// How a runner selects a single test, per language. Emitted so a CI job can
// paste the command rather than re-deriving it from the test list.
fn run_command(language: &str, tests: &[&TestSite]) -> Option<(String, &'static str)> {
    if tests.is_empty() {
        return None;
    }
    let names: Vec<&str> = tests.iter().map(|t| t.name.as_str()).collect();
    Some(match language {
        // Bare substring filters, *not* `--exact`: exact matching wants the
        // full module path (`extract::tests::foo`), which the map does not
        // record - and a command that silently selects zero tests is the worst
        // possible failure for a trigger list.
        "rust" => (
            format!("cargo test -- {}", names.join(" ")),
            "substring filters, so a name that is a prefix of another test selects both -              erring toward running more. Needs Rust 1.68+ for multiple filters.",
        ),
        "go" => {
            let dirs: BTreeSet<&str> = tests
                .iter()
                .map(|t| t.file.rsplit_once('/').map(|(d, _)| d).unwrap_or("."))
                .collect();
            (
                format!(
                    "go test -run '^({})$' {}",
                    names.join("|"),
                    dirs.iter().map(|d| format!("./{d}")).collect::<Vec<_>>().join(" ")
                ),
                "anchored, so only these test functions run.",
            )
        }
        "python" => (
            format!("pytest -k \"{}\"", names.join(" or ")),
            "`-k` matches against test ids by substring.",
        ),
        "javascript" | "typescript" | "tsx" => {
            // BDD labels arrive as `it("charges a fee")`; jest selects on the
            // title, so the label is what goes through
            let labels: Vec<String> = names
                .iter()
                .map(|n| {
                    n.split_once('(')
                        .and_then(|(_, r)| r.strip_suffix(')'))
                        .map(|l| l.trim_matches('"').to_string())
                        .unwrap_or_else(|| (*n).to_string())
                })
                .collect();
            (
                format!("npx jest -t \"{}\"", labels.join("|")),
                "`-t` matches the test *title*. Exported test helpers that are not \
                 `it`/`test` blocks will not be selected by it.",
            )
        }
        "cpp" => (
            format!("ctest -R '{}'", names.join("|")),
            "ctest selects by *registered* test name, which need not match the C++ \
             function name - adjust if your suite registers them differently.",
        ),
        _ => return None,
    })
}

// The trigger walk, run before the recommendations so its gap list can pin the
// `test_targets` rows those gaps cite. Splitting it out is what keeps the two
// sections consistent: one walk decides both what runs and what is missing.
struct Triggered {
    // (test, call hops from the change, which impacted functions it names)
    run: Vec<(TestSite, usize, BTreeSet<String>)>,
    // changed functions no test reaches at all, in report order
    gaps: Vec<(String, String, Vec<usize>)>,
    // every distinct test in the map, for the "just run everything" call
    total_tests: usize,
    any_change: bool,
}

fn triggered(g: &Graph, report: &changes::ChangesReport) -> Triggered {
    // changed function names -> graph nodes
    let mut seeds: Vec<usize> = Vec::new();
    for i in 0..g.nodes.len() {
        let f = g.func(i);
        let file = g.file(i);
        if report
            .changed_functions
            .iter()
            .any(|c| c.function == f.name && c.file == file)
        {
            seeds.push(i);
        }
    }

    // walk upwards: everything that (transitively) calls a changed function
    let mut distance: BTreeMap<usize, usize> = BTreeMap::new();
    let mut queue: VecDeque<usize> = VecDeque::new();
    for &s in &seeds {
        distance.insert(s, 0);
        queue.push_back(s);
    }
    while let Some(v) = queue.pop_front() {
        let d = distance[&v];
        if d >= 8 {
            continue; // far enough that the link stops being meaningful
        }
        for &caller in &g.into[v] {
            if let std::collections::btree_map::Entry::Vacant(slot) = distance.entry(caller) {
                slot.insert(d + 1);
                queue.push_back(caller);
            }
        }
    }

    // a test is triggered when it references anything in the impacted set
    let sites = test_sites(g);
    let mut by_test: BTreeMap<(String, String), (TestSite, usize, BTreeSet<String>)> =
        BTreeMap::new();
    for (&node, &d) in &distance {
        if g.is_test(node) {
            continue;
        }
        let name = g.name(node).to_string();
        let Some(tests) = sites.get(&name) else { continue };
        for t in tests {
            let key = (t.file.clone(), t.name.clone());
            let entry = by_test
                .entry(key)
                .or_insert_with(|| (t.clone(), d, BTreeSet::new()));
            entry.1 = entry.1.min(d);
            if entry.2.len() < 8 {
                entry.2.insert(name.clone());
            }
        }
    }
    let mut run: Vec<(TestSite, usize, BTreeSet<String>)> = by_test.into_values().collect();
    run.sort_by(|a, b| (a.1, &a.0.file, &a.0.name).cmp(&(b.1, &b.0.file, &b.0.name)));

    // changed functions no test reaches: the gaps a pipeline should fail on
    let reachable: BTreeSet<&str> = run
        .iter()
        .flat_map(|(_, _, covers)| covers.iter().map(String::as_str))
        .collect();
    // A changed *test* is not a coverage gap - nothing covers a test, and
    // `test_targets` never ranks one, so listing it here would produce advice
    // that cannot be acted on.
    let is_test: BTreeSet<(String, String)> = (0..g.nodes.len())
        .filter(|&i| g.is_test(i))
        .map(|i| (g.file(i), g.func(i).name.clone()))
        .collect();
    let gaps: Vec<(String, String, Vec<usize>)> = report
        .changed_functions
        .iter()
        .filter(|c| !reachable.contains(c.function.as_str()) && c.tested_by.is_empty())
        .filter(|c| !is_test.contains(&(c.file.clone(), c.function.clone())))
        .map(|c| (c.file.clone(), c.function.clone(), c.lines.to_vec()))
        .collect();

    let total_tests = sites
        .values()
        .flatten()
        .map(|t| (t.file.as_str(), t.name.as_str()))
        .collect::<BTreeSet<_>>()
        .len();

    Triggered {
        run,
        gaps,
        total_tests,
        any_change: !report.changed_functions.is_empty(),
    }
}

// Which tests a change makes necessary.
//
// A change to a function does not only invalidate the tests that name it: any
// test exercising something *upstream* runs through it too. So the impacted set
// is the changed functions plus everything that transitively calls them, and a
// test is triggered if it references any member. `distance` records how many
// call hops away the test's entry point was, which is how the list is ranked -
// a test naming the changed function directly is the most likely to fail.
//
// The change set itself is *not* restated here - it is computed once and lives
// under the payload's `changes` key. This section carries only what is specific
// to triggering: which tests to run, how far each sits from the change, the
// runnable command, and which gaps a pipeline should fail on.
fn test_triggers(report: &changes::ChangesReport, trig: &Triggered, targets: &Value) -> Value {
    let Triggered {
        run,
        gaps,
        total_tests,
        any_change,
    } = trig;
    let empty = Vec::new();
    let known: BTreeSet<&str> = targets["targets"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|t| t["id"].as_str())
        .collect();
    // A gap is a *reference* into `test_targets`, not a second copy of the
    // recommendation. Those rows are pinned against truncation, so `resolved`
    // is only false for a function `test_targets` never ranks at all - a test
    // function, which is never the target of one.
    let add: Vec<Value> = gaps
        .iter()
        .map(|(file, function, lines)| {
            let id = target_id(file, function);
            json!({
                "target": id,
                "resolved": known.contains(id.as_str()),
                "lines": lines,
            })
        })
        .collect();

    // one runnable command per language present in the triggered set
    let mut by_lang: BTreeMap<&str, Vec<&TestSite>> = BTreeMap::new();
    for (t, _, _) in run {
        by_lang.entry(t.language).or_default().push(t);
    }
    let commands: Vec<Value> = by_lang
        .iter()
        .filter_map(|(lang, tests)| {
            run_command(lang, tests).map(|(command, caveat)| {
                json!({
                    "language": lang,
                    "command": command,
                    "selects": tests.len(),
                    "caveat": caveat,
                })
            })
        })
        .collect();

    let dirty: Vec<&str> = report
        .changed_files
        .iter()
        .filter(|f| f.uncommitted)
        .map(|f| f.path.as_str())
        .collect();

    // When the trigger set covers most of the suite, a long name filter is
    // slower and more fragile than just running everything - say so rather
    // than emitting a 60-name command.
    let total_tests = *total_tests;
    let full_suite = total_tests > 0 && run.len() * 5 >= total_tests * 4;

    json!({
        "available": true,
        "base": report.base,
        "base_sha": report.base_sha,
        "head_sha": report.head_sha,
        // uncommitted edits are included in the diff, and named here
        "uncommitted_files": dirty,
        // The change set is under the payload's `changes` key - `services_to_test`
        // is repeated because it is three words and the header renders it.
        "services_to_test": report.services_to_test,
        "change_set": "changes",
        "run": run.iter().map(|(t, d, covers)| json!({
            "test": t.name,
            "file": t.file,
            "line": t.line,
            "language": t.language,
            "distance": d,
            // why this test is in the list
            "covers": covers.iter().collect::<Vec<_>>(),
            "reason": if *d == 0 {
                format!("references {}, which changed", covers.iter().take(3).cloned().collect::<Vec<_>>().join(", "))
            } else {
                format!("reaches the change through {d} call hop(s) via {}",
                        covers.iter().take(3).cloned().collect::<Vec<_>>().join(", "))
            },
        })).collect::<Vec<_>>(),
        "add": add,
        "commands": commands,
        "total_tests": total_tests,
        // the selection has stopped being a saving
        "full_suite_advised": full_suite,
        "counts": {
            "changed_files": report.changed_files.len(),
            "changed_functions": report.changed_functions.len(),
            "uncommitted_files": dirty.len(),
            "tests_to_run": run.len(),
            "gaps": add.len(),
            "direct": run.iter().filter(|(_, d, _)| *d == 0).count(),
        },
        "note": "Tests are matched to changes by name through the call graph, so this is the \
                 set worth running - not proof that running it covers the change. A test that \
                 exercises code without naming it, or through dynamic dispatch, cannot be seen. \
                 `distance` is call hops from the changed function to what the test names. \
                 Each `add` entry is a `target` id into `test_targets`, where the \
                 recommendation itself lives.",
        "changed_note": if *any_change {
            "changed functions are diffed against the merge-base, including uncommitted edits"
        } else {
            "nothing changed against the base"
        },
    })
}

// Analyse `root` from scratch: walk, parse and build the payload
pub fn analyse(root: &Path, root_label: &str, base: Option<&str>) -> anyhow::Result<Value> {
    let files = crate::scan::collect_files(root)?;
    let caches = crate::scan::build_caches(root, &files);
    Ok(insights(&caches, root, root_label, &crate::render::now_ts(), base))
}

// Build the whole insights payload for a parsed map.
pub fn insights(
    caches: &[FileCache],
    root: &Path,
    root_label: &str,
    generated: &str,
    // diff base for the test triggers; `None` resolves to origin/main
    base: Option<&str>,
) -> Value {
    let started = Instant::now();
    let g = build_graph(caches);
    let ctx = service_ctx(&g, root);
    let n = g.nodes.len();

    // The change set is computed once, here, and every consumer refers to this
    // one copy: the `changes` section below, the trigger list, and the ranked
    // targets that back it. It used to be recomputed inside `test_triggers`
    // and restated in its output, which made the same facts arrive twice.
    let change_set = change_set(&g, root, root_label, base);
    // The trigger walk runs first: its gap list is exactly the set of target
    // rows that must survive truncation, since `add` cites them by id alone.
    let trig = change_set.as_ref().ok().map(|r| triggered(&g, r));
    let must_keep: BTreeSet<(String, String)> = trig
        .as_ref()
        .map(|t| {
            t.gaps
                .iter()
                .map(|(file, function, _)| (file.clone(), function.clone()))
                .collect()
        })
        .unwrap_or_default();
    let targets = test_targets(&g, &ctx, &must_keep);
    let (changes_section, triggers) = match (&change_set, &trig) {
        (Ok(r), Some(t)) => (
            serde_json::to_value(r).unwrap_or(Value::Null),
            test_triggers(r, t, &targets),
        ),
        // no git, no base ref, shallow clone: say why rather than render an
        // empty tab that reads as "nothing to run"
        _ => {
            let reason = change_set.as_ref().err().cloned().unwrap_or_default();
            let why = json!({
                "available": false,
                "reason": reason,
                "hint": "The change set diffs the branch against its base. In CI, fetch history \
                         (actions/checkout with fetch-depth: 0); locally, make sure the branch \
                         has an upstream such as origin/main.",
            });
            (why.clone(), why)
        }
    };

    // roots: nothing *else* calls them - a self-recursive function with no
    // other caller is still an entry point. Test functions are roots too, but
    // they drown out the application tree, so they are ranked last :)
    let mut roots: Vec<usize> = (0..n).filter(|&i| g.is_root(i)).collect();
    roots.sort_by_key(|&i| {
        (
            g.is_test(i),
            std::cmp::Reverse(g.out[i].len()),
            g.file(i),
            g.func(i).line,
        )
    });

    let mut budget = FLAME_NODES;
    let (tree, flame_truncated) = flame(&g, &ctx, &roots, &mut budget);
    let (lint_rows, lints_truncated) = lints(&g);

    // One flame graph per service that declares dependencies in map.json -
    // those are the services whose call trees are expected to leave their own
    // boundary. With no `deps` declared, every service gets one instead, so the
    // view is not empty just because nobody filled in the config.
    // With `deps` declared, those services are exactly the ones whose call
    // trees are expected to leave their own boundary. Without them every real
    // service gets one - but not when the grouping degenerated to one unit per
    // file, where it would just redraw the same tree once per module.
    let flame_keys: Vec<String> = if !ctx.deps.is_empty() {
        ctx.deps.keys().cloned().collect()
    } else if ctx.per_file {
        Vec::new()
    } else {
        ctx.map.keys().cloned().collect()
    };
    let mut groups = vec![json!({
        "service": "(whole map)",
        "declares": Value::Null,
        "roots": tree,
        "truncated": flame_truncated,
    })];
    for key in flame_keys.iter().take(MAX_FLAME_GROUPS) {
        // a service's entry points: called by nobody, or only from outside it.
        // Those are the frames a caller in another service actually lands on.
        let svc_roots: Vec<usize> = (0..n)
            .filter(|&i| ctx.of_node(&g, i) == Some(key.as_str()))
            .filter(|&i| {
                g.into[i]
                    .iter()
                    .all(|&c| c == i || ctx.of_node(&g, c) != Some(key.as_str()))
            })
            .collect();
        let mut b = FLAME_NODES;
        let (t, cut) = flame(&g, &ctx, &svc_roots, &mut b);
        groups.push(json!({
            "service": key,
            "declares": ctx.deps.get(key),
            "roots": t,
            "truncated": cut,
        }));
    }
    let groups_truncated = flame_keys.len() > MAX_FLAME_GROUPS;

    let mut by_callers: Vec<usize> = (0..n).collect();
    by_callers.sort_by_key(|&i| {
        (
            std::cmp::Reverse(g.into[i].len()),
            std::cmp::Reverse(g.call_sites[i]),
            g.file(i),
        )
    });
    let mut by_fanout: Vec<usize> = (0..n).collect();
    by_fanout.sort_by_key(|&i| (std::cmp::Reverse(g.out[i].len()), g.file(i)));
    let mut by_complexity: Vec<usize> = (0..n).collect();
    by_complexity.sort_by_key(|&i| (std::cmp::Reverse(g.func(i).metrics.complexity()), g.file(i)));

    let row = |i: usize| {
        let f = g.func(i);
        json!({
            "name": f.name,
            "file": g.file(i),
            "line": f.line,
            "callers": g.into[i].len(),
            "call_sites": g.call_sites[i],
            "calls": g.out[i].len(),
            "lines": f.metrics.body_lines,
            "complexity": f.metrics.complexity(),
            "loop_depth": f.metrics.max_loop_depth(),
            "recursive": f.metrics.recursive,
            "language": g.lang(i).as_str(),
            "test": g.is_test(i),
        })
    };
    let take = |v: &Vec<usize>, pred: &dyn Fn(usize) -> bool| {
        v.iter()
            .copied()
            .filter(|&i| pred(i))
            .take(TOP_N)
            .map(row)
            .collect::<Vec<_>>()
    };

    // per-language totals, so the UI can say which rules even apply
    let mut langs: BTreeMap<&str, (usize, usize, usize, usize)> = BTreeMap::new();
    for c in caches {
        let e = langs.entry(c.language.as_str()).or_default();
        e.0 += 1;
        e.1 += c.funcs.len();
        e.2 += c.funcs.iter().map(|f| f.metrics.complexity()).sum::<usize>();
        e.3 += c.lines;
    }
    let total_lines: usize = caches.iter().map(|c| c.lines).sum();

    let edge_count: usize = g.out.iter().map(|s| s.len()).sum();
    json!({
        "schema": SCHEMA,
        "root": root_label,
        // when the map this was computed from was parsed
        "generated": generated,
        // how long this analysis took, measured over the whole build above
        "took_ns": started.elapsed().as_nanos() as u64,
        "totals": {
            "files": caches.len(),
            // source lines across every mapped file
            "lines": total_lines,
            // definitions only - the module frames the graph adds for top-level
            // code are nodes, but nobody wrote them as functions
            "functions": caches.iter().map(|c| c.funcs.len()).sum::<usize>(),
            "edges": edge_count,
            "roots": roots.len(),
        },
        "languages": langs.iter().map(|(name, (files, funcs, cx, lines))| json!({
            "language": name,
            "files": files,
            "funcs": funcs,
            "lines": lines,
            "avg_complexity": if *funcs == 0 { 0.0 } else { *cx as f64 / *funcs as f64 },
        })).collect::<Vec<_>>(),
        "flame": {
            // one group per service declaring deps, plus the whole map
            "groups": groups,
            "groups_truncated": groups_truncated,
            "note": "static call tree - `value` counts reachable call sites, not time. \
                     Recursion is cut where it repeats. A highlighted frame is one the \
                     call reached by leaving its caller's service.",
        },
        "hot": {
            // module frames are callers, not functions, so they carry the
            // `most_called` counts without ever being ranked as a row
            "most_called": take(&by_callers, &|i| !g.into[i].is_empty() && !g.is_module(i)),
            "widest": take(&by_fanout, &|i| !g.out[i].is_empty() && !g.is_module(i)),
            "most_complex": take(&by_complexity, &|i| !g.is_test(i) && !g.is_module(i)),
            "deepest_chains": deepest_chains(&g, &roots),
            "cycles": cycles(&g),
            "note": "structural, not measured: ranks by call-graph shape, not execution frequency.",
        },
        "services": services(&g, &ctx),
        // the one canonical change set; `test_triggers` refers to it
        "changes": changes_section,
        "test_targets": targets,
        "test_triggers": triggers,
        "lints": {
            "findings": lint_rows,
            "truncated": lints_truncated,
            "rules": rule_catalogue(),
            "note": "syntax-level heuristics with no type or data-flow information. \
                     Every finding cites the measurement it came from - verify before acting.",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;

    // `tag` must be unique per test: these run in parallel in one process
    fn map(tag: &str, files: &[(&str, &str)]) -> (tempdir::Dir, Vec<FileCache>) {
        let dir = tempdir::Dir::new(tag);
        for (path, content) in files {
            let to = dir.path().join(path);
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::write(&to, content).unwrap();
        }
        let found = scan::collect_files(dir.path()).unwrap();
        let caches = scan::build_caches(dir.path(), &found);
        (dir, caches)
    }

    // a throwaway directory that cleans up on drop
    mod tempdir {
        pub struct Dir(std::path::PathBuf);
        impl Dir {
            pub fn new(tag: &str) -> Dir {
                let p = std::env::temp_dir().join(format!("ccc-{tag}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn call_graph_resolves_same_file_and_evidenced_cross_file() {
        let (dir, caches) = map("xfile", &[
            (
                "src/lib.rs",
                "pub fn helper(x: u64) -> u64 { x + 1 }\n\
                 pub fn charge(x: u64) -> u64 { helper(x) }\n",
            ),
            (
                "src/main.rs",
                "use crate::lib::charge;\n\
                 fn main() { let _ = charge(1); }\n",
            ),
        ]);
        let g = build_graph(&caches);
        let pos = |name: &str| (0..g.nodes.len()).find(|&i| g.name(i) == name).unwrap();
        // same-file edge from `refs`
        assert!(g.out[pos("charge")].contains(&pos("helper")));
        // cross-file edge, evidenced by the `use`
        assert!(g.out[pos("main")].contains(&pos("charge")));
        // and nothing calls main
        assert!(g.into[pos("main")].is_empty());
        drop(dir);
    }

    #[test]
    fn unevidenced_name_collisions_produce_no_edge() {
        // `run` is defined in two files and called with no qualifier or import
        let (dir, caches) = map("collide", &[
            ("src/a.rs", "pub fn run() -> u8 { 1 }\n"),
            ("src/b.rs", "pub fn run() -> u8 { 2 }\n"),
            ("src/c.rs", "fn go() -> u8 { run() }\n"),
        ]);
        let g = build_graph(&caches);
        let go = (0..g.nodes.len()).find(|&i| g.name(i) == "go").unwrap();
        assert!(
            g.out[go].is_empty(),
            "an ambiguous call must not invent an edge"
        );
        drop(dir);
    }

    #[test]
    fn module_scope_calls_make_a_python_entry_point_a_caller() {
        // The shape every Python CLI has. `__main__.py` does its work at the
        // top level, so the extractor attributes the call to module scope
        // rather than to a function - and with no node for that scope the call
        // used to be dropped, leaving `main` reading as uncalled.
        let (dir, caches) = map("pymain", &[
            (
                "mypkg/cli.py",
                "def parse(argv):\n    return argv\n\n\ndef main(argv=None):\n    return parse(argv)\n",
            ),
            (
                "mypkg/__main__.py",
                "from mypkg.cli import main\n\nraise SystemExit(main())\n",
            ),
        ]);
        let g = build_graph(&caches);
        let pos = |name: &str| (0..g.nodes.len()).find(|&i| g.name(i) == name).unwrap();
        let (top, main) = (pos(TOP_LEVEL), pos("main"));
        assert!(g.is_module(top));
        assert_eq!(g.file(top), "mypkg/__main__.py");
        assert!(g.out[top].contains(&main));
        assert_eq!(g.into[main], BTreeSet::from([top]));
        assert_eq!(g.call_sites[main], 1);
        // nothing runs a module, so it is the entry point a flame graph of a
        // Python package should be rooted at
        assert!(g.is_root(top));
        // but it is not a definition: nobody wrote it, so it is not a thing to
        // lint, rank or recommend a test for
        let (rows, _) = lints(&g);
        assert!(rows.iter().all(|r| r["function"] != TOP_LEVEL));
        drop(dir);
    }

    #[test]
    fn a_package_facade_import_reaches_the_module_that_defines_the_name() {
        let (dir, caches) = map("pyfacade", &[
            ("mypkg/cli.py", "def run():\n    return 1\n"),
            ("mypkg/__init__.py", "from .cli import run\n\n__all__ = [\"run\"]\n"),
            ("mypkg/app.py", "from mypkg import run\n\n\ndef go():\n    return run()\n"),
        ]);
        let g = build_graph(&caches);
        let pos = |name: &str| (0..g.nodes.len()).find(|&i| g.name(i) == name).unwrap();
        assert!(g.out[pos("go")].contains(&pos("run")));
        drop(dir);
    }

    #[test]
    fn a_package_root_defining_the_same_name_does_not_blur_a_direct_import() {
        // this is a very specific test for a problem I encountered this week :(
        let (dir, caches) = map("pyspecific", &[
            ("mypkg/cli.py", "def run():\n    return 1\n"),
            ("mypkg/__init__.py", "def run():\n    return 2\n"),
            (
                "mypkg/app.py",
                "from mypkg.cli import run\n\n\ndef go():\n    return run()\n",
            ),
        ]);
        let g = build_graph(&caches);
        let cli = caches.iter().position(|c| c.rel_path.ends_with("cli.py")).unwrap();
        let go = (0..g.nodes.len()).find(|&i| g.name(i) == "go").unwrap();
        let called: Vec<String> = g.out[go].iter().map(|&i| g.file(i)).collect();
        assert_eq!(called, vec![changes::path_str(&caches[cli].rel_path)]);
        drop(dir);
    }

    #[test]
    fn cross_file_calls_resolve_in_the_newly_added_languages() {
        // each pair is written so the call can only resolve through evidence
        struct Pair {
            tag: &'static str,
            files: &'static [(&'static str, &'static str)],
            caller: &'static str,
            callee: &'static str,
        }
        const PAIRS: &[Pair] = &[
            Pair {
                tag: "xcs",
                files: &[
                    (
                        "lib/Money.cs",
                        "namespace Lib { public class Money { public static int Charge(int c) { return c; } } }\n",
                    ),
                    (
                        "api/Api.cs",
                        "using Lib;\nnamespace Api { public class Handler { public int Handle() { return Money.Charge(100); } } }\n",
                    ),
                ],
                caller: "Handle",
                callee: "Charge",
            },
            Pair {
                tag: "xzig",
                files: &[
                    ("money.zig", "pub fn charge(cents: u32) u32 {\n    return cents;\n}\n"),
                    (
                        "api.zig",
                        "const money = @import(\"money.zig\");\n\npub fn handle() u32 {\n    return money.charge(100);\n}\n",
                    ),
                ],
                caller: "handle",
                callee: "charge",
            },
            Pair {
                tag: "xodin",
                files: &[
                    ("money/money.odin", "package money\n\ncharge :: proc(cents: int) -> int {\n    return cents\n}\n"),
                    (
                        "api/api.odin",
                        "package api\n\nimport \"money\"\n\nhandle :: proc() -> int {\n    return money.charge(100)\n}\n",
                    ),
                ],
                caller: "handle",
                callee: "charge",
            },
        ];

        for pair in PAIRS {
            let (dir, caches) = map(pair.tag, pair.files);
            let g = build_graph(&caches);
            let pos = |name: &str| {
                (0..g.nodes.len())
                    .find(|&i| g.name(i) == name)
                    .unwrap_or_else(|| panic!("{}: no `{name}`", pair.tag))
            };
            let (from, to) = (pos(pair.caller), pos(pair.callee));
            assert!(
                g.out[from].contains(&to),
                "{}: `{}` -> `{}` did not resolve across files",
                pair.tag, pair.caller, pair.callee
            );
            assert_eq!(g.into[to].len(), 1, "{}: caller count", pair.tag);
            drop(dir);
        }
    }

    #[test]
    fn a_call_is_credited_to_the_definition_whose_body_spans_it() {
        // interface method and class mehtods share names so ended up resolving 
        // which was hit first... wrong
        let (dir, caches) = map("overload", &[(
            "src/pay.cs",
            "public interface ICharger { int Pay(int a); }\n\
             public class Payer : ICharger {\n\
             \x20   public int Pay(int a) { return Settle(a); }\n\
             \x20   private int Settle(int a) { return a; }\n\
             }\n",
        )]);
        let g = build_graph(&caches);
        let at = |line: usize| {
            (0..g.nodes.len())
                .find(|&i| g.func(i).line == line)
                .unwrap_or_else(|| panic!("nothing defined on line {line}"))
        };
        let settle = (0..g.nodes.len()).find(|&i| g.name(i) == "Settle").unwrap();
        assert!(g.out[at(3)].contains(&settle), "the class body makes the call");
        assert!(g.out[at(1)].is_empty(), "the interface declaration makes none");
        assert_eq!(g.into[settle].len(), 1);
        drop(dir);
    }

    #[test]
    fn an_include_makes_the_included_file_s_definitions_resolvable() {
        let cases: &[(&str, &[(&str, &str)])] = &[
            (
                "incc",
                &[
                    ("money.c", "int charge(int cents) { return cents; }\n"),
                    ("api.c", "#include \"money.h\"\nint handle(void) { return charge(100); }\n"),
                ],
            ),
            (
                "inccpp",
                &[
                    ("money.cpp", "int charge(int cents) { return cents; }\n"),
                    ("api.cpp", "#include \"money.h\"\nint handle() { return charge(100); }\n"),
                ],
            ),
        ];
        for (tag, files) in cases {
            let (dir, caches) = map(tag, files);
            let g = build_graph(&caches);
            let pos = |name: &str| {
                (0..g.nodes.len())
                    .find(|&i| g.name(i) == name)
                    .unwrap_or_else(|| panic!("{tag}: no `{name}`"))
            };
            assert!(
                g.out[pos("handle")].contains(&pos("charge")),
                "{tag}: the call did not resolve through the include"
            );
            drop(dir);
        }
    }

    #[test]
    fn flame_values_nest_and_recursion_is_cut() {
        let (dir, caches) = map("flame", &[(
            "src/lib.rs",
            "fn leaf() -> u8 { 1 }\n\
             fn mid() -> u8 { leaf() }\n\
             fn top() -> u8 { mid() }\n\
             fn spin(n: u8) -> u8 { spin(n) }\n",
        )]);
        let g = build_graph(&caches);
        let mut budget = FLAME_NODES;
        let roots: Vec<usize> = (0..g.nodes.len()).filter(|&i| g.is_root(i)).collect();
        let ctx = service_ctx(&g, dir.path());
        let (tree, _) = flame(&g, &ctx, &roots, &mut budget);
        let top = tree.iter().find(|n| n["name"] == "top").unwrap();
        // top -> mid -> leaf, so the root covers three frames
        assert_eq!(top["value"], 3);
        assert_eq!(top["children"][0]["name"], "mid");
        assert_eq!(top["children"][0]["children"][0]["name"], "leaf");
        // self-recursion terminates rather than expanding forever
        let spin = tree.iter().find(|n| n["name"] == "spin").unwrap();
        assert_eq!(spin["value"], 2);
        assert_eq!(spin["children"][0]["recursive"], true);
        drop(dir);
    }

    #[test]
    fn lints_fire_with_evidence_and_skip_tests() {
        let (dir, caches) = map("lints", &[
            (
                "src/hot.cpp",
                // leaks (malloc, no free) and nests three loops deep
                "void work(int n) {\n\
                 \x20 char* p = (char*)malloc(n);\n\
                 \x20 for (int i = 0; i < n; i++) {\n\
                 \x20   for (int j = 0; j < n; j++) {\n\
                 \x20     for (int k = 0; k < 4; k++) { use(p, i, j, k); }\n\
                 \x20   }\n\
                 \x20 }\n\
                 }\n",
            ),
            (
                "src/hot_test.cpp",
                "void t() { char* q = (char*)malloc(4); }\n",
            ),
        ]);
        let g = build_graph(&caches);
        let (found, _) = lints(&g);
        let rules: Vec<&str> = found.iter().map(|f| f["rule"].as_str().unwrap()).collect();
        assert!(rules.contains(&"leak-risk"), "{rules:?}");
        assert!(rules.contains(&"deep-loop-nest"), "{rules:?}");
        // the innermost `k < 4` loop has a literal bound the others lack
        assert!(rules.contains(&"unroll-candidate"), "{rules:?}");
        // findings cite a real location
        let leak = found.iter().find(|f| f["rule"] == "leak-risk").unwrap();
        assert_eq!(leak["file"], "src/hot.cpp");
        assert_eq!(leak["function"], "work");
        assert!(leak["message"].as_str().unwrap().contains("malloc"));
        // the test file is skipped entirely
        assert!(
            !found.iter().any(|f| f["file"] == "src/hot_test.cpp"),
            "test files must not raise lints"
        );
        drop(dir);
    }

    #[test]
    fn a_release_discharges_every_acquire_that_calls_for_it() {
        let (dir, caches) = map("leakpairs", &[
            (
                "svc/main.go",
                "func openFile(p string) error {\n\
                 \x20 f, err := os.OpenFile(p, os.O_RDWR, 0644)\n\
                 \x20 if err != nil { return err }\n\
                 \x20 defer f.Close()\n\
                 \x20 return nil\n\
                 }\n\
                 func dial(a string) error {\n\
                 \x20 c, err := net.Dial(\"tcp\", a)\n\
                 \x20 if err != nil { return err }\n\
                 \x20 defer c.Close()\n\
                 \x20 return nil\n\
                 }\n\
                 func timer() {\n\
                 \x20 t := time.NewTimer(time.Second)\n\
                 \x20 defer t.Stop()\n\
                 }\n\
                 func aliased(p string) error {\n\
                 \x20 f, err := os.OpenFile(p, os.O_RDWR, 0644)\n\
                 \x20 if err != nil { return err }\n\
                 \x20 r := f\n\
                 \x20 defer r.Close()\n\
                 \x20 return nil\n\
                 }\n\
                 func leaks(p string) error {\n\
                 \x20 f, err := os.Open(p)\n\
                 \x20 _ = f\n\
                 \x20 return err\n\
                 }\n",
            ),
            (
                "svc/alloc.cpp",
                "void ok(int n) {\n\
                 \x20 char* p = (char*)calloc(n, 1);\n\
                 \x20 free(p);\n\
                 }\n",
            ),
            (
                "svc/db.py",
                "def ok(dsn):\n\
                 \x20   c = connect(dsn)\n\
                 \x20   c.close()\n\
                 \x20   return 1\n",
            ),
        ]);
        let g = build_graph(&caches);
        let (found, _) = lints(&g);
        let leaks: Vec<&str> = found
            .iter()
            .filter(|f| f["rule"] == "leak-risk")
            .map(|f| f["function"].as_str().unwrap())
            .collect();
        // the paired acquires are clean, whichever pair names their release,
        // and whether the handle is deferred directly or through an alias
        assert_eq!(leaks, vec!["leaks"], "only the unpaired acquire may fire");
        // and the one real finding still names the acquire that needs closing
        let leak = found.iter().find(|f| f["rule"] == "leak-risk").unwrap();
        assert!(leak["message"].as_str().unwrap().contains("unreleased `Open`"));
        assert!(leak["hint"].as_str().unwrap().contains("`Close`"));
        drop(dir);
    }

    // `.ccc/map.json` is the source of truth for the service tab: its globs
    // group the files, and its `deps` are edges of the same graph
    #[test]
    fn map_json_drives_services_deps_and_orphans() {
        let (dir, caches) = map(
            "changescfg",
            &[
                (
                    ".ccc/map.json",
                    r#"{"services":{"auth":["auth/**"],"billing":["billing/**"],"gateway":["gateway/**"]},
                        "deps":{"gateway":["auth"]}}"#,
                ),
                ("auth/lib.rs", "pub fn verify(t: &str) -> bool { !t.is_empty() }\n"),
                ("billing/charge.rs", "pub fn charge(c: u64) -> u64 { c }\n"),
                (
                    "gateway/main.rs",
                    "use crate::charge::charge;\nfn handle() -> u64 { charge(1) }\n",
                ),
                // matches no glob - must not disappear silently ("vendor" and
                // friends would not do: `scan` skips those directories outright)
                ("tools/codegen.rs", "pub fn helper() -> u64 { 7 }\n"),
            ],
        );
        let g = build_graph(&caches);
        let s = services(&g, &service_ctx(&g, dir.path()));

        assert_eq!(s["source"], ".ccc/map.json");
        let names: Vec<&str> = s["services"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["auth", "billing", "gateway"]);

        let edge = |from: &str, to: &str| {
            s["edges"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["from"] == from && e["to"] == to)
                .unwrap_or_else(|| panic!("missing {from} -> {to} edge"))
                .clone()
        };
        // detected from the call graph
        let detected = edge("gateway", "billing");
        assert_eq!(detected["declared"], false);
        assert_eq!(detected["symbols"][0], "charge");
        // declared in map.json: an edge of the same graph, flagged as declared
        let declared = edge("gateway", "auth");
        assert_eq!(declared["declared"], true);
        assert!(declared["symbols"].as_array().unwrap().is_empty());

        // a file matching no glob is reported, not dropped
        assert_eq!(
            s["unassigned_files"].as_array().unwrap(),
            &vec![json!("tools/codegen.rs")]
        );
        drop(dir);
    }

    // one flame graph per service that declares deps, with the frames a call
    // reached by leaving its caller's service marked
    #[test]
    fn flame_groups_follow_declared_deps_and_mark_crossings() {
        let (dir, caches) = map(
            "flamedeps",
            &[
                (
                    ".ccc/map.json",
                    r#"{"services":{"gateway":["gateway/**"],"billing":["billing/**"],"store":["store/**"]},
                        "deps":{"gateway":["billing"]}}"#,
                ),
                ("store/db.rs", "pub fn fetch(id: u64) -> u64 { id }
"),
                (
                    "billing/charge.rs",
                    "pub fn charge(id: u64) -> u64 { store::fetch(id) }
",
                ),
                (
                    "gateway/main.rs",
                    "fn handle(id: u64) -> u64 { billing::charge(id) }
",
                ),
            ],
        );
        let v = insights(&caches, dir.path(), "demo", "ts", None);

        // the analysis reports its own cost
        assert!(v["took_ns"].as_u64().unwrap() > 0, "took_ns must be measured");

        let groups = v["flame"]["groups"].as_array().unwrap();
        let names: Vec<&str> = groups.iter().map(|g| g["service"].as_str().unwrap()).collect();
        // the whole map, then one per `deps` key - not per service
        assert_eq!(names, vec!["(whole map)", "gateway"]);
        assert_eq!(groups[1]["declares"][0], "billing");

        // gateway's tree crosses into billing and on into store
        fn walk(n: &Value, out: &mut Vec<(String, bool, String)>) {
            out.push((
                n["name"].as_str().unwrap_or("").to_string(),
                n["crosses"].as_bool().unwrap_or(false),
                n["service"].as_str().unwrap_or("").to_string(),
            ));
            for c in n["children"].as_array().into_iter().flatten() {
                walk(c, out);
            }
        }
        let mut frames = Vec::new();
        for r in groups[1]["roots"].as_array().unwrap() {
            walk(r, &mut frames);
        }
        assert_eq!(
            frames,
            vec![
                ("handle".into(), false, "gateway".into()),
                ("charge".into(), true, "billing".into()),
                ("fetch".into(), true, "store".into()),
            ],
            "each hop out of a service must be marked"
        );
    }

    // the explore view needs the calls that carry each hop, not just its name
    #[test]
    fn service_edges_carry_their_call_sites() {
        let (dir, caches) = map(
            "explore",
            &[
                (
                    ".ccc/map.json",
                    r#"{"services":{"gateway":["gateway/**"],"billing":["billing/**"]}}"#,
                ),
                (
                    "billing/charge.rs",
                    "pub fn charge(id: u64) -> u64 { id }
pub fn helper() -> u64 { 1 }
",
                ),
                (
                    "gateway/main.rs",
                    "fn handle(id: u64) -> u64 { billing::charge(id) }
",
                ),
            ],
        );
        let g = build_graph(&caches);
        let s = services(&g, &service_ctx(&g, dir.path()));
        let edge = s["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["from"] == "gateway" && e["to"] == "billing")
            .expect("gateway -> billing");
        let site = &edge["sites"][0];
        assert_eq!(site["symbol"], "charge");
        assert_eq!(site["caller"], "handle");
        assert_eq!(site["target_file"], "billing/charge.rs");
        assert_eq!(site["caller_file"], "gateway/main.rs");
        // an untouched function in the target is not listed as invoked
        assert!(!edge["symbols"].as_array().unwrap().iter().any(|x| x == "helper"));
        drop(dir);
    }

    // The recommendation has to follow the measurements, not the other way
    // round: a nested-loop function is a perf target, a cross-service boundary
    // is a contract target, an orchestrator is an integration target.
    #[test]
    fn test_targets_pick_the_kind_the_signals_justify() {
        let (dir, caches) = map(
            "targets",
            &[
                (
                    ".ccc/map.json",
                    r#"{"services":{"gateway":["gateway/**"],"billing":["billing/**"]}}"#,
                ),
                (
                    "billing/charge.rs",
                    // nested loops: a cost question, not a correctness one
                    "pub fn tally(rows: &[u64]) -> u64 {\n\
                     \x20   let mut t = 0;\n\
                     \x20   for a in rows { for b in rows { for c in rows { t += a + b + c; } } }\n\
                     \x20   t\n\
                     }\n\
                     pub fn charge(id: u64) -> u64 { id }\n",
                ),
                (
                    "gateway/main.rs",
                    "fn handle(id: u64) -> u64 { billing::charge(id) }\n",
                ),
            ],
        );
        let v = insights(&caches, dir.path(), "demo", "ts", None);
        let t = &v["test_targets"];
        let find = |name: &str| {
            t["targets"]
                .as_array()
                .unwrap()
                .iter()
                .find(|x| x["function"] == name)
                .unwrap_or_else(|| panic!("no target for {name}"))
                .clone()
        };

        // three nested loops -> benchmark it
        let tally = find("tally");
        assert_eq!(tally["kind"], "perf-test");
        assert_eq!(tally["signals"]["loop_depth"], 3);
        assert!(tally["suggest"].as_str().unwrap().contains("nested loop"));

        // called from another service -> pin the contract
        let charge = find("charge");
        assert_eq!(charge["kind"], "contract-test");
        assert!(
            charge["suggest"].as_str().unwrap().contains("gateway depends on it"),
            "{}", charge["suggest"]
        );

        // every recommendation cites the numbers behind it
        for x in t["targets"].as_array().unwrap() {
            assert!(!x["signals"]["complexity"].is_null());
            assert!(!x["suggest"].as_str().unwrap().is_empty());
            assert!(["smoke-test", "integration-test", "contract-test",
                     "perf-test", "load-test"]
                .contains(&x["kind"].as_str().unwrap()));
        }
        // and the rubric that produced them ships alongside
        assert_eq!(t["rubric"].as_array().unwrap().len(), 5);
        assert_eq!(t["summary"]["untested"], t["summary"]["functions"]);
        drop(dir);
    }

    // a language's semantics change the advice, not just the kind
    #[test]
    fn language_semantics_sharpen_the_suggestion() {
        let (dir, caches) = map(
            "semantics",
            &[
                // non-trivial on purpose: a one-line uncalled leaf is filtered
                // out as noise before it ever reaches a recommendation
                (
                    "src/io.rs",
                    "pub fn load(p: &str) -> Result<u64, String> {\n\
                     \x20   if p.is_empty() { return Err(\"empty\".into()); }\n\
                     \x20   let mut n = 0;\n\
                     \x20   for c in p.chars() { n += c as u64; }\n\
                     \x20   Ok(n)\n\
                     }\n",
                ),
                (
                    "app/loose.py",
                    "def blend(a, b):\n\
                     \x20   if a > b:\n\
                     \x20       return a - b\n\
                     \x20   total = 0\n\
                     \x20   for x in range(b):\n\
                     \x20       total += x\n\
                     \x20   return total\n",
                ),
            ],
        );
        let v = insights(&caches, dir.path(), "demo", "ts", None);
        let of = |name: &str| {
            v["test_targets"]["targets"]
                .as_array()
                .unwrap()
                .iter()
                .find(|x| x["function"] == name)
                .unwrap()
                .clone()
        };
        let joined = |x: &Value| {
            x["semantics"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap().to_string())
                .collect::<Vec<_>>()
                .join(" | ")
        };
        assert!(joined(&of("load")).contains("error path"), "{}", joined(&of("load")));
        // an untyped language has no compiler checking the shape
        assert!(joined(&of("blend")).contains("no compiler-checked signature"));
        drop(dir);
    }

    // per-service flame graphs are only worth drawing when "service" means
    // something; the one-unit-per-file fallback would redraw the same tree
    #[test]
    fn per_file_grouping_does_not_fan_out_flame_graphs() {
        let (dir, caches) = map(
            "perfile",
            &[
                ("a.rs", "pub fn one() -> u64 { 1 }\n"),
                ("b.rs", "pub fn two() -> u64 { 2 }\n"),
            ],
        );
        let v = insights(&caches, dir.path(), "demo", "ts", None);
        assert!(v["services"]["source"].as_str().unwrap().starts_with("one unit per file"));
        let names: Vec<&str> = v["flame"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["service"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["(whole map)"], "no per-file flame explosion");
        drop(dir);
    }

    // Declaring a dependency in map.json must never stand in for analysing it
    #[test]
    fn declared_deps_are_still_resolved_not_skipped() {
        let (dir, caches) = map(
            "declared",
            &[
                (
                    ".ccc/map.json",
                    r#"{"services":{"gateway":["gateway/**"],"auth":["auth/**"],"queue":["queue/**"]},
                        "deps":{"gateway":["auth","queue"]}}"#,
                ),
                ("auth/lib.rs", "pub fn verify(t: &str) -> bool { !t.is_empty() }\n"),
                // traversed, declared, and genuinely uncallable
                ("queue/worker.rs", "pub fn consume(job: u64) -> u64 { job }\n"),
                (
                    "gateway/main.rs",
                    "fn handle(t: &str) -> bool { auth::verify(t) }\n",
                ),
            ],
        );
        let g = build_graph(&caches);
        let s = services(&g, &service_ctx(&g, dir.path()));
        let edge = |to: &str| {
            s["edges"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["from"] == "gateway" && e["to"] == to)
                .unwrap_or_else(|| panic!("no gateway -> {to} edge"))
                .clone()
        };

        // declared AND resolved: both facts true, and the call sites are there
        // for the explore view to drill into
        let a = edge("auth");
        assert_eq!(a["declared"], true);
        assert_eq!(a["detected"], true, "a declared dep must still be analysed");
        assert_eq!(a["symbols"][0], "verify");
        assert_eq!(a["sites"][0]["symbol"], "verify");
        assert_eq!(a["sites"][0]["caller"], "handle");

        // declared with nothing to find: the edge still exists, but says so
        let q = edge("queue");
        assert_eq!(q["declared"], true);
        assert_eq!(q["detected"], false);
        assert!(q["sites"].as_array().unwrap().is_empty());
        drop(dir);
    }

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    // The operational question: given what changed on this branch - including
    // what is not committed yet - which tests must run, and where is nothing
    // covering the change at all?
    #[test]
    fn test_triggers_follow_the_diff_through_the_call_graph() {
        let (dir, _) = map(
            "triggers",
            &[
                ("src/money.rs", "pub fn fee() -> u64 { 30 }\npub fn charge(c: u64) -> u64 { c + fee() }\n"),
                ("src/api.rs", "use crate::money::charge;\npub fn handle() -> u64 { charge(1) }\n"),
                // covers `handle`, which sits one hop above `charge`
                ("tests/api_test.rs", "#[test]\nfn handles_a_request() { assert!(api::handle() > 0); }\n"),
                // covers `charge` directly
                ("tests/money_test.rs", "#[test]\nfn charges_with_fee() { assert_eq!(money::charge(1), 31); }\n"),
            ],
        );
        let d = dir.path();
        run_git(d, &["init", "-q", "-b", "main"]);
        run_git(d, &["add", "-A"]);
        run_git(d, &["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false",
                     "commit", "-qm", "base"]);
        // a branch off main, so origin/main is absent but `main` resolves
        run_git(d, &["checkout", "-q", "-b", "work"]);

        // an UNCOMMITTED edit to `fee`, the deepest function
        std::fs::write(
            d.join("src/money.rs"),
            "pub fn fee() -> u64 { 45 }\npub fn charge(c: u64) -> u64 { c + fee() }\n",
        )
        .unwrap();

        let files = scan::collect_files(d).unwrap();
        let caches = scan::build_caches(d, &files);
        let v = insights(&caches, d, "demo", "ts", None);
        let t = &v["test_triggers"];

        assert_eq!(t["available"], true, "{}", t["reason"]);
        // the uncommitted edit is in the diff, and called out as uncommitted
        assert_eq!(t["uncommitted_files"], json!(["src/money.rs"]));
        // the change set lives in exactly one place
        let changed: Vec<&str> = v["changes"]["changed_functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["function"].as_str().unwrap())
            .collect();
        assert!(changed.contains(&"fee"), "{changed:?}");
        assert!(
            t["changed_functions"].is_null() && t["changed_files"].is_null(),
            "test_triggers must cite the change set, not restate it"
        );

        // both tests trigger: one names `fee`'s caller chain directly, the
        // other reaches it from further up
        let run: BTreeMap<&str, u64> = t["run"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["test"].as_str().unwrap(), r["distance"].as_u64().unwrap()))
            .collect();
        assert!(run.contains_key("charges_with_fee"), "{run:?}");
        assert!(run.contains_key("handles_a_request"), "{run:?}");
        // `handle` is further from `fee` than `charge` is
        assert!(
            run["handles_a_request"] > run["charges_with_fee"],
            "distance must grow with call hops: {run:?}"
        );

        // and a runnable command that names them
        let cmd = t["commands"][0]["command"].as_str().unwrap();
        assert!(cmd.starts_with("cargo test -- "), "{cmd}");
        assert!(cmd.contains("charges_with_fee"), "{cmd}");
        // never `--exact`: it needs full module paths and would select nothing
        assert!(!cmd.contains("--exact"), "{cmd}");
        drop(dir);
    }

    // Every gap cites a `test_targets` row instead of carrying its own copy of
    // the recommendation, so the citation has to resolve - including for the
    // trivial functions `test_targets` would otherwise drop as noise.
    #[test]
    fn gaps_cite_a_target_row_that_survives_truncation() {
        let (dir, _) = map(
            "gapcite",
            &[
                ("src/lib.rs", "pub fn seed() -> u64 { 1 }\n"),
                ("tests/x_test.rs", "#[test]\nfn nothing_useful() { assert!(true); }\n"),
            ],
        );
        let d = dir.path();
        run_git(d, &["init", "-q", "-b", "main"]);
        run_git(d, &["add", "-A"]);
        run_git(d, &["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false",
                     "commit", "-qm", "base"]);
        run_git(d, &["checkout", "-q", "-b", "work"]);
        // a one-line leaf nobody calls: exactly the shape `test_targets` skips
        std::fs::write(d.join("src/lib.rs"), "pub fn seed() -> u64 { 2 }\n").unwrap();
        // and an edited test, which is a change but never a coverage gap
        std::fs::write(
            d.join("tests/x_test.rs"),
            "#[test]\nfn nothing_useful() { assert!(1 == 1); }\n",
        )
        .unwrap();

        let files = scan::collect_files(d).unwrap();
        let caches = scan::build_caches(d, &files);
        let v = insights(&caches, d, "demo", "ts", None);

        let ids: BTreeSet<&str> = v["test_targets"]["targets"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["id"].as_str())
            .collect();
        let add = v["test_triggers"]["add"].as_array().unwrap();
        assert!(!add.is_empty(), "the changed function has no test: {add:?}");
        for a in add {
            let id = a["target"].as_str().unwrap();
            assert_eq!(a["resolved"], true, "{id} is cited but unresolved");
            assert!(ids.contains(id), "{id} missing from test_targets: {ids:?}");
            // a citation, not a second copy
            assert!(a["suggest"].is_null() && a["why"].is_null(), "{a}");
            // nothing covers a test, so recommending one is unactionable advice
            assert!(!id.contains("nothing_useful"), "a changed test is not a gap");
        }
        drop(dir);
    }

    // Outside a git repo the tab must explain itself rather than look empty.
    #[test]
    fn test_triggers_say_why_when_git_cannot_answer() {
        let (dir, caches) = map("nogit", &[("src/a.rs", "pub fn one() -> u64 { 1 }\n")]);
        let v = insights(&caches, dir.path(), "demo", "ts", None);
        // both the change set and the triggers that depend on it explain why
        for key in ["changes", "test_triggers"] {
            let t = &v[key];
            assert_eq!(t["available"], false, "{key}");
            assert!(!t["reason"].as_str().unwrap().is_empty(), "{key}");
            assert!(t["hint"].as_str().unwrap().contains("fetch-depth"), "{key}");
        }
        drop(dir);
    }

    #[test]
    fn payload_is_shaped_and_services_fall_back_to_directories() {
        let (dir, caches) = map("payload", &[
            ("api/main.go", "package main\nfunc handle() int { return 1 }\n"),
            ("lib/money.go", "package money\nfunc Charge() int { return 2 }\n"),
        ]);
        let v = insights(&caches, dir.path(), "demo", "ts", None);
        assert_eq!(v["schema"], SCHEMA);
        assert_eq!(v["totals"]["files"], 2);
        assert_eq!(v["services"]["source"], "top-level directories (no .ccc/map.json)");
        let names: Vec<&str> = v["services"]["services"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["api", "lib"]);
        // every tab the UI renders must be present
        for key in ["flame", "hot", "services", "lints", "languages", "test_targets"] {
            assert!(!v[key].is_null(), "missing {key}");
        }
        assert!(!v["flame"]["groups"].as_array().unwrap().is_empty());
        assert!(!v["lints"]["rules"].as_array().unwrap().is_empty());
        drop(dir);
    }
}
