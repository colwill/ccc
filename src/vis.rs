//! The architecture visualiser behind `ccc run --vis`.
//!
//! Like the rest of ccc this reads syntax, not behaviour: an edge is a call the
//! resolver found evidence for, and a branch is a node in the tree, not a path
//! anything has been seen to take.

use crate::changes;
use crate::contracts::ContractIndex;
use crate::externals::norm_key;
use crate::extract::loop_label;
use crate::insights;
use crate::languages::Language;
use crate::model::{Boundary, FileCache};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tree_sitter::{Node, Parser};

pub const SCHEMA: &str = "ccc-vis/v1";

// files no service glob claims still need a box to sit in
const UNASSIGNED: &str = "(unassigned)";
// symbols named on one edge, and call sites kept for its detail panel
const EDGE_SYMBOLS: usize = 8;
const EDGE_SITES: usize = 12;
// stand-ins for functions outside a component, so a hub file called from
// everywhere cannot drown its own code view
const MAX_PROXIES: usize = 120;
// callers listed on a flow's entry node
const MAX_CALLERS: usize = 16;
// a generated thousand-line function must not become an unbounded graph
const MAX_STEPS: usize = 600;
// flow constructs nested deeper than this are cut, and so is a syntax tree
// deeper than `MAX_TREE_DEPTH`, which only generated code reaches
const MAX_NESTING: usize = 32;
const MAX_TREE_DEPTH: usize = 400;
// characters kept from text read off the source
const LABEL_CHARS: usize = 56;
const ARG_CHARS: usize = 28;
const MAX_ARGS: usize = 6;

// one function in the call graph, copied out so the model owns its data
struct FnNode {
    file: String,
    name: String,
    owner: Option<String>,
    line: usize,
    start: usize,
    end: usize,
    ret: Option<String>,
    param_types: Vec<String>,
    params: usize,
    complexity: usize,
    score: u8,
    branches: usize,
    loops: usize,
    recursive: bool,
    test: bool,
    // a file's top-level code rather than a function someone wrote
    module: bool,
    // nothing else calls it
    entry: bool,
    doc: Option<String>,
    language: &'static str,
}

impl FnNode {
    fn json(&self, id: usize) -> Value {
        json!({
            "id": id,
            "name": self.name,
            "owner": self.owner,
            "file": self.file,
            "line": self.line,
            "start": self.start,
            "end": self.end,
            "ret": self.ret,
            "params": self.params,
            "param_types": self.param_types,
            "complexity": self.complexity,
            "score": self.score,
            "branches": self.branches,
            "loops": self.loops,
            "recursive": self.recursive,
            "test": self.test,
            "module": self.module,
            "entry": self.entry,
            "doc": self.doc,
            "language": self.language,
        })
    }

    // enough to draw a stand-in and follow it
    fn brief(&self, id: usize) -> Value {
        json!({
            "id": id,
            "name": self.name,
            "owner": self.owner,
            "file": self.file,
            "line": self.line,
            "module": self.module,
        })
    }
}

// The visualiser's view of one map generation, built once and sliced per
// request: the overview whole, the code and flow levels on demand.
pub struct Model {
    overview: Value,
    funcs: Vec<FnNode>,
    // per function, what it calls and the line of the first call there, in
    // source order - the order a node's outputs are drawn in
    out: Vec<Vec<(usize, usize)>>,
    into: Vec<Vec<usize>>,
    // every mapped file, with the functions defined in it
    by_file: BTreeMap<String, Vec<usize>>,
    container_of: BTreeMap<String, String>,
}

impl Model {
    pub fn build(
        caches: &[FileCache],
        root: &Path,
        root_label: &str,
        generated: &str,
        contracts: &ContractIndex,
    ) -> Model {
        let g = insights::build_graph(caches, contracts);
        let ctx = insights::service_ctx(&g, root, contracts);
        let n = g.nodes.len();

        // where each call sits, so a function's outputs read in source order
        let mut sites: BTreeMap<(usize, &str, &str), Vec<usize>> = BTreeMap::new();
        for (fi, c) in caches.iter().enumerate() {
            for call in &c.calls {
                sites
                    .entry((fi, call.caller.as_str(), call.name.as_str()))
                    .or_default()
                    .push(call.line);
            }
        }
        let funcs: Vec<FnNode> = (0..n)
            .map(|i| {
                let f = g.func(i);
                FnNode {
                    file: g.file(i),
                    name: f.name.clone(),
                    owner: f.owner.clone(),
                    line: f.line,
                    start: f.start_line,
                    end: f.end_line,
                    ret: f.ret.clone(),
                    param_types: f.param_types.clone(),
                    params: f.metrics.params,
                    complexity: f.metrics.complexity(),
                    score: f.metrics.complexity_score(),
                    branches: f.metrics.branches,
                    loops: f.metrics.loops.len(),
                    recursive: f.metrics.recursive,
                    test: g.is_test(i),
                    module: g.is_module(i),
                    entry: g.is_root(i),
                    doc: f.comment.clone(),
                    language: g.lang(i).as_str(),
                }
            })
            .collect();
        let out: Vec<Vec<(usize, usize)>> = (0..n)
            .map(|i| {
                let f = g.func(i);
                let mut calls: Vec<(usize, usize)> = g.out[i]
                    .iter()
                    .filter(|&&t| t != i)
                    .map(|&t| {
                        let line = sites
                            .get(&(g.node_file(i), f.name.as_str(), g.name(t)))
                            .and_then(|ls| {
                                ls.iter()
                                    .copied()
                                    .filter(|&l| l >= f.start_line && l <= f.end_line)
                                    .min()
                            })
                            .unwrap_or(f.line);
                        (t, line)
                    })
                    .collect();
                calls.sort_by_key(|&(t, line)| (line, t));
                calls
            })
            .collect();
        let into: Vec<Vec<usize>> = (0..n)
            .map(|i| g.into[i].iter().copied().filter(|&c| c != i).collect())
            .collect();

        let paths: Vec<String> = caches.iter().map(|c| changes::path_str(&c.rel_path)).collect();
        let container_of: Vec<String> = (0..caches.len())
            .map(|fi| {
                ctx.of_file
                    .get(fi)
                    .and_then(|v| v.first())
                    .cloned()
                    .unwrap_or_else(|| UNASSIGNED.to_string())
            })
            .collect();
        let mut by_file: BTreeMap<String, Vec<usize>> =
            paths.iter().map(|p| (p.clone(), Vec::new())).collect();
        for (i, f) in funcs.iter().enumerate() {
            if let Some(ids) = by_file.get_mut(&f.file) {
                ids.push(i);
            }
        }

        let overview = json!({
            "schema": SCHEMA,
            "root": root_label,
            // for `vscode://file/...` links; the page is served on loopback only
            "root_path": root.to_string_lossy().replace('\\', "/"),
            "generated": generated,
            "system": system(caches, &out, root_label, &ctx.source),
            "containers": containers(&g, &ctx, caches, &paths, &container_of),
            "container_edges": container_edges(&insights::services(&g, &ctx)),
            "peers": ctx.externals.iter().map(|e| e.json()).collect::<Vec<_>>(),
            "outside": outside(&ctx, caches, &container_of),
            "components": components(caches, &paths, &container_of, &by_file, &funcs),
            "component_edges": component_edges(&g, &out, &paths),
            "note": "read off the syntax tree: an edge is a call the resolver found \
                     evidence for, a branch is a node in the tree - not a profile of \
                     what runs.",
        });
        Model {
            overview,
            funcs,
            out,
            into,
            by_file,
            container_of: paths.into_iter().zip(container_of).collect(),
        }
    }

