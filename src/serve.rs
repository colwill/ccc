//! `ccc serve` local REST/MCP endpoints for AI agents.
//!
//! On startup the whole project is parsed into an in-memory map (the same
//! model `.ccc` is rendered from); every query answers from memory. A watcher
//! thread polls a walk fingerprint (path + mtime + size) and swaps a freshly
//! parsed map in whenever source changes - `/refresh` forces it immediately.

use crate::model::{FileCache, Counts};
use crate::{render, scan};
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

const MCP_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
const MCP_LATEST: &str = "2025-06-18";

const FIND_CAP: usize = 200;
const REFS_CAP: usize = 500;
const EDGE_SYMBOL_CAP: usize = 20;

pub struct ServeOptions {
    pub addr: String,
    pub port: u16,
    pub watch: Option<std::time::Duration>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        ServeOptions {
            addr: "127.0.0.1".into(),
            port: 6767,
            watch: Some(std::time::Duration::from_secs(2)),
        }
    }
}

struct MapState {
    root: PathBuf,
    root_label: String,
    ts: String,
    caches: Vec<FileCache>,
    watch_secs: Option<u64>,
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
        })
    }

    fn rescan(&mut self) -> Result<(usize, usize)> {
        let before = self.caches.len();
        let files = scan::collect_files(&self.root)?;
        self.caches = scan::build_caches(&self.root, &files);
        self.ts = render::now_ts();
        Ok((before, self.caches.len()))
    }

    // swap in a fresh map (built outside lock by watcher)
    fn swap_in(&mut self, caches: Vec<FileCache>) {
        self.caches = caches;
        self.ts = render::now_ts();
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
    if !matches!(kind, "any" | "func" | "const" | "note") {
        return Err(format!("kind '{kind}' not one of any|func|const|note"));
    }
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

fn q_references(map: &MapState, symbol: &str) -> Result<Value, String> {
    let symbol = symbol.trim();
    if symbol.is_empty() {
        return Err("empty symbol".into());
    }
    let mut definitions = Vec::new();
    let mut references = Vec::new();
    for c in &map.caches {
        let path = map.path_of(c);
        for f in &c.funcs {
            if f.name == symbol {
                definitions.push(json!({
                    "file": path, "line": f.line, "col": f.col,
                    "ret": f.ret, "doc": f.comment, "span": [f.start_line, f.end_line],
                }));
            }
        }
        for call in &c.calls {
            if call.name == symbol {
                references.push(json!({
                    "file": path, "line": call.line, "caller": call.caller,
                    "qualifier": call.qualifier, "test_ctx": call.test_ctx,
                }));
            }
        }
    }
    if definitions.is_empty() && references.is_empty() {
        return Err(format!("symbol '{symbol}' not found in the map"));
    }
    let total_refs = references.len();
    references.truncate(REFS_CAP);
    Ok(json!({
        "symbol": symbol,
        "counts": {"definitions": definitions.len(), "references": total_refs},
        "truncated": total_refs > REFS_CAP,
        "definitions": definitions,
        "references": references,
    }))
}

// File-level dependency edges
fn q_dependencies(map: &MapState, file: Option<&str>) -> Result<Value, String> {
    let defs = map.def_files();
    // (from, to) -> symbols
    let mut edges: BTreeMap<(usize, usize), BTreeSet<String>> = BTreeMap::new();
    let mut ambiguous: BTreeSet<String> = BTreeSet::new();

    for (a, c) in map.caches.iter().enumerate() {
        for call in &c.calls {
            let Some(files) = defs.get(call.name.as_str()) else { continue };
            if files.contains(&a) {
                continue; // resolv locally
            }
            let others: Vec<usize> = files.iter().copied().collect();
            let target = if others.len() == 1 {
                Some(others[0])
            } else {
                let by_stem: Vec<usize> = others
                    .iter()
                    .copied()
                    .filter(|&b| {
                        let stem = map.caches[b]
                            .rel_path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("");
                        call.qualifier
                            .as_deref()
                            .map(|q| crate::surf::qualifier_names_service(q, stem))
                            .unwrap_or(false)
                    })
                    .collect();
                if by_stem.len() == 1 {
                    Some(by_stem[0])
                } else {
                    ambiguous.insert(call.name.clone());
                    None
                }
            };
            if let Some(b) = target {
                edges.entry((a, b)).or_default().insert(call.name.clone());
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
            "Search the code map for symbols by substring (case-insensitive). Returns file:line locations with return types and doc summaries.",
            json!({
                "query": {"type": "string", "description": "substring to search for"},
                "kind": {"type": "string", "enum": ["any", "func", "const", "note"], "description": "filter by symbol kind (default any)"},
            }),
            &["query"],
        ),
        tool(
            "references",
            "Definitions and every call site of an exact symbol name across the project. Use before changing a function's signature.",
            json!({"symbol": {"type": "string", "description": "exact symbol name"}}),
            &["symbol"],
        ),
        tool(
            "dependencies",
            "File-level dependency edges resolved from the call map. Without arguments: the whole project graph; with a file: what it depends on and what depends on it.",
            json!({"file": {"type": "string", "description": "relative path (optional)"}}),
            &[],
        ),
        tool(
            "file",
            "The full map entry for one source file: constants, functions (with spans), intra-file call graph, notes, and the rendered .ccc markdown.",
            json!({"path": {"type": "string", "description": "relative path, cache name, or unique path suffix"}}),
            &["path"],
        ),
        tool(
            "notes",
            "All marker comments (TODO/FIXME/XXX/HACK/BUG/NOTE/SAFETY), optionally filtered by marker.",
            json!({"marker": {"type": "string", "description": "e.g. TODO (optional)"}}),
            &[],
        ),
        tool("refresh", "Rescan the source tree into memory. Call after editing source files.", json!({}), &[]),
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
            Orient with `index`, locate symbols with `find`, check `references` before \
            changing a signature, `dependencies` for file-level impact, `file` for one \
            file's full map, `notes` for TODO/FIXME markers. The map auto-refreshes \
            when source files change (three seconds of lag); call `refresh` to force an \
            immediate rescan after editing. Open real source files for exact code - \
            this map is for navigation and impact, not authoritative content.",
    })
}

fn mcp_text(v: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
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
            Ok((before, after)) => Ok(mcp_text(
                &json!({"files_before": before, "files_after": after, "generated": map.ts}),
                false,
            )),
            Err(e) => Ok(mcp_text(&json!({"error": e.to_string()}), true)),
        };
    }

    let map = state.read().expect("map lock poisoned");
    let out: Result<Value, String> = match name {
        "index" => Ok(q_index(&map)),
        "find" => q_find(
            &map,
            &arg("query").unwrap_or_default(),
            arg("kind").as_deref().unwrap_or("any"),
        ),
        "references" => q_references(&map, &arg("symbol").unwrap_or_default()),
        "dependencies" => q_dependencies(&map, arg("file").as_deref()),
        "file" => q_file(&map, &arg("path").unwrap_or_default()),
        "notes" => Ok(q_notes(&map, arg("marker").as_deref())),
        _ => return Err((-32602, format!("unknown tool '{name}'"))),
    };
    Ok(match out {
        Ok(v) => mcp_text(&v, false),
        Err(e) => mcp_text(&json!({"error": e}), true),
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
// opened from file:// (the generated `ccc surf --html` report)
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

// ------------------------------------------------- HTML fragments (HTMX) ----
// tiny Tailwind-styled snippets consumed by the `ccc surf --html` report's
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
            "// Charge an amount.\npub fn charge(c: u64) -> u64 { c }\n\
             // TODO: support currencies\npub fn refund(c: u64) -> u64 { c }\n",
        )
        .unwrap();
        fs::write(
            dir.join("api/main.rs"),
            "fn handle() -> u64 { money::charge(1) + helper() }\nfn helper() -> u64 { 2 }\n",
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
            vec!["index", "find", "references", "dependencies", "file", "notes", "refresh"]
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
        // file:// pages (the generated surf HTML report) send Origin: null
        assert!(origin_ok(Some("null")));
        assert!(!origin_ok(Some("https://evil.example.com")));
        assert!(!origin_ok(Some("http://nullable.example.com")));
    }
}