    // the system, container and component levels, drawn from the whole map
    pub fn overview(&self) -> &Value {
        &self.overview
    }

    // The code inside one component: every function defined in the file, with
    // the calls between them, plus a stand-in for each function outside it
    // that one of them calls or is called by.
    pub fn code(&self, caches: &[FileCache], file: &str) -> Result<Value, String> {
        let path = file.trim().trim_start_matches("./");
        let ids = self
            .by_file
            .get(path)
            .ok_or_else(|| format!("no file `{path}` in the map"))?;
        let cache = caches.iter().find(|c| changes::path_str(&c.rel_path) == path);
        let mine: BTreeSet<usize> = ids.iter().copied().collect();
        // what this code calls is drawn first; its callers fill what is left
        let mut proxies: Vec<usize> = Vec::new();
        let mut seen: BTreeSet<usize> = BTreeSet::new();
        for &i in ids {
            for &(t, _) in &self.out[i] {
                if !mine.contains(&t) && seen.insert(t) {
                    proxies.push(t);
                }
            }
        }
        for &i in ids {
            for &c in &self.into[i] {
                if !mine.contains(&c) && seen.insert(c) {
                    proxies.push(c);
                }
            }
        }
        let truncated = proxies.len() > MAX_PROXIES;
        proxies.truncate(MAX_PROXIES);
        let shown: BTreeSet<usize> = mine.iter().chain(proxies.iter()).copied().collect();

        let functions: Vec<Value> = ids
            .iter()
            .map(|&i| {
                let mut v = self.funcs[i].json(i);
                v["calls"] = json!(self.out[i]
                    .iter()
                    .filter(|(t, _)| shown.contains(t))
                    .map(|&(t, line)| json!({"to": t, "line": line}))
                    .collect::<Vec<_>>());
                v["callers"] = json!(self.into[i]
                    .iter()
                    .filter(|c| shown.contains(c))
                    .collect::<Vec<_>>());
                v["calls_total"] = json!(self.out[i].len());
                v["callers_total"] = json!(self.into[i].len());
                v
            })
            .collect();
        let proxies: Vec<Value> = proxies
            .iter()
            .map(|&p| {
                let f = &self.funcs[p];
                let mut v = f.brief(p);
                v["container"] = json!(self.container_of.get(&f.file));
                v["language"] = json!(f.language);
                v
            })
            .collect();
        Ok(json!({
            "schema": SCHEMA,
            "file": path,
            "language": cache.map(|c| c.language.as_str()),
            "container": self.container_of.get(path),
            "lines": cache.map(|c| c.lines),
            "functions": functions,
            "types": cache
                .map(|c| {
                    c.types
                        .iter()
                        .map(|t| json!({"name": t.name, "kind": t.kind, "line": t.line}))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            "proxies": proxies,
            "proxies_truncated": truncated,
        }))
    }

    // The logic of one function as a node graph. The file is parsed again on
    // request rather than held: the outline is only wanted for the function
    // somebody opened, and the source is the one thing the map does not keep.
    pub fn flow(
        &self,
        caches: &[FileCache],
        root: &Path,
        file: &str,
        line: usize,
        name: Option<&str>,
    ) -> Result<Value, String> {
        let path = file.trim().trim_start_matches("./");
        let ids = self
            .by_file
            .get(path)
            .ok_or_else(|| format!("no file `{path}` in the map"))?;
        let on_line: Vec<usize> = ids.iter().copied().filter(|&i| self.funcs[i].line == line).collect();
        let id = on_line
            .iter()
            .copied()
            .find(|&i| name.is_some_and(|n| self.funcs[i].name == n))
            .or_else(|| on_line.iter().copied().find(|&i| !self.funcs[i].module))
            .or_else(|| on_line.first().copied())
            .ok_or_else(|| format!("nothing is defined on line {line} of `{path}`"))?;
        let f = &self.funcs[id];
        let lang = caches
            .iter()
            .find(|c| changes::path_str(&c.rel_path) == path)
            .map(|c| c.language)
            .ok_or_else(|| format!("no file `{path}` in the map"))?;
        let src = std::fs::read_to_string(root.join(path)).map_err(|e| format!("reading {path}: {e}"))?;
        let mut parser = Parser::new();
        parser.set_language(&lang.ts_language()).map_err(|e| e.to_string())?;
        let tree = parser.parse(&src, None).ok_or_else(|| format!("`{path}` did not parse"))?;
        // a module frame is the file's own top level
        let node = if f.module {
            Some(tree.root_node())
        } else {
            definition(tree.root_node(), lang, f.start, f.end)
        }
        .ok_or_else(|| {
            format!(
                "`{}` has moved since the map was built - the map refreshes within \
                 seconds, so try again",
                f.name
            )
        })?;

        let mut w = Walker {
            lang,
            src: &src,
            steps: 0,
            truncated: false,
            breaks_case: Vec::new(),
            calls: &self.out[id],
            funcs: &self.funcs,
            siblings: ids,
        };
        let mut steps = Vec::new();
        match node.child_by_field_name("body").filter(|_| !f.module) {
            Some(body) => w.visit(body, 0, 0, &mut steps),
            None => w.children(node, 0, 0, &mut steps),
        }
        Ok(json!({
            "schema": SCHEMA,
            "function": f.json(id),
            "language": lang.as_str(),
            "params": if f.module { Vec::new() } else { params(node, lang, &src) },
            "callers": self.into[id]
                .iter()
                .take(MAX_CALLERS)
                .map(|&c| self.funcs[c].brief(c))
                .collect::<Vec<_>>(),
            "callers_total": self.into[id].len(),
            "steps": steps,
            "step_count": w.steps,
            "truncated": w.truncated,
        }))
    }
}

// the whole system: what it holds, and in which languages
fn system(caches: &[FileCache], out: &[Vec<(usize, usize)>], root_label: &str, grouping: &str) -> Value {
    let mut langs: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for c in caches {
        let e = langs.entry(c.language.as_str()).or_default();
        e.0 += 1;
        e.1 += c.lines;
    }
    let mut langs: Vec<(&str, (usize, usize))> = langs.into_iter().collect();
    langs.sort_by_key(|&(l, (_, lines))| (std::cmp::Reverse(lines), l));
    json!({
        "name": root_label,
        "files": caches.len(),
        "lines": caches.iter().map(|c| c.lines).sum::<usize>(),
        "functions": caches.iter().map(|c| c.funcs.len()).sum::<usize>(),
        "edges": out.iter().map(Vec::len).sum::<usize>(),
        "languages": langs
            .iter()
            .map(|(l, (files, lines))| json!({"language": l, "files": files, "lines": lines}))
            .collect::<Vec<_>>(),
        // how files were grouped into containers - map.json, or a fallback
        "grouping": grouping,
    })
}

// The services, each with the languages it is written in as its technology.
// A service whose globs match nothing is still listed: the config names it.
fn containers(
    g: &insights::Graph,
    ctx: &insights::ServiceCtx,
    caches: &[FileCache],
    paths: &[String],
    container_of: &[String],
) -> Vec<Value> {
    #[derive(Default)]
    struct Tally<'a> {
        files: usize,
        lines: usize,
        funcs: usize,
        entries: usize,
        tests: usize,
        langs: BTreeMap<&'a str, usize>,
    }
    let mut tally: BTreeMap<&str, Tally> = BTreeMap::new();
    for (fi, c) in caches.iter().enumerate() {
        let t = tally.entry(container_of[fi].as_str()).or_default();
        t.files += 1;
        t.lines += c.lines;
        t.funcs += c.funcs.len();
        *t.langs.entry(c.language.as_str()).or_default() += 1;
        if changes::is_test_path(&paths[fi]) {
            t.tests += 1;
        }
    }
    for i in 0..g.nodes.len() {
        if g.is_root(i) && !g.is_test(i) && !g.is_module(i) {
            tally.entry(container_of[g.node_file(i)].as_str()).or_default().entries += 1;
        }
    }
    let mut names: Vec<&str> = ctx.map.keys().map(String::as_str).collect();
    if tally.contains_key(UNASSIGNED) {
        names.push(UNASSIGNED);
    }
    names
        .into_iter()
        .map(|name| {
            let t = tally.get(name);
            let mut langs: Vec<(&str, usize)> =
                t.map(|t| t.langs.iter().map(|(l, n)| (*l, *n)).collect()).unwrap_or_default();
            langs.sort_by_key(|&(l, n)| (std::cmp::Reverse(n), l));
            json!({
                "id": name,
                "name": name,
                "globs": ctx.map.get(name).cloned().unwrap_or_default(),
                "files": t.map_or(0, |t| t.files),
                "lines": t.map_or(0, |t| t.lines),
                "funcs": t.map_or(0, |t| t.funcs),
                "entries": t.map_or(0, |t| t.entries),
                "tests": t.map_or(0, |t| t.tests),
                "languages": langs
                    .iter()
                    .map(|(l, n)| json!({"language": l, "files": n}))
                    .collect::<Vec<_>>(),
            })
        })
        .collect()
}

// The service edges the insights pass already resolved - detected calls,
// declared relatives and annotated crossings alike - trimmed for drawing.
fn container_edges(services: &Value) -> Vec<Value> {
    let external: BTreeSet<&str> = services["external_names"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    services["edges"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            let sites: Vec<&Value> = e["sites"].as_array().into_iter().flatten().collect();
            let transports: BTreeSet<&str> = sites
                .iter()
                .filter_map(|s| s.get("transport").and_then(Value::as_str))
                .collect();
            json!({
                "from": e["from"],
                "to": e["to"],
                "count": e["count"],
                "symbols": e["symbols"].as_array().into_iter().flatten().take(EDGE_SYMBOLS).collect::<Vec<_>>(),
                "declared": e["declared"],
                "detected": e["detected"],
                "transports": transports,
                // the far end is a peer repository rather than a service here
                "external": e["to"].as_str().is_some_and(|t| external.contains(t)),
                "sites": sites
                    .iter()
                    .take(EDGE_SITES)
                    .map(|s| json!({
                        "symbol": s["symbol"],
                        "caller": s["caller"],
                        "caller_file": s["caller_file"],
                        "caller_line": s["caller_line"],
                        "target_file": s["target_file"],
                        "target_line": s["target_line"],
                    }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect()
}

// The system's edges to the world beyond the map: a `ccc:calls` nothing here
// or in a peer answers, and a `ccc:serves` nothing here calls. They group by
// transport and direction, because a key alone names no system.
fn outside(ctx: &insights::ServiceCtx, caches: &[FileCache], container_of: &[String]) -> Vec<Value> {
    let answered: BTreeSet<String> = ctx
        .crossings
        .iter()
        .filter(|c| !c.to.is_empty())
        .map(|c| norm_key(&c.key))
        .collect();
    let called: BTreeSet<String> = ctx
        .crossings
        .iter()
        .filter(|c| !c.from.is_empty())
        .map(|c| norm_key(&c.key))
        .collect();
    type Group<'a> = (BTreeSet<&'a str>, BTreeMap<&'a str, usize>);
    let mut groups: BTreeMap<(&str, &str), Group> = BTreeMap::new();
    for (fi, c) in caches.iter().enumerate() {
        for a in &c.annotations {
            let (direction, matched) = match a.boundary {
                Boundary::Calls => ("out", &answered),
                Boundary::Serves => ("in", &called),
            };
            if matched.contains(&norm_key(&a.key)) {
                continue;
            }
            let g = groups.entry((direction, a.transport.as_str())).or_default();
            g.0.insert(a.key.as_str());
            *g.1.entry(container_of[fi].as_str()).or_default() += 1;
        }
    }
    groups
        .iter()
        .map(|((direction, transport), (keys, by))| {
            json!({
                "id": format!("{direction}:{transport}"),
                "direction": direction,
                "transport": transport,
                "keys": keys.iter().take(EDGE_SYMBOLS).collect::<Vec<_>>(),
                "key_count": keys.len(),
                "containers": by
                    .iter()
                    .map(|(c, n)| json!({"container": c, "count": n}))
                    .collect::<Vec<_>>(),
            })
        })
        .collect()
}

// every mapped file, as a component of the container that owns it
fn components(
    caches: &[FileCache],
    paths: &[String],
    container_of: &[String],
    by_file: &BTreeMap<String, Vec<usize>>,
    funcs: &[FnNode],
) -> Vec<Value> {
    caches
        .iter()
        .enumerate()
        .map(|(fi, c)| {
            let path = &paths[fi];
            let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
            let ids = by_file.get(path).map(Vec::as_slice).unwrap_or_default();
            json!({
                "id": path,
                "container": container_of[fi],
                "dir": dir,
                "name": name,
                "language": c.language.as_str(),
                "lines": c.lines,
                "funcs": c.funcs.len(),
                "types": c.types.len(),
                "entries": ids
                    .iter()
                    .filter(|&&i| funcs[i].entry && !funcs[i].test && !funcs[i].module)
                    .count(),
                "max_complexity": ids.iter().map(|&i| funcs[i].complexity).max().unwrap_or(0),
                "test": changes::is_test_path(path),
                "withdrawn": c.withdrawn.len(),
            })
        })
        .collect()
}

// file-to-file edges, totalled from the function call graph
fn component_edges(g: &insights::Graph, out: &[Vec<(usize, usize)>], paths: &[String]) -> Vec<Value> {
    let mut edges: BTreeMap<(usize, usize), (usize, BTreeSet<&str>)> = BTreeMap::new();
    for (i, calls) in out.iter().enumerate() {
        for &(t, _) in calls {
            let (a, b) = (g.node_file(i), g.node_file(t));
            if a == b {
                continue;
            }
            let e = edges.entry((a, b)).or_default();
            e.0 += 1;
            e.1.insert(g.name(t));
        }
    }
    edges
        .iter()
        .map(|((a, b), (count, symbols))| {
            json!({
                "from": paths[*a],
                "to": paths[*b],
                "count": count,
                "symbols": symbols.iter().take(EDGE_SYMBOLS).collect::<Vec<_>>(),
            })
        })
        .collect()
}

// The node a map entry was read from: the outermost function whose span is
// exactly the one recorded. Subtrees that cannot hold it are never entered.
fn definition<'t>(root: Node<'t>, lang: Language, start: usize, end: usize) -> Option<Node<'t>> {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        let (s, e) = (line(n), n.end_position().row + 1);
        if s > end || e < start {
            continue;
        }
        if s == start && e == end && lang.func_kinds().contains(&n.kind()) {
            return Some(n);
        }
        let mut c = n.walk();
        let kids: Vec<Node<'t>> = n.named_children(&mut c).collect();
        stack.extend(kids.into_iter().rev());
    }
    None
}

// the parameter list as written, one entry per parameter
fn params(def: Node, lang: Language, src: &str) -> Vec<String> {
    let kinds = lang.param_list_kinds();
    let list = def
        .child_by_field_name("parameters")
        .filter(|n| kinds.contains(&n.kind()))
        .or_else(|| {
            // the first list in source order, never one inside the body
            let body = def.child_by_field_name("body");
            let mut stack = vec![def];
            while let Some(n) = stack.pop() {
                if kinds.contains(&n.kind()) {
                    return Some(n);
                }
                if Some(n) == body {
                    continue;
                }
                let mut c = n.walk();
                let kids: Vec<Node> = n.named_children(&mut c).collect();
                stack.extend(kids.into_iter().rev());
            }
            None
        });
    let Some(list) = list else { return Vec::new() };
    let mut c = list.walk();
    list.named_children(&mut c)
        .filter(|k| !lang.comment_kinds().contains(&k.kind()))
        .map(|k| clip(&oneline(&src[k.byte_range()]), ARG_CHARS + 12))
        .collect()
}

// Walks one function body into the steps a node graph draws: a call, a
// branch with an arm per way through it, a loop with its body, an exit, a
// closure. Anything else contributes the steps inside it, so an assignment
// shows as the calls on its right-hand side - which is all of it that runs.
struct Walker<'a> {
    lang: Language,
    src: &'a str,
    steps: usize,
    truncated: bool,
    // per enclosing construct: does a `break` inside it end a case rather than a loop
    breaks_case: Vec<bool>,
    // what the function calls, with the line of the first call - how a call
    // step finds the definition it reaches
    calls: &'a [(usize, usize)],
    funcs: &'a [FnNode],
    // the functions defined in this file, so a nested one links to itself
    siblings: &'a [usize],
}

impl<'a> Walker<'a> {
    fn text(&self, n: Node) -> &'a str {
        let src: &'a str = self.src;
        &src[n.byte_range()]
    }

    fn label(&self, n: Node) -> String {
        clip(&oneline(self.text(n)), LABEL_CHARS)
    }

    fn visit<'t>(&mut self, n: Node<'t>, nest: usize, depth: usize, out: &mut Vec<Value>) {
        if self.steps >= MAX_STEPS || nest > MAX_NESTING || depth > MAX_TREE_DEPTH {
            self.truncated = true;
            return;
        }
        let kind = n.kind();
        let lang = self.lang;
        if !n.is_named() || lang.comment_kinds().contains(&kind) {
            return;
        }
        if lang.func_kinds().contains(&kind) || lang.closure_kinds().contains(&kind) {
            self.closure(n, nest, depth, out);
        } else if lang.loop_kinds().contains(&kind) {
            self.looped(n, nest, depth, out);
        } else if self.is_if(kind) {
            self.branch(n, nest, depth, out);
        } else if lang.exit_kinds().contains(&kind) {
            self.exit(n, nest, depth, out);
        } else if self.is_call(kind) {
            self.call(n, nest, depth, out);
        } else {
            self.children(n, nest, depth + 1, out);
        }
    }

    // A node's children in order - unless some of them are the arms of a
    // many-way choice, which sit together under the statement choosing.
    fn children<'t>(&mut self, n: Node<'t>, nest: usize, depth: usize, out: &mut Vec<Value>) {
        let mut c = n.walk();
        let kids: Vec<Node<'t>> = n.named_children(&mut c).collect();
        if kids.iter().any(|k| self.is_arm(k.kind())) {
            self.cases(n, &kids, nest, depth, out);
            return;
        }
        for k in kids {
            self.visit(k, nest, depth, out);
        }
    }

    // a two-way choice: an `if`, a ternary, an `if` expression
    fn is_if(&self, kind: &str) -> bool {
        self.lang.branch_kinds().contains(&kind)
            && (kind.starts_with("if") || kind.contains("ternary") || kind == "conditional_expression")
    }

    // one arm of a many-way choice: a match arm, a case, a catch
    fn is_arm(&self, kind: &str) -> bool {
        // a default arm is no decision point, so the branch tables leave it out
        matches!(kind, "switch_default" | "default_case")
            || (self.lang.branch_kinds().contains(&kind) && !self.is_if(kind) && kind != "elif_clause")
    }

    // a call, a constructor call (js, ts, c++), or a rust macro - which runs
    // code just as a call does
    fn is_call(&self, kind: &str) -> bool {
        self.lang.call_kinds().contains(&kind)
            || kind == "new_expression"
            || (self.lang == Language::Rust && kind == "macro_invocation")
    }

    fn branch<'t>(&mut self, n: Node<'t>, nest: usize, depth: usize, out: &mut Vec<Value>) {
        let cond = n.child_by_field_name("condition");
        // the condition is evaluated before either side runs
        if let Some(c) = cond {
            self.visit(c, nest, depth + 1, out);
        }
        let (then, rest) = self.if_parts(n, cond);
        let mut yes = Vec::new();
        if let Some(t) = then {
            self.visit(t, nest + 1, depth + 1, &mut yes);
        }
        let no = self.else_steps(&rest, nest + 1, depth + 1);
        let kind = if n.kind().starts_with("if") { "if" } else { "ternary" };
        let label = cond.map(|c| self.label(c)).unwrap_or_else(|| kind.to_string());
        self.push_branch(out, kind, label, line(n), yes, no, rest.is_empty());
    }

    // the side taken when the condition holds, then whatever else follows
    fn if_parts<'t>(&self, n: Node<'t>, cond: Option<Node<'t>>) -> (Option<Node<'t>>, Vec<Node<'t>>) {
        let then = n
            .child_by_field_name("consequence")
            .or_else(|| n.child_by_field_name("body"));
        let mut rest = Vec::new();
        let mut unlabelled = Vec::new();
        let mut c = n.walk();
        for (i, k) in n.children(&mut c).enumerate() {
            if !k.is_named() || Some(k) == cond || Some(k) == then || self.lang.comment_kinds().contains(&k.kind()) {
                continue;
            }
            let field = n.field_name_for_child(i as u32);
            if field == Some("alternative") || matches!(k.kind(), "else_clause" | "elif_clause" | "else_if_clause") {
                rest.push(k);
            } else if field.is_none() {
                unlabelled.push(k);
            }
        }
        // zig's `if` expression labels nothing but its condition: the two
        // unlabelled children are the branches, in order
        if then.is_none() && !unlabelled.is_empty() {
            let then = unlabelled.remove(0);
            if rest.is_empty() {
                rest = unlabelled;
            }
            return (Some(then), rest);
        }
        (then, rest)
    }

    fn else_steps<'t>(&mut self, rest: &[Node<'t>], nest: usize, depth: usize) -> Vec<Value> {
        let mut out = Vec::new();
        let Some((&first, tail)) = rest.split_first() else {
            return out;
        };
        match first.kind() {
            // python's `elif` and odin's `else if` sit beside the `if`: each is
            // a branch of its own, with the rest of the chain as its else
            "elif_clause" | "else_if_clause" => {
                let cond = first.child_by_field_name("condition");
                if let Some(c) = cond {
                    self.visit(c, nest, depth, &mut out);
                }
                let mut yes = Vec::new();
                if let Some(t) = first.child_by_field_name("consequence") {
                    self.visit(t, nest + 1, depth + 1, &mut yes);
                }
                let no = self.else_steps(tail, nest + 1, depth + 1);
                let label = cond.map(|c| self.label(c)).unwrap_or_else(|| "elif".into());
                self.push_branch(&mut out, "if", label, line(first), yes, no, tail.is_empty());
            }
            // a wrapper around the else body, which may itself be an `if`
            "else_clause" => {
                let inner = ["alternative", "body", "consequence"]
                    .iter()
                    .find_map(|f| first.child_by_field_name(f))
                    .or_else(|| {
                        let mut c = first.walk();
                        let kids: Vec<Node<'t>> = first.named_children(&mut c).collect();
                        kids.into_iter().find(|k| !self.lang.comment_kinds().contains(&k.kind()))
                    });
                if let Some(i) = inner {
                    self.visit(i, nest, depth, &mut out);
                }
            }
            // go and c# put the else body, or the next `if`, there directly
            _ => self.visit(first, nest, depth, &mut out),
        }
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn push_branch(
        &mut self,
        out: &mut Vec<Value>,
        kind: &str,
        label: String,
        at: usize,
        yes: Vec<Value>,
        no: Vec<Value>,
        implicit_else: bool,
    ) {
        self.steps += 1;
        out.push(json!({
            "t": "branch",
            "kind": kind,
            "label": label,
            "line": at,
            "arms": [
                {"label": "true", "steps": yes},
                // no `else` written: the false side carries straight on
                {"label": "false", "steps": no, "implicit": implicit_else},
            ],
        }));
    }

    // A many-way choice: a match, a switch, a select, a try with its catches.
    // `n` holds the arms; the statement doing the choosing is `n` itself (go,
    // zig, odin, a try) or the one holding the block they sit in (rust, js, c,
    // c#, python's match).
    fn cases<'t>(&mut self, n: Node<'t>, kids: &[Node<'t>], nest: usize, depth: usize, out: &mut Vec<Value>) {
        let subject_of = |s: Node<'t>| {
            ["value", "subject", "condition"]
                .iter()
                .find_map(|f| s.child_by_field_name(f))
        };
        let is_try = |s: Node<'t>| s.kind().contains("try");
        let (stmt, own) = if subject_of(n).is_some() || is_try(n) {
            (n, true)
        } else {
            match n.parent().filter(|p| subject_of(*p).is_some() || is_try(*p)) {
                Some(p) => (p, false),
                None => (n, true),
            }
        };
        let tried = is_try(stmt);
        let mut subject = subject_of(stmt);
        if own && subject.is_none() && !tried {
            // zig and c# switch expressions name their subject first, unlabelled
            subject = kids.iter().copied().find(|k| !self.is_arm(k.kind()));
        }
        // the subject is read once, before any arm; when the arms sit in a
        // block, the walk over the statement has already passed it
        if own {
            if let Some(s) = subject {
                self.visit(s, nest, depth + 1, out);
            }
        }
        let body = stmt.child_by_field_name("body").filter(|_| tried);
        let mut arms = Vec::new();
        if let Some(b) = body {
            let mut steps = Vec::new();
            self.visit(b, nest + 1, depth + 1, &mut steps);
            arms.push(json!({"label": "try", "line": line(b), "steps": steps}));
        }
        let ends_case = !tried && self.lang.break_ends_case();
        let mut after = Vec::new();
        for &k in kids {
            if self.is_arm(k.kind()) {
                self.breaks_case.push(ends_case);
                let steps = self.arm_steps(k, nest + 1, depth + 1);
                self.breaks_case.pop();
                arms.push(json!({"label": self.arm_label(k), "line": line(k), "steps": steps}));
            } else if Some(k) != subject && Some(k) != body {
                // `finally`, python's `try ... else`: they run once the arms are done
                after.push(k);
            }
        }
        let kind = if tried {
            "try"
        } else if stmt.kind().contains("match") {
            "match"
        } else if stmt.kind().contains("select") {
            "select"
        } else {
            "switch"
        };
        let label = subject.map(|s| self.label(s)).unwrap_or_else(|| kind.to_string());
        self.steps += 1;
        out.push(json!({"t": "branch", "kind": kind, "label": label, "line": line(stmt), "arms": arms}));
        for k in after {
            self.visit(k, nest, depth + 1, out);
        }
    }

    // What an arm matches on - its pattern, case value or caught exception.
    // Rust calls an arm's result `value`, so a pattern always wins.
    fn arm_head<'t>(&self, arm: Node<'t>) -> Option<Node<'t>> {
        ["pattern", "value", "type", "communication", "parameter", "parameters", "condition"]
            .iter()
            .find_map(|f| arm.child_by_field_name(f))
            .or_else(|| {
                arm.named_child(0).filter(|c| {
                    let k = c.kind();
                    k.ends_with("pattern") || k == "catch_declaration" || k == "discard"
                })
            })
    }

    fn arm_label(&self, arm: Node) -> String {
        let kind = arm.kind();
        let head = self.arm_head(arm).map(|h| self.label(h));
        if kind.contains("catch") {
            return match head {
                Some(h) if h.starts_with('(') => format!("catch {h}"),
                Some(h) => format!("catch ({h})"),
                None => "catch".into(),
            };
        }
        if kind.contains("except") {
            return head.map_or_else(|| "except".into(), |h| format!("except {h}"));
        }
        let text = head.unwrap_or_else(|| arm_text(self.text(arm)));
        if text.is_empty() || kind.contains("default") {
            "default".into()
        } else {
            text
        }
    }

    fn arm_steps<'t>(&mut self, arm: Node<'t>, nest: usize, depth: usize) -> Vec<Value> {
        let head = self.arm_head(arm);
        let mut out = Vec::new();
        let mut c = arm.walk();
        let kids: Vec<Node<'t>> = arm.named_children(&mut c).collect();
        for k in kids {
            if Some(k) != head {
                self.visit(k, nest, depth, &mut out);
            }
        }
        out
    }

    fn looped<'t>(&mut self, n: Node<'t>, nest: usize, depth: usize, out: &mut Vec<Value>) {
        // odin keeps a loop's body under `consequence`
        let body = n
            .child_by_field_name("body")
            .or_else(|| n.child_by_field_name("consequence"));
        let label = match body {
            Some(b) => {
                let head = format!(
                    "{} {}",
                    &self.src[n.start_byte()..b.start_byte()],
                    &self.src[b.end_byte()..n.end_byte()]
                );
                clip(oneline(&head).trim_end_matches([';', ':', '{', ' ']), LABEL_CHARS)
            }
            None => self.label(n),
        };
        let mut inner = Vec::new();
        let mut c = n.walk();
        let kids: Vec<Node<'t>> = n.named_children(&mut c).collect();
        self.breaks_case.push(false);
        match body {
            // a `do ... while` tests its condition after each pass, not before
            Some(b) if n.kind().starts_with("do") => {
                self.visit(b, nest + 1, depth + 1, &mut inner);
                for k in kids.into_iter().filter(|k| *k != b) {
                    self.visit(k, nest + 1, depth + 1, &mut inner);
                }
            }
            Some(b) => {
                // the header - the iterable, the condition - is read before the first pass
                for k in kids.into_iter().filter(|k| *k != b) {
                    self.visit(k, nest, depth + 1, out);
                }
                self.visit(b, nest + 1, depth + 1, &mut inner);
            }
            // a comprehension is all body
            None => {
                for k in kids {
                    self.visit(k, nest + 1, depth + 1, &mut inner);
                }
            }
        }
        self.breaks_case.pop();
        self.steps += 1;
        out.push(json!({
            "t": "loop",
            "kind": loop_label(n.kind()),
            "label": label,
            "line": line(n),
            "body": inner,
        }));
    }

    fn exit<'t>(&mut self, n: Node<'t>, nest: usize, depth: usize, out: &mut Vec<Value>) {
        let kind = n.kind();
        let what = ["return", "throw", "raise", "break", "continue"]
            .into_iter()
            .find(|w| kind.starts_with(w))
            .unwrap_or("return");
        // in a c-family or go switch `break` ends the case, which the end of
        // the arm already draws
        if what == "break" && self.breaks_case.last() == Some(&true) {
            return;
        }
        // the value returned or thrown is computed first
        let mut c = n.walk();
        let kids: Vec<Node<'t>> = n.named_children(&mut c).collect();
        for k in kids {
            self.visit(k, nest, depth + 1, out);
        }
        self.steps += 1;
        out.push(json!({"t": "exit", "kind": what, "label": self.label(n), "line": line(n)}));
    }

    fn call<'t>(&mut self, n: Node<'t>, nest: usize, depth: usize, out: &mut Vec<Value>) {
        let callee = ["function", "constructor", "type", "macro"]
            .iter()
            .find_map(|f| n.child_by_field_name(f));
        // the callee expression runs first - `a().b()` calls `a` before `b` -
        // then each argument, in order
        if let Some(c) = callee {
            self.visit(c, nest, depth + 1, out);
        }
        let mut c = n.walk();
        let rest: Vec<Node<'t>> = n.named_children(&mut c).filter(|k| Some(*k) != callee).collect();
        for &k in &rest {
            self.visit(k, nest, depth + 1, out);
        }

        let macro_call = n.kind() == "macro_invocation";
        let args: Vec<String> = match n.child_by_field_name("arguments") {
            Some(a) => {
                let mut c = a.walk();
                let list: Vec<Node> = a
                    .named_children(&mut c)
                    .filter(|k| !self.lang.comment_kinds().contains(&k.kind()))
                    .collect();
                list.iter()
                    .take(MAX_ARGS)
                    .map(|k| clip(&oneline(self.text(*k)), ARG_CHARS))
                    .collect()
            }
            // a macro's tokens are not parsed, so they are shown as written
            None if macro_call => rest
                .iter()
                .map(|k| {
                    let t = oneline(self.text(*k));
                    clip(t.trim_start_matches(['(', '[', '{']).trim_end_matches([')', ']', '}']), ARG_CHARS * 2)
                })
                .filter(|t| !t.is_empty())
                .collect(),
            None => Vec::new(),
        };
        // a function written and called on the spot - `go func() { .. }()`, an
        // iife - has no name, and its text is already drawn as the closure
        let inline = callee.is_some_and(|c| {
            self.lang.func_kinds().contains(&c.kind()) || self.lang.closure_kinds().contains(&c.kind())
        });
        let text = match callee {
            Some(_) if inline => "(closure)".to_string(),
            Some(c) => oneline(self.text(c)),
            None => String::new(),
        };
        let (qual, name) = split_callee(&text);
        let mut name = clip(&name, LABEL_CHARS);
        let target = self.resolve(&name, line(n));
        if macro_call {
            name.push('!');
        }
        let mode = match n.parent().map(|p| p.kind()) {
            Some("await_expression" | "await") => Some("await"),
            Some("defer_statement") => Some("defer"),
            Some("go_statement") => Some("go"),
            Some("try_expression") => Some("try"),
            _ if matches!(n.kind(), "new_expression" | "object_creation_expression") => Some("new"),
            _ => None,
        };
        self.steps += 1;
        out.push(json!({
            "t": "call",
            "name": name,
            "qual": qual,
            "line": line(n),
            "args": args,
            "mode": mode,
            "target": target,
        }));
    }

    // The definition a call reaches, when the resolver found evidence for one:
    // the same name, nearest the call's own line. An rpc stub's spelling can
    // differ from its handler's by case and underscores alone.
    fn resolve(&self, name: &str, at: usize) -> Value {
        let norm = |s: &str| s.replace('_', "").to_ascii_lowercase();
        let want = norm(name);
        let named: Vec<(usize, usize)> = self
            .calls
            .iter()
            .copied()
            .filter(|&(t, _)| norm(&self.funcs[t].name) == want)
            .collect();
        named
            .iter()
            .find(|&&(_, l)| l == at)
            .or(named.first())
            .map_or(Value::Null, |&(t, _)| self.funcs[t].brief(t))
    }

    // A function written inside this one. A named definition is a node of its
    // own in the map, so it is linked rather than drawn twice; an anonymous
    // closure has no other home, so its body is drawn here.
    fn closure<'t>(&mut self, n: Node<'t>, nest: usize, depth: usize, out: &mut Vec<Value>) {
        let (s, e) = (line(n), n.end_position().row + 1);
        let own = self
            .siblings
            .iter()
            .copied()
            .find(|&i| self.funcs[i].start == s && self.funcs[i].end == e && !self.funcs[i].module);
        if let Some(id) = own {
            self.steps += 1;
            out.push(json!({
                "t": "def",
                "label": self.funcs[id].name,
                "line": s,
                "target": self.funcs[id].brief(id),
            }));
            return;
        }
        let params = n.child_by_field_name("parameters");
        let label = params.map_or_else(|| "closure".to_string(), |p| format!("closure {}", oneline(self.text(p))));
        let mut inner = Vec::new();
        self.breaks_case.push(false);
        match n.child_by_field_name("body") {
            Some(b) => self.visit(b, nest + 1, depth + 1, &mut inner),
            None => {
                let mut c = n.walk();
                let kids: Vec<Node<'t>> = n.named_children(&mut c).filter(|k| Some(*k) != params).collect();
                for k in kids {
                    self.visit(k, nest + 1, depth + 1, &mut inner);
                }
            }
        }
        self.breaks_case.pop();
        self.steps += 1;
        out.push(json!({"t": "closure", "label": clip(&label, LABEL_CHARS), "line": s, "body": inner}));
    }
}

fn line(n: Node) -> usize {
    n.start_position().row + 1
}

fn oneline(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// An arm's head when the grammar labels none of it: the text up to `=>`, or
// to a `:` that is not part of `::`, with a leading `case` dropped.
fn arm_text(arm: &str) -> String {
    let first = arm.lines().next().unwrap_or_default();
    let bytes = first.as_bytes();
    let mut end = first.len();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'=' if bytes.get(i + 1) == Some(&b'>') => {
                end = i;
                break;
            }
            b':' if bytes.get(i + 1) == Some(&b':') => i += 1,
            b':' | b'{' => {
                end = i;
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let head = first[..end].trim();
    let head = head.strip_prefix("case ").unwrap_or(if head == "case" { "" } else { head });
    clip(head.trim(), LABEL_CHARS)
}

// `billing::charge` -> (Some("billing"), "charge"); generic arguments name no
// part of the callee, so `Vec::<u8>::new` reads as `Vec::new`
fn split_callee(text: &str) -> (Option<String>, String) {
    let mut plain = String::new();
    let mut depth = 0usize;
    for ch in text.chars() {
        match ch {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => plain.push(ch),
            _ => {}
        }
    }
    let plain = plain.replace("::::", "::");
    let cut = ["::", "->", "."]
        .iter()
        .filter_map(|sep| plain.rfind(sep).map(|i| (i, sep.len())))
        .max_by_key(|&(i, _)| i);
    match cut {
        Some((i, len)) if i > 0 => (
            Some(clip(plain[..i].trim_end_matches('?'), 32)),
            plain[i + len..].trim().to_string(),
        ),
        _ => (None, plain.trim().to_string()),
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// The page `ccc run --vis` serves at `/vis`: one self-contained document - no
// CDN, no build step - so it works offline. Its stylesheet and script are
// kept as files of their own so they read, diff and lint as what they are.
pub fn page(root_label: &str) -> String {
    const HTML: &str = include_str!("../assets/vis/vis.html");
    const CSS: &str = include_str!("../assets/vis/vis.css");
    const JS: &str = include_str!("../assets/vis/vis.js");
    HTML.replace("__CCC_ROOT__", &esc(root_label))
        .replace("/*__CCC_VIS_CSS__*/", CSS)
        // `<\/` keeps a `</script` inside the script from closing the element
        .replace("/*__CCC_VIS_JS__*/", &JS.replace("</script", "<\\/script"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;
    use std::fs;
    use std::path::PathBuf;

    // a project on disk, removed on drop; `tag` keeps parallel tests apart
    struct Project(PathBuf);

    impl Drop for Project {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn project(tag: &str, files: &[(&str, &str)]) -> (Project, Vec<FileCache>, Model) {
        let dir = std::env::temp_dir().join(format!("ccc-vis-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (path, text) in files {
            let p = dir.join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
        let caches = scan::build_caches(&dir, &scan::collect_files(&dir).unwrap());
        let contracts = ContractIndex::for_root(&dir, &caches);
        let model = Model::build(&caches, &dir, "demo", "now", &contracts);
        (Project(dir), caches, model)
    }

    // a flow as one line per step, in drawing order: arms and bodies follow
    // the step that owns them
    fn shape(steps: &Value, out: &mut Vec<String>) {
        for s in steps.as_array().into_iter().flatten() {
            let field = |k: &str| s[k].as_str().unwrap_or_default().to_string();
            match field("t").as_str() {
                "call" => out.push(format!("call {}", field("name"))),
                "branch" => {
                    out.push(format!("branch {} {}", field("kind"), field("label")));
                    for arm in s["arms"].as_array().into_iter().flatten() {
                        out.push(format!("arm {}", arm["label"].as_str().unwrap_or_default()));
                        shape(&arm["steps"], out);
                    }
                }
                "loop" => {
                    out.push(format!("loop {}", field("kind")));
                    shape(&s["body"], out);
                }
                "closure" => {
                    out.push("closure".into());
                    shape(&s["body"], out);
                }
                "exit" => out.push(format!("exit {}", field("kind"))),
                other => out.push(format!("{other} {}", field("label"))),
            }
        }
    }

    fn flow_of(p: &Project, caches: &[FileCache], m: &Model, file: &str, name: &str) -> Vec<String> {
        let id = m.by_file[file]
            .iter()
            .copied()
            .find(|&i| m.funcs[i].name == name)
            .unwrap_or_else(|| panic!("no `{name}` in {file}"));
        let v = m.flow(caches, &p.0, file, m.funcs[id].line, Some(name)).unwrap();
        let mut out = Vec::new();
        shape(&v["steps"], &mut out);
        out
    }

    // every `want` appears in `got`, in this order, with anything between
    fn in_order(got: &[String], want: &[&str]) -> bool {
        let mut at = 0;
        for w in want {
            match got[at..].iter().position(|g| g == w) {
                Some(i) => at += i + 1,
                None => return false,
            }
        }
        true
    }

    const RUST: &str = "fn f(x: i32) -> i32 {
    if x > 0 { a(); } else if x < 0 { b(); } else { c(); }
    match x { 1 => d(), _ => { e(); } }
    for i in items() { if i { break; } continue; }
    while g() { h(); }
    loop { return k(x).m(); }
    let f = |y| n(y);
    println!(\"{}\", p());
    q()?;
}
";
    const PYTHON: &str = "def f(x):
    if x > 0:
        a()
    elif x < 0:
        b()
    else:
        c()
    match x:
        case 1:
            d()
        case _:
            e()
    for i in items():
        break
    while g():
        h()
    try:
        k()
    except ValueError as e:
        m()
    finally:
        n()
    raise X()
    return p(x) if q() else r()
";
    const JS: &str = "function f(x) {
  if (x > 0) { a(); } else if (x < 0) { b(); } else { c(); }
  switch (x) { case 1: d(); break; default: e(); }
  for (const i of items()) { continue; }
  while (g()) { h(); }
  do { k(); } while (m());
  try { n(); } catch (e) { p(); } finally { q(); }
  const z = x ? r() : s();
  items().map((y) => t(y));
  throw new Error(u());
}
";
    const TS: &str = "function f(x: number): number {
  if (x > 0) { a(); } else { c(); }
  for (const i of items()) { continue; }
  const z = x ? r() : s();
  return v();
}
";
    const GO: &str = "package p
func f(x int) int {
	if x > 0 { a() } else if x < 0 { b() } else { c() }
	switch x { case 1: d(); default: e() }
	switch v := y.(type) { case int: g() }
	select { case <-ch: h() }
	for i := range items() { break }
	go func() { k() }()
	return m()
}
";
    const C: &str = "int f(int x) {
  if (x > 0) { a(); } else if (x < 0) { b(); } else { c(); }
  switch (x) { case 1: d(); break; default: e(); }
  while (g()) { h(); }
  do { k(); } while (m());
  return x ? n() : p();
}
";
    const CPP: &str = "int f(int x) {
  switch (x) { case 1: d(); break; default: e(); }
  for (auto i : items()) { continue; }
  try { n(); } catch (const std::exception& e) { p(); }
  auto l = [](int y) { return q(y); };
  obj.method(z());
  throw std::runtime_error(r());
}
";
    const CSHARP: &str = "class C { int F(int x) {
  if (x > 0) { A(); } else if (x < 0) { B(); } else { Cc(); }
  switch (x) { case 1: D(); break; default: E(); break; }
  var y = x switch { 1 => G(), _ => H() };
  foreach (var i in Items()) { continue; }
  try { N(); } catch (Exception e) { P(); } finally { Q(); }
  var o = new Foo(R());
  return x > 0 ? S() : T();
} }
";
    const ZIG: &str = "fn f(x: i32) i32 {
    if (x > 0) { a(); } else if (x < 0) { b(); } else { c(); }
    switch (x) { 1 => d(), else => e() }
    for (items()) |i| { _ = i; break; }
    while (g()) { h(); continue; }
    const y = if (x > 0) k() else m();
    return n();
}
";
    const ODIN: &str = "package p
f :: proc(x: int) -> int {
	if x > 0 { a() } else if x < 0 { b() } else { c() }
	switch x { case 1: d() case: e() }
	for i in items() { break }
	defer k()
	return m()
}
";

    // One function per language, written with the same shapes: an if chain,
    // a many-way choice, loops, closures, exits. The walker knows a grammar
    // only through the kind tables, so each has to come out drawn alike - in
    // the order the code runs, with a c-family `break` ending its case rather
    // than drawn as an exit.
    #[test]
    fn the_flow_walker_draws_every_language_alike() {
        let (p, caches, m) = project(
            "flow",
            &[
                ("f.rs", RUST),
                ("f.py", PYTHON),
                ("f.js", JS),
                ("f.ts", TS),
                ("f.go", GO),
                ("f.c", C),
                ("f.cpp", CPP),
                ("f.cs", CSHARP),
                ("f.zig", ZIG),
                ("f.odin", ODIN),
            ],
        );
        let cases: &[(&str, &str, &[&str], &[&str])] = &[
            (
                "f.rs",
                "f",
                &[
                    "branch if x > 0", "arm true", "call a", "arm false", "branch if x < 0", "call b", "call c",
                    "branch match x", "arm 1", "call d", "arm _", "call e",
                    "call items", "loop for", "branch if i", "exit break", "exit continue",
                    "call g", "loop while", "call h",
                    "loop loop", "call k", "call m", "exit return",
                    "closure", "call n", "call println!", "call q",
                ],
                &[],
            ),
            (
                "f.py",
                "f",
                &[
                    "branch if x > 0", "call a", "branch if x < 0", "call b", "call c",
                    "branch match x", "arm 1", "call d", "arm _", "call e",
                    "call items", "loop for", "exit break", "call g", "loop while", "call h",
                    "branch try try", "arm try", "call k", "arm except ValueError as e", "call m", "call n",
                    "call X", "exit raise", "call p", "call q", "call r", "exit return",
                ],
                &[],
            ),
            (
                "f.js",
                "f",
                &[
                    "branch if (x > 0)", "call a", "branch if (x < 0)", "call b", "call c",
                    "branch switch (x)", "arm 1", "call d", "arm default", "call e",
                    "call items", "loop for", "exit continue", "call g", "loop while", "call h",
                    "loop do", "call k", "call m",
                    "branch try try", "arm try", "call n", "arm catch (e)", "call p", "call q",
                    "branch ternary x", "arm true", "call r", "arm false", "call s",
                    "call items", "closure", "call t", "call map",
                    "call u", "call Error", "exit throw",
                ],
                &["exit break"],
            ),
            (
                "f.ts",
                "f",
                &["branch if (x > 0)", "call a", "call c", "call items", "loop for", "exit continue", "branch ternary x", "call r", "call s", "call v", "exit return"],
                &[],
            ),
            (
                "f.go",
                "f",
                &[
                    "branch if x > 0", "call a", "branch if x < 0", "call b", "call c",
                    "branch switch x", "arm 1", "call d", "arm default", "call e",
                    "branch switch y", "arm int", "call g",
                    "branch select select", "arm <-ch", "call h",
                    "call items", "loop for", "exit break",
                    "closure", "call k", "call (closure)", "call m", "exit return",
                ],
                &[],
            ),
            (
                "f.c",
                "f",
                &[
                    "branch if (x > 0)", "call a", "branch if (x < 0)", "call b", "call c",
                    "branch switch (x)", "arm 1", "call d", "arm default", "call e",
                    "call g", "loop while", "call h", "loop do", "call k", "call m",
                    "branch ternary x", "call n", "call p", "exit return",
                ],
                &["exit break"],
            ),
            (
                "f.cpp",
                "f",
                &[
                    "branch switch (x)", "arm 1", "call d", "arm default", "call e",
                    "call items", "loop for", "exit continue",
                    "branch try try", "arm try", "call n", "arm catch (const std::exception& e)", "call p",
                    "closure", "call q", "exit return",
                    "call z", "call method", "call r", "call runtime_error", "exit throw",
                ],
                &["exit break"],
            ),
            (
                "f.cs",
                "F",
                &[
                    "branch if x > 0", "call A", "branch if x < 0", "call B", "call Cc",
                    "branch switch x", "arm 1", "call D", "arm default", "call E",
                    "branch switch x", "arm 1", "call G", "arm _", "call H",
                    "call Items", "loop for", "exit continue",
                    "branch try try", "call N", "arm catch (Exception e)", "call P", "call Q",
                    "call R", "call Foo", "branch ternary x > 0", "call S", "call T", "exit return",
                ],
                &["exit break"],
            ),
            (
                "f.zig",
                "f",
                &[
                    "branch if x > 0", "call a", "branch if x < 0", "call b", "call c",
                    "branch switch x", "arm 1", "call d", "arm else", "call e",
                    "call items", "loop for", "exit break",
                    "call g", "loop while", "call h", "exit continue",
                    "branch if x > 0", "call k", "call m", "call n", "exit return",
                ],
                &[],
            ),
            (
                "f.odin",
                "f",
                &[
                    "branch if x > 0", "call a", "branch if x < 0", "call b", "call c",
                    "branch switch x", "arm 1", "call d", "arm default", "call e",
                    "call items", "loop for", "exit break", "call k", "call m", "exit return",
                ],
                &[],
            ),
        ];
        for (file, name, want, never) in cases {
            let got = flow_of(&p, &caches, &m, file, name);
            assert!(in_order(&got, want), "{file}: wanted, in order:\n  {}\ngot:\n  {}", want.join("\n  "), got.join("\n  "));
            for n in *never {
                assert!(!got.iter().any(|g| g == n), "{file}: `{n}` should not be drawn:\n  {}", got.join("\n  "));
            }
        }
    }

    // a schema's rpc has no body: its flow is empty, not an error
    #[test]
    fn an_rpc_has_an_empty_flow() {
        let (p, caches, m) = project(
            "rpc",
            &[("billing.proto", "syntax = \"proto3\";\nservice Billing {\n  rpc Charge(Req) returns (Res);\n}\nmessage Req {}\nmessage Res {}\n")],
        );
        assert!(flow_of(&p, &caches, &m, "billing.proto", "Charge").is_empty());
    }

    // The levels above the code - containers from directories, the calls
    // between them, the components inside each, a call nothing answers - and
    // the code of one file with stand-ins for what it reaches beyond itself.
    #[test]
    fn the_levels_roll_calls_up_from_functions_to_containers() {
        let (p, caches, m) = project(
            "levels",
            &[
                (
                    "api/server.py",
                    "from core.store import save\n\n# ccc:calls http billing.charge\ndef handle(x):\n    return save(x)\n",
                ),
                ("core/store.py", "def save(x):\n    return clean(x)\n\ndef clean(x):\n    return x\n"),
            ],
        );
        let o = m.overview();
        assert_eq!(o["schema"], SCHEMA);
        let names: Vec<&str> = o["containers"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["api", "core"]);
        let edge = o["container_edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["from"] == "api" && e["to"] == "core")
            .unwrap_or_else(|| panic!("api calls core: {o:#}"));
        assert_eq!(edge["detected"], true);
        assert!(edge["symbols"].as_array().unwrap().iter().any(|s| s == "save"));
        assert!(o["component_edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["from"] == "api/server.py" && e["to"] == "core/store.py"));
        // nothing answers the annotated call, so it leaves the map
        let outside = &o["outside"][0];
        assert_eq!(outside["direction"], "out");
        assert_eq!(outside["transport"], "http");
        assert_eq!(outside["containers"][0]["container"], "api");

        let code = m.code(&caches, "core/store.py").unwrap();
        let fns: Vec<&str> = code["functions"].as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap()).collect();
        assert_eq!(fns, ["save", "clean"]);
        assert!(code["proxies"].as_array().unwrap().iter().any(|x| x["name"] == "handle" && x["file"] == "api/server.py"));
        let clean = code["functions"][1]["id"].clone();
        assert!(code["functions"][0]["calls"].as_array().unwrap().iter().any(|c| c["to"] == clean));
        assert!(m.code(&caches, "nowhere.py").is_err());

        // a call step names the definition it reaches, across files
        let v = m.flow(&caches, &p.0, "api/server.py", 4, Some("handle")).unwrap();
        assert_eq!(v["steps"][0]["name"], "save");
        assert_eq!(v["steps"][0]["target"]["file"], "core/store.py");
        assert_eq!(v["params"][0], "x");
        assert!(m.flow(&caches, &p.0, "api/server.py", 99, None).is_err());
    }

    #[test]
    fn a_callee_reads_as_its_name_and_qualifier() {
        assert_eq!(split_callee("billing::charge"), (Some("billing".into()), "charge".into()));
        assert_eq!(split_callee("Vec::<u8>::new"), (Some("Vec".into()), "new".into()));
        assert_eq!(split_callee("self.client.send"), (Some("self.client".into()), "send".into()));
        assert_eq!(split_callee("ptr->run"), (Some("ptr".into()), "run".into()));
        assert_eq!(split_callee("plain"), (None, "plain".into()));
        assert_eq!(arm_text("Some(x) if x > 0 => {"), "Some(x) if x > 0");
        assert_eq!(arm_text("case Foo::Bar: go();"), "Foo::Bar");
        assert_eq!(arm_text("default: e();"), "default");
        assert_eq!(arm_text("case: e()"), "");
    }
}
