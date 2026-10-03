//! `edit_*` - the write half of the map: staged against a handle, previewed as a diff, confined to the root, written all or nothing, revertible

use crate::languages::Language;
use crate::model::FileCache;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tree_sitter::{Node, Parser, Tree};

// result handles kept per server - the oldest is dropped first
const MAX_RESULTS: usize = 64;
// applied changesets kept for `edit_revert`
const MAX_APPLIED: usize = 32;
// diff lines one answer carries before it counts the rest
const DIFF_CAP: usize = 300;
// unchanged lines shown either side of a change
const CONTEXT: usize = 1;
// rows a section lists before it counts the rest
const LIST_CAP: usize = 40;
// edit distance past which a diff is shown as one whole replacement
const MAX_DIFF_D: usize = 1000;
// source text a listed row quotes
const QUOTE_CAP: usize = 100;
// the ledger `edit_apply` appends to under `.ccc/` - `prompts` reads it back
pub const LEDGER_NAME: &str = "edits.jsonl";
// written text kept per file in the ledger
const LEDGER_TEXT_CAP: usize = 2000;
// the only `.ccc` files a person edits by hand - the rest is generated or a ledger
const CCC_WRITABLE: &[&str] = &["map.json", "surface.json"];
// wrappers a call may sit in and still be the whole of its statement
const CALL_WRAPPERS: &[&str] = &["await_expression", "await", "try_expression", "parenthesized_expression"];
// the phrase an applied changeset is reported with - `prompts` reads it out of transcripts
const APPLIED_PHRASE: &str = "applied changeset ";

// one place a `find` or `references` answer pointed at
#[derive(Debug, Clone)]
pub struct Site {
    pub path: String,
    pub line: usize,
    // the identifier as the map indexed it - none for a note
    pub name: Option<String>,
    // func | const | type | call | use | import | reexport | note | site
    pub kind: String,
}

// what a handle stands for
#[derive(Debug, Clone)]
pub struct ResultSet {
    pub id: String,
    // find | references | leftovers | live
    pub origin: String,
    // the part of a name the lookup matched - a `find` substring in any case or the exact `references` name
    pub pattern: String,
    pub exact: bool,
    // a qualifier narrowed the lookup so other same-named sites are not presumed to be the symbol
    pub qualified: bool,
    // the answer was capped so the handle does not hold every site
    pub truncated: bool,
    pub sites: Vec<Site>,
}

impl ResultSet {
    // the sites a `find` answer listed - `pattern` is the unqualified part of its query
    pub fn from_find(v: &Value, pattern: &str, qualified: bool) -> ResultSet {
        ResultSet {
            id: String::new(),
            origin: "find".into(),
            pattern: pattern.to_ascii_lowercase(),
            exact: false,
            qualified,
            truncated: v.get("truncated").and_then(|t| t.as_bool()).unwrap_or(false),
            sites: rows(v, "results", None),
        }
    }

    // the definitions and references a `references` answer listed
    pub fn from_references(v: &Value) -> ResultSet {
        let name = v.get("name").and_then(|n| n.as_str()).unwrap_or_default();
        let mut sites = rows(v, "definitions", Some(name));
        sites.extend(rows(v, "references", Some(name)));
        ResultSet {
            id: String::new(),
            origin: "references".into(),
            pattern: name.to_string(),
            exact: true,
            qualified: v.get("qualifier").is_some_and(|q| q.is_string()),
            truncated: v.get("truncated").and_then(|t| t.as_bool()).unwrap_or(false),
            sites,
        }
    }

    // does an identifier carry what this lookup matched
    fn matches(&self, ident: &str) -> bool {
        if self.exact {
            ident == self.pattern
        } else {
            !self.pattern.is_empty() && ident.to_ascii_lowercase().contains(&self.pattern)
        }
    }

    // the same lookup narrowed to other sites - a follow-up handle inherits how names are rewritten
    fn derived(&self, origin: &str, sites: Vec<Site>) -> ResultSet {
        ResultSet {
            id: String::new(),
            origin: origin.into(),
            pattern: self.pattern.clone(),
            exact: self.exact,
            qualified: self.qualified,
            truncated: false,
            sites,
        }
    }
}

// the rows of one answer array as sites
fn rows(v: &Value, key: &str, name: Option<&str>) -> Vec<Site> {
    v.get(key)
        .and_then(|r| r.as_array())
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let path = r.get("file")?.as_str()?.to_string();
            let line = r.get("line")?.as_u64()? as usize;
            let kind = r.get("kind").and_then(|k| k.as_str()).unwrap_or("site").to_string();
            let name = if kind == "note" {
                None
            } else {
                name.map(str::to_string)
                    .or_else(|| r.get("name").and_then(|n| n.as_str()).map(str::to_string))
            };
            Some(Site { path, line, name, kind })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
struct FileChange {
    // none - the file did not exist when it was first staged
    before: Option<String>,
    // none - the changeset removes the file
    after: Option<String>,
}

// edits staged together and applied together
#[derive(Debug, Clone)]
pub struct Changeset {
    pub id: String,
    ops: Vec<String>,
    files: BTreeMap<String, FileChange>,
    // identifiers renamed old to new - counted again once applied
    renames: BTreeMap<String, String>,
    // why, one terse line per staging call that gave one - for the timeline
    intents: Vec<String>,
    // each staging call's own change, in the order they came - so the
    // timeline replays the changeset the way it was built
    calls: Vec<Activity>,
}

// handles, pending changesets and the recently applied ones, for one server
pub struct Store {
    seq: u64,
    // per process so a changeset id stays unique in the ledger across restarts
    tag: String,
    results: Vec<ResultSet>,
    pending: BTreeMap<String, Changeset>,
    applied: Vec<Changeset>,
}

impl Default for Store {
    fn default() -> Self {
        Store::new()
    }
}

impl Store {
    pub fn new() -> Store {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Store {
            seq: 0,
            tag: format!(
                "{}{}",
                base36(secs % 36u64.pow(5)),
                base36(u64::from(std::process::id()) % 1296)
            ),
            results: Vec::new(),
            pending: BTreeMap::new(),
            applied: Vec::new(),
        }
    }

    // keep a result and hand back its handle
    pub fn record(&mut self, mut r: ResultSet) -> String {
        self.seq += 1;
        r.id = format!("r{}", self.seq);
        let id = r.id.clone();
        self.results.push(r);
        if self.results.len() > MAX_RESULTS {
            self.results.remove(0);
        }
        id
    }

    fn result(&self, id: &str) -> Result<ResultSet, String> {
        self.results
            .iter()
            .find(|r| r.id == id.trim())
            .cloned()
            .ok_or_else(|| {
                format!(
                    "no result handle `{id}` - handles come from `find` and `references`, and the newest \
                     {MAX_RESULTS} are kept; run the lookup again"
                )
            })
    }

    // the changeset to stage into - taken out while it is worked on
    fn open(&mut self, id: Option<&str>) -> Result<Changeset, String> {
        match id.map(str::trim).filter(|i| !i.is_empty()) {
            Some(id) => self.pending.remove(id).ok_or_else(|| {
                if self.applied.iter().any(|c| c.id == id) {
                    format!("changeset `{id}` is already applied - stage into a new one")
                } else {
                    format!("no pending changeset `{id}`")
                }
            }),
            None => {
                self.seq += 1;
                Ok(Changeset {
                    id: format!("c{}-{}", self.seq, self.tag),
                    ops: Vec::new(),
                    files: BTreeMap::new(),
                    renames: BTreeMap::new(),
                    intents: Vec::new(),
                    calls: Vec::new(),
                })
            }
        }
    }
}

// what an edit reads - the project root and the map built from it
pub struct Ctx<'a> {
    pub root: &'a Path,
    pub caches: &'a [FileCache],
}

// how a staging call is carried out
#[derive(Debug, Default)]
pub struct Stage {
    // add to this pending changeset rather than opening a new one
    pub changeset: Option<String>,
    // write it straight away once staged
    pub apply: bool,
    // `path:line` sites to leave untouched
    pub skip: Vec<String>,
    // why, in one terse line - shown beside the step on the visualiser's timeline
    pub intent: Option<String>,
}

// an intent past this is cut - a note beside a step, not a description
const INTENT_MAX: usize = 120;

// what a call produced - the answer, and whether anything reached the disk
#[derive(Debug)]
pub struct Outcome {
    pub text: String,
    pub wrote: bool,
    // what the call did to the code, for the visualiser's timeline
    pub activity: Vec<Activity>,
}

// One step of an edit's life - staged, applied, reverted or discarded - with
// every file it touches and that file's text on each side, so the visualiser
// can show the change as it happens and replay it later.
#[derive(Debug, Clone)]
pub struct Activity {
    pub changeset: String,
    // staged | applied | reverted | discarded - or edited, a change made by
    // hand that the server found on disk
    pub status: &'static str,
    pub ops: Vec<String>,
    pub files: Vec<FileText>,
    // why, as the agent put it when staging - a line per call that said
    pub intent: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FileText {
    pub path: String,
    // none - the file did not exist before the step
    pub before: Option<String>,
    // none - the step removes the file
    pub after: Option<String>,
}

impl Changeset {
    // the files this changeset moves, as one step; a revert runs the change
    // backwards, so its before and after swap
    fn activity(&self, status: &'static str) -> Activity {
        let backwards = status == "reverted";
        Activity {
            changeset: self.id.clone(),
            status,
            ops: self.ops.clone(),
            files: self
                .files
                .iter()
                .filter(|(_, fc)| fc.before != fc.after)
                .map(|(path, fc)| {
                    let (before, after) = if backwards {
                        (fc.after.clone(), fc.before.clone())
                    } else {
                        (fc.before.clone(), fc.after.clone())
                    };
                    FileText { path: path.clone(), before, after }
                })
                .collect(),
            intent: self.intents.clone(),
        }
    }

    // the files this changeset moves, one step per staging call in the order
    // they came; a revert runs them backwards, each with its sides swapped
    fn steps(&self, status: &'static str) -> Vec<Activity> {
        if self.calls.is_empty() {
            return vec![self.activity(status)];
        }
        let mut steps: Vec<Activity> = self.calls.iter().map(|c| Activity { status, ..c.clone() }).collect();
        if status == "reverted" {
            steps.reverse();
            for f in steps.iter_mut().flat_map(|s| s.files.iter_mut()) {
                std::mem::swap(&mut f.before, &mut f.after);
            }
        }
        steps
    }
}

// path safety

// a project-relative path resolved inside the root - no `..`, no symlink on the way, never `.git`, only hand-edited `.ccc` files
pub fn confine(root: &Path, raw: &str) -> Result<(String, PathBuf), String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains('\0') {
        return Err("an empty path names no file".into());
    }
    let p = Path::new(raw);
    let rel = if p.is_absolute() {
        let canon = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        p.strip_prefix(root)
            .or_else(|_| p.strip_prefix(&canon))
            .map_err(|_| format!("`{raw}` is outside the project root - writes stay inside it"))?
            .to_path_buf()
    } else {
        p.to_path_buf()
    };
    let mut clean = PathBuf::new();
    for c in rel.components() {
        match c {
            Component::Normal(s) => clean.push(s),
            Component::CurDir => {}
            _ => return Err(format!("`{raw}` climbs out of the project root - `..` is refused")),
        }
    }
    let rel_str = clean.to_string_lossy().replace('\\', "/");
    if rel_str.is_empty() {
        return Err(format!("`{raw}` names the project root, not a file"));
    }
    let parts: Vec<String> = clean
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.iter().any(|s| s == ".git") {
        return Err(format!("`{rel_str}` is inside `.git` - git's own files are never written"));
    }
    if parts[0] == ".ccc" && !(parts.len() == 2 && CCC_WRITABLE.contains(&parts[1].as_str())) {
        return Err(format!(
            "`{rel_str}` is generated or a ledger - only {} under `.ccc` are edited by hand",
            CCC_WRITABLE.join(" and ")
        ));
    }
    // no symlink between the root and the file, the file included
    let mut at = root.to_path_buf();
    for c in clean.components() {
        at.push(c);
        match fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!(
                    "`{rel_str}` goes through a symlink at `{}` - a write could land outside the project",
                    at.strip_prefix(root).unwrap_or(&at).display()
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(format!("`{rel_str}`: {e}")),
        }
    }
    let abs = root.join(&clean);
    if abs.is_dir() {
        return Err(format!("`{rel_str}` is a directory, not a file"));
    }
    Ok((rel_str, abs))
}

// file text

// a file as this changeset would leave it - staged text first, then the disk
fn text_of(root: &Path, cs: &Changeset, rel: &str) -> Result<Option<String>, String> {
    if let Some(fc) = cs.files.get(rel) {
        return Ok(fc.after.clone());
    }
    let (_, abs) = confine(root, rel)?;
    read_opt(&abs).map_err(|e| format!("reading {rel}: {e}"))
}

fn read_opt(abs: &Path) -> std::io::Result<Option<String>> {
    match fs::read_to_string(abs) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

// record a file's new text, keeping what was on disk when it was first staged
fn put(root: &Path, cs: &mut Changeset, rel: &str, after: Option<String>) -> Result<(), String> {
    if let Some(fc) = cs.files.get_mut(rel) {
        fc.after = after;
        return Ok(());
    }
    let (_, abs) = confine(root, rel)?;
    let before = read_opt(&abs).map_err(|e| format!("reading {rel}: {e}"))?;
    cs.files.insert(rel.to_string(), FileChange { before, after });
    Ok(())
}

// sites carry the line the map saw on disk, so a file this changeset already reshaped cannot take another
fn lines_stable(cs: &Changeset, rel: &str) -> Result<(), String> {
    if let Some(fc) = cs.files.get(rel) {
        let n = |t: &Option<String>| t.as_deref().map(|t| t.lines().count());
        if n(&fc.before) != n(&fc.after) {
            return Err(format!(
                "{rel} already gains or loses lines in changeset `{}` - apply it, look the sites up again, then stage this",
                cs.id
            ));
        }
    }
    Ok(())
}

// syntax

fn parse(lang: Language, src: &str) -> Option<Tree> {
    let mut p = Parser::new();
    p.set_language(&lang.ts_language()).ok()?;
    p.parse(src, None)
}

fn parsed(rel: &str, text: &str) -> Result<(Language, Tree), String> {
    let lang = Language::from_path(Path::new(rel))
        .ok_or_else(|| format!("{rel} is not a language the map parses - use `edit_text` for it"))?;
    let tree = parse(lang, text).ok_or_else(|| format!("{rel} could not be parsed"))?;
    Ok((lang, tree))
}

// one identifier token in a parsed file
#[derive(Debug, Clone)]
struct Tok {
    start: usize,
    end: usize,
    line: usize,
    text: String,
    // on the path side of `a::b` - a module or type, never a function or value
    qualifier: bool,
}

fn is_ident(kind: &str) -> bool {
    kind == "identifier" || kind.ends_with("_identifier")
}

// every identifier leaf the tree holds whose text `want` accepts
fn idents(tree: &Tree, src: &str, want: &dyn Fn(&str) -> bool) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if n.child_count() == 0 && n.is_named() && is_ident(n.kind()) {
            let text = &src[n.byte_range()];
            if want(text) {
                out.push(Tok {
                    start: n.start_byte(),
                    end: n.end_byte(),
                    line: n.start_position().row + 1,
                    text: text.to_string(),
                    qualifier: in_qualifier(n),
                });
            }
        }
        let mut c = n.walk();
        stack.extend(n.children(&mut c));
    }
    out.sort_by_key(|t| t.start);
    out
}

// a token on the path side of `a::b` - climbs through `a::b::c` so `b` counts too
fn in_qualifier(n: Node) -> bool {
    let mut cur = n;
    while let Some(p) = cur.parent() {
        let field = match p.kind() {
            "scoped_identifier" | "scoped_type_identifier" | "scoped_use_list" => "path",
            "qualified_identifier" => "scope",
            "qualified_name" => "qualifier",
            _ => return false,
        };
        if p.child_by_field_name(field).is_some_and(|c| c.id() == cur.id()) {
            return true;
        }
        cur = p;
    }
    false
}

fn is_texty(lang: Language, kind: &str) -> bool {
    lang.comment_kinds().contains(&kind) || kind.contains("string") || kind == "char_literal"
}

// comments and strings that spell one of `names` as a word - prose and literals an edit leaves alone
fn mentions(tree: &Tree, src: &str, lang: Language, names: &BTreeSet<String>) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if is_texty(lang, n.kind()) {
            let text = &src[n.byte_range()];
            for name in names {
                if let Some(off) = word_at(text, name) {
                    let line = src[..n.start_byte() + off].matches('\n').count() + 1;
                    out.push((line, name.clone()));
                }
            }
            continue;
        }
        let mut c = n.walk();
        stack.extend(n.children(&mut c));
    }
    out.sort();
    out.dedup();
    out
}

// where `word` first sits in `text` with no identifier character either side
fn word_at(text: &str, word: &str) -> Option<usize> {
    let is_id = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut from = 0;
    while let Some(off) = text[from..].find(word) {
        let at = from + off;
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        if !before.is_some_and(is_id) && !after.is_some_and(is_id) {
            return Some(at);
        }
        from = at + word.len().max(1);
    }
    None
}

// the identifiers a site stands for on its own line - its indexed name, else whatever the lookup matched
fn site_toks<'t>(toks: &'t [Tok], site: &Site, rs: &ResultSet) -> Vec<&'t Tok> {
    let on: Vec<&Tok> = toks.iter().filter(|t| t.line == site.line).collect();
    let named: Vec<&Tok> = on
        .iter()
        .copied()
        .filter(|t| site.name.as_deref() == Some(t.text.as_str()))
        .collect();
    if !named.is_empty() {
        return named;
    }
    on.into_iter().filter(|t| rs.matches(&t.text)).collect()
}

// node kinds that define something a site can name
fn def_kinds(lang: Language) -> Vec<&'static str> {
    let mut k: Vec<&'static str> = lang.func_kinds().to_vec();
    k.extend(lang.const_kinds());
    k.extend(lang.type_kinds().iter().map(|(kind, _)| *kind));
    k.extend(lang.variant_kinds());
    k
}

// the definition a site names, widened to what carries it - an export, decorators, a lone spec or statement
fn definition<'t>(tree: &'t Tree, src: &str, lang: Language, site: &Site) -> Option<Node<'t>> {
    let name = site.name.as_deref()?;
    let kinds = def_kinds(lang);
    for t in idents(tree, src, &|t| t == name).iter().filter(|t| t.line == site.line) {
        let leaf = tree.root_node().descendant_for_byte_range(t.start, t.end)?;
        let mut cur = leaf.parent();
        let mut found = None;
        while let Some(p) = cur {
            if kinds.contains(&p.kind()) {
                found = Some(p);
                break;
            }
            cur = p.parent();
        }
        let Some(mut d) = found else {
            continue;
        };
        while let Some(p) = d.parent() {
            let k = p.kind();
            let lone = p.named_child_count() == 1;
            let wraps = match k {
                "lexical_declaration" | "variable_declaration" => lone,
                "expression_statement" | "type_declaration" | "const_declaration" | "var_declaration" => lone,
                _ => lang.doc_wrapper_kinds().contains(&k),
            };
            if !wraps {
                break;
            }
            d = p;
        }
        return Some(d);
    }
    None
}

// the last row a node occupies - a node that ends at column 0 ended on the row before
fn end_row(n: Node) -> usize {
    let e = n.end_position();
    if e.column == 0 && e.row > n.start_position().row {
        e.row - 1
    } else {
        e.row
    }
}

// first row of a definition counting the doc comments and attributes stacked directly on it
fn lead_row(node: Node, src: &str, lang: Language) -> usize {
    let mut row = node.start_position().row;
    let mut s = node.prev_sibling();
    while let Some(p) = s {
        let k = p.kind();
        let attaches = lang.comment_kinds().contains(&k) || lang.annotation_kinds().contains(&k);
        if !attaches || end_row(p) + 1 != row || !starts_line(src, p.start_byte()) {
            break;
        }
        row = p.start_position().row;
        s = p.prev_sibling();
    }
    row
}

// the first node of one of `kinds` that starts on `row` (0-based)
fn kind_on_row<'t>(tree: &'t Tree, row: usize, kinds: &[&str]) -> Option<Node<'t>> {
    let mut stack = vec![tree.root_node()];
    let mut best: Option<Node<'t>> = None;
    while let Some(n) = stack.pop() {
        if n.start_position().row > row || n.end_position().row < row {
            continue;
        }
        if n.start_position().row == row && kinds.contains(&n.kind()) {
            if best.map_or(true, |b| n.start_byte() < b.start_byte()) {
                best = Some(n);
            }
            continue;
        }
        let mut c = n.walk();
        stack.extend(n.children(&mut c));
    }
    best
}

// the statement a call is the whole of - `charge(1);`, `await send(x)`, `close()?;`
fn call_statement<'t>(tree: &'t Tree, src: &str, lang: Language, site: &Site) -> Result<Node<'t>, String> {
    let name = site.name.as_deref().ok_or("the site names no call")?;
    let calls = lang.call_kinds();
    for t in idents(tree, src, &|t| t == name).iter().filter(|t| t.line == site.line) {
        let Some(leaf) = tree.root_node().descendant_for_byte_range(t.start, t.end) else {
            continue;
        };
        let mut cur = leaf.parent();
        let mut call = None;
        while let Some(p) = cur {
            if calls.contains(&p.kind()) {
                call = Some(p);
                break;
            }
            cur = p.parent();
        }
        let Some(mut c) = call else {
            continue;
        };
        loop {
            let Some(p) = c.parent() else {
                break;
            };
            if p.kind() == "expression_statement" {
                return Ok(p);
            }
            if !CALL_WRAPPERS.contains(&p.kind()) {
                return Err("its value is used by the code around it - `edit_replace` target=line instead".into());
            }
            c = p;
        }
    }
    Err("no call to it on that line".into())
}

// the import statement a site sits in, if removing it takes no other name with it
fn import_statement<'t>(tree: &'t Tree, src: &str, lang: Language, site: &Site) -> Result<Node<'t>, String> {
    let row = site.line - 1;
    if lang == Language::Go {
        if let Some(spec) = kind_on_row(tree, row, &["import_spec"]) {
            return Ok(spec);
        }
    }
    let node = kind_on_row(tree, row, lang.import_kinds()).ok_or("no import statement starts on that line")?;
    let text = &src[node.byte_range()];
    if text.contains('{') || text.contains(',') {
        return Err("the import binds other names too - `edit_text` the one name out of it".into());
    }
    Ok(node)
}

// line arithmetic

fn line_start(src: &str, byte: usize) -> usize {
    src[..byte].rfind('\n').map_or(0, |i| i + 1)
}

// the byte after the newline that ends the line `byte` is on, or the end of the text
fn line_end(src: &str, byte: usize) -> usize {
    src[byte..].find('\n').map_or(src.len(), |i| byte + i + 1)
}

fn row_start(src: &str, row: usize) -> usize {
    if row == 0 {
        return 0;
    }
    src.match_indices('\n').nth(row - 1).map_or(src.len(), |(i, _)| i + 1)
}

fn starts_line(src: &str, byte: usize) -> bool {
    src[line_start(src, byte)..byte].trim().is_empty()
}

fn indent_at(src: &str, byte: usize) -> String {
    src[line_start(src, byte)..]
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

fn row_text(src: &str, row: usize) -> &str {
    let s = row_start(src, row);
    let e = line_end(src, s);
    src[s..e].trim_end_matches(['\n', '\r'])
}

// one source line quoted for a listed row
fn quote(src: &str, line: usize) -> String {
    let t = row_text(src, line.saturating_sub(1)).trim();
    if t.chars().count() > QUOTE_CAP {
        format!("{}…", t.chars().take(QUOTE_CAP).collect::<String>())
    } else {
        t.to_string()
    }
}

// the bytes a removal takes out - whole lines when the node owns them, else the node and the separator after it
fn removal(src: &str, start: usize, end: usize) -> (usize, usize) {
    let eol = src[end..].find('\n').map_or(src.len(), |i| end + i);
    let tail = src[end..eol].trim();
    if starts_line(src, start) && matches!(tail, "" | "," | ";") {
        let a = line_start(src, start);
        let mut b = (eol + 1).min(src.len());
        // close the gap rather than leave two blank lines where the node was
        let blank_before = a == 0 || src[line_start(src, a - 1)..a].trim().is_empty();
        let next = &src[b..line_end(src, b)];
        if blank_before && b < src.len() && next.trim().is_empty() {
            b = line_end(src, b);
        }
        return (a, b);
    }
    let rest = &src[end..];
    let ws = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    let mut b = end;
    if rest[ws..].starts_with([',', ';']) {
        b = end + ws + 1;
        let after = &src[b..];
        b += after.len() - after.trim_start_matches([' ', '\t']).len();
    }
    (start, b)
}

// `text` laid at `indent` unless it brings its own
fn reindent(text: &str, indent: &str) -> String {
    let body = text.replace("\r\n", "\n");
    let body = body.trim_end_matches('\n');
    let own = body
        .lines()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| l.starts_with([' ', '\t']));
    if own || indent.is_empty() {
        return body.to_string();
    }
    body.lines()
        .map(|l| if l.trim().is_empty() { String::new() } else { format!("{indent}{l}") })
        .collect::<Vec<_>>()
        .join("\n")
}

// new text in the line endings the file already uses
fn in_style(file: &str, text: String) -> String {
    if file.contains("\r\n") {
        text.replace("\r\n", "\n").replace('\n', "\r\n")
    } else {
        text
    }
}

// one byte range of a file and what replaces it
type Splice = (usize, usize, String);

// `edits` applied to `text` - they must not overlap
fn splice(rel: &str, text: &str, edits: &mut Vec<Splice>) -> Result<String, String> {
    edits.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    edits.dedup();
    for w in edits.windows(2) {
        if w[1].0 < w[0].1 {
            return Err(format!("two edits overlap in {rel} - stage them separately"));
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (s, e, r) in edits.iter() {
        out.push_str(&text[at..*s]);
        out.push_str(r);
        at = *e;
    }
    out.push_str(&text[at..]);
    Ok(out)
}

// names

// an identifier in every language the map reads
fn valid_ident(s: &str) -> bool {
    let mut cs = s.chars();
    match cs.next() {
        Some(c) if c.is_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    cs.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

// `pattern` replaced inside `ident`, each match keeping its case shape - hello_all, Hello, HELLO
fn substitute(ident: &str, pattern: &str, to: &str) -> String {
    if pattern.is_empty() {
        return ident.to_string();
    }
    let lower = ident.to_ascii_lowercase();
    let mut out = String::new();
    let mut i = 0;
    while let Some(off) = lower[i..].find(pattern) {
        let at = i + off;
        out.push_str(&ident[i..at]);
        out.push_str(&shaped(&ident[at..at + pattern.len()], to));
        i = at + pattern.len();
    }
    out.push_str(&ident[i..]);
    out
}

fn shaped(matched: &str, to: &str) -> String {
    let letters: Vec<char> = matched.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.len() > 1 && letters.iter().all(|c| c.is_uppercase()) {
        return to.to_uppercase();
    }
    if letters.first().is_some_and(|c| c.is_uppercase()) {
        let mut cs = to.chars();
        return cs
            .next()
            .map(|c| c.to_uppercase().collect::<String>() + cs.as_str())
            .unwrap_or_default();
    }
    to.to_string()
}

fn path_of(c: &FileCache) -> String {
    c.rel_path.to_string_lossy().replace('\\', "/")
}

// where the map defines `name` - path, line, kind
fn definitions(caches: &[FileCache], name: &str) -> Vec<(String, usize, &'static str)> {
    let mut out = Vec::new();
    for c in caches {
        let p = path_of(c);
        out.extend(c.funcs.iter().filter(|f| f.name == name).map(|f| (p.clone(), f.line, "func")));
        out.extend(c.consts.iter().filter(|k| k.name == name).map(|k| (p.clone(), k.line, "const")));
        out.extend(c.types.iter().filter(|t| t.name == name).map(|t| (p.clone(), t.line, "type")));
    }
    out
}

// files the map ties to `name` - a definition, call, usage or import of it
fn tied(caches: &[FileCache], name: &str) -> BTreeSet<String> {
    caches
        .iter()
        .filter(|c| {
            c.funcs.iter().any(|f| f.name == name)
                || c.consts.iter().any(|k| k.name == name)
                || c.types.iter().any(|t| t.name == name)
                || c.calls.iter().chain(&c.uses).chain(&c.constructs).any(|s| s.name == name)
                || c.imports.iter().any(|i| {
                    i.names.iter().any(|n| n == name)
                        || i.module.rsplit([':', '.', '/']).next() == Some(name)
                })
        })
        .map(path_of)
        .collect()
}

// what the map calls the site of `name` on a line - so a follow-up handle edits it the right way
fn kind_at(caches: &[FileCache], path: &str, line: usize, name: &str) -> &'static str {
    let Some(c) = caches.iter().find(|c| path_of(c) == path) else {
        return "site";
    };
    if c.funcs.iter().any(|f| f.line == line && f.name == name) {
        "func"
    } else if c.consts.iter().any(|k| k.line == line && k.name == name) {
        "const"
    } else if c.types.iter().any(|t| t.line == line && t.name == name) {
        "type"
    } else if c.calls.iter().any(|s| s.line == line && s.name == name) {
        "call"
    } else if c.constructs.iter().any(|s| s.line == line && s.name == name) {
        "construct"
    } else if c.imports.iter().any(|i| i.line == line) {
        "import"
    } else {
        "use"
    }
}

fn parse_skip(skip: &[String]) -> Result<BTreeSet<(String, usize)>, String> {
    skip.iter()
        .map(|s| {
            let (p, l) = s
                .trim()
                .rsplit_once(':')
                .ok_or_else(|| format!("skip entry `{s}` is not `path:line`"))?;
            let line = l
                .parse::<usize>()
                .map_err(|_| format!("skip entry `{s}` is not `path:line`"))?;
            Ok((p.trim_start_matches("./").to_string(), line))
        })
        .collect()
}

fn by_path(sites: &[Site]) -> BTreeMap<String, Vec<&Site>> {
    let mut out: BTreeMap<String, Vec<&Site>> = BTreeMap::new();
    for s in sites {
        out.entry(s.path.clone()).or_default().push(s);
    }
    out
}

// the project sweep

// one identifier carrying a swept name
#[derive(Debug, Clone)]
struct Hit {
    path: String,
    line: usize,
    start: usize,
    end: usize,
    text: String,
    qualifier: bool,
    quote: String,
}

#[derive(Default)]
struct Sweep {
    hits: Vec<Hit>,
    mentions: Vec<(String, usize, String, String)>,
    texts: BTreeMap<String, String>,
}

// every source file under the root as this changeset leaves it
fn project_files(root: &Path, cs: &Changeset) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = crate::scan::collect_files(root)
        .unwrap_or_default()
        .iter()
        .filter_map(|p| p.strip_prefix(root).ok())
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    for (rel, fc) in &cs.files {
        match fc.after {
            Some(_) if Language::from_path(Path::new(rel)).is_some() => {
                out.insert(rel.clone());
            }
            None => {
                out.remove(rel);
            }
            _ => {}
        }
    }
    out
}

// every identifier in the project spelled as one of `names`, plus the comments and strings that spell them
fn sweep(root: &Path, cs: &Changeset, names: &BTreeSet<String>) -> Sweep {
    let mut out = Sweep::default();
    for rel in project_files(root, cs) {
        let text = match cs.files.get(&rel) {
            Some(fc) => fc.after.clone(),
            None => fs::read_to_string(root.join(&rel)).ok(),
        };
        let Some(text) = text else {
            continue;
        };
        if !names.iter().any(|n| text.contains(n.as_str())) {
            continue;
        }
        let Ok((lang, tree)) = parsed(&rel, &text) else {
            continue;
        };
        for t in idents(&tree, &text, &|t| names.contains(t)) {
            out.hits.push(Hit {
                path: rel.clone(),
                line: t.line,
                start: t.start,
                end: t.end,
                quote: quote(&text, t.line),
                text: t.text,
                qualifier: t.qualifier,
            });
        }
        for (line, name) in mentions(&tree, &text, lang, names) {
            out.mentions.push((rel.clone(), line, name, quote(&text, line)));
        }
        out.texts.insert(rel, text);
    }
    out
}

// staging

// run one staging op against a changeset, which stays as it was if the op fails, then apply when asked
fn staged(
    ctx: &Ctx,
    store: &mut Store,
    stage: &Stage,
    op: impl FnOnce(&Ctx, &mut Store, &mut Changeset) -> Result<String, String>,
) -> Result<Outcome, String> {
    let base = store.open(stage.changeset.as_deref())?;
    let mut cs = base.clone();
    let report = match op(ctx, store, &mut cs) {
        Ok(r) => r,
        Err(e) => {
            if stage.changeset.is_some() {
                store.pending.insert(base.id.clone(), base);
            }
            return Err(e);
        }
    };
    // its first line, kept short
    let why: Option<String> = stage
        .intent
        .as_deref()
        .and_then(|i| i.lines().next())
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .map(|w| w.chars().take(INTENT_MAX).collect());
    // the same reason given call after call is one reason
    if let Some(w) = why.clone().filter(|w| cs.intents.last() != Some(w)) {
        cs.intents.push(w);
    }
    // this call's own change, from the text it found to the text it left
    let files: Vec<FileText> = cs
        .files
        .iter()
        .filter_map(|(rel, fc)| {
            let found = base.files.get(rel).map_or(&fc.before, |b| &b.after);
            (*found != fc.after).then(|| FileText { path: rel.clone(), before: found.clone(), after: fc.after.clone() })
        })
        .collect();
    let activity: Vec<Activity> = if files.is_empty() {
        Vec::new()
    } else {
        let call = Activity {
            changeset: cs.id.clone(),
            status: "staged",
            ops: cs.ops.get(base.ops.len()..).unwrap_or_default().to_vec(),
            files,
            intent: why.into_iter().collect(),
        };
        cs.calls.push(call.clone());
        vec![call]
    };
    let id = cs.id.clone();
    let mut text = format!("{report}\n{}", preview(&cs));
    store.pending.insert(id.clone(), cs);
    if stage.apply {
        let done = apply(ctx.root, store, &id)?;
        text.push('\n');
        text.push_str(&done.text);
        return Ok(Outcome { text, wrote: true, activity: done.activity });
    }
    text.push_str(&format!(
        "\nnext: `edit_apply` changeset=\"{id}\" to write it, `edit_discard` to drop it, or pass \
         changeset=\"{id}\" to another edit tool to stage more into it\n"
    ));
    Ok(Outcome { text, wrote: false, activity })
}

// a listed section, capped
fn section(out: &mut String, title: &str, rows: &[String]) {
    if rows.is_empty() {
        return;
    }
    out.push_str(&format!("\n## {title} ({})\n", rows.len()));
    for r in rows.iter().take(LIST_CAP) {
        out.push_str(r);
        out.push('\n');
    }
    if rows.len() > LIST_CAP {
        out.push_str(&format!("… and {} more\n", rows.len() - LIST_CAP));
    }
}

// renames every identifier a handle points at, then follows each name to the rest of its symbol
pub fn rename(ctx: &Ctx, store: &mut Store, result: &str, to: &str, stage: &Stage) -> Result<Outcome, String> {
    let rs = store.result(result)?;
    usable(&rs)?;
    let to = to.trim();
    if to.is_empty() {
        return Err("`to` is empty".into());
    }
    if rs.exact && !valid_ident(to) {
        return Err(format!("`{to}` is not an identifier"));
    }
    let skip = parse_skip(&stage.skip)?;
    staged(ctx, store, stage, |ctx, store, cs| rename_into(ctx, store, cs, &rs, to, &skip))
}

fn usable(rs: &ResultSet) -> Result<(), String> {
    if rs.truncated {
        return Err(format!(
            "handle `{}` was capped and does not hold every site - narrow the lookup (`kind`, a qualifier, a longer name) and use the new handle",
            rs.id
        ));
    }
    if rs.sites.is_empty() {
        return Err(format!("handle `{}` holds no sites", rs.id));
    }
    Ok(())
}

fn rename_into(
    ctx: &Ctx,
    store: &mut Store,
    cs: &mut Changeset,
    rs: &ResultSet,
    to: &str,
    skip: &BTreeSet<(String, usize)>,
) -> Result<String, String> {
    // what each site's identifier becomes
    let mut renames: BTreeMap<String, String> = BTreeMap::new();
    let mut site_lines: BTreeSet<(String, usize)> = BTreeSet::new();
    let mut unmatched: Vec<String> = Vec::new();
    for (path, sites) in by_path(&rs.sites) {
        let sites: Vec<&Site> = sites
            .into_iter()
            .filter(|s| s.kind != "note" && !skip.contains(&(s.path.clone(), s.line)))
            .collect();
        if sites.is_empty() {
            continue;
        }
        lines_stable(cs, &path)?;
        let Some(text) = text_of(ctx.root, cs, &path)? else {
            unmatched.push(format!("{path} - the file no longer exists"));
            continue;
        };
        let (_, tree) = parsed(&path, &text)?;
        let toks = idents(&tree, &text, &|t| {
            rs.matches(t) || sites.iter().any(|s| s.name.as_deref() == Some(t))
        });
        for s in &sites {
            let found = site_toks(&toks, s, rs);
            if found.is_empty() {
                unmatched.push(format!("{path}:{} `{}`", s.line, quote(&text, s.line)));
                continue;
            }
            site_lines.insert((path.clone(), s.line));
            for t in found {
                let new = if rs.exact { to.to_string() } else { substitute(&t.text, &rs.pattern, to) };
                renames.insert(t.text.clone(), new);
            }
        }
    }
    renames.retain(|old, new| old != new);
    if renames.is_empty() {
        return Err(format!(
            "nothing to rename - no site of `{}` carries an identifier that `{to}` changes",
            rs.id
        ));
    }
    if let Some(bad) = renames.values().find(|n| !valid_ident(n)) {
        return Err(format!("`{bad}` is not an identifier"));
    }

    // per name, whether it is safe to follow it through the files the map ties to it
    let mut follow: BTreeMap<String, (BTreeSet<String>, bool)> = BTreeMap::new();
    let mut why_not: BTreeMap<String, String> = BTreeMap::new();
    for old in renames.keys() {
        let defs = definitions(ctx.caches, old);
        if rs.qualified {
            why_not.insert(old.clone(), "the lookup was qualified, so this may be another symbol".into());
        } else if defs.len() > 1 {
            why_not.insert(old.clone(), format!("{} definitions share the name", defs.len()));
        } else if defs.is_empty() {
            why_not.insert(old.clone(), "the map holds no definition of it".into());
        } else {
            follow.insert(old.clone(), (tied(ctx.caches, old), defs[0].2 == "type"));
        }
    }

    // every identifier carrying an old name, changed or left alone
    let names: BTreeSet<String> = renames.keys().cloned().collect();
    let sw = sweep(ctx.root, cs, &names);
    let mut edits: BTreeMap<String, Vec<(usize, usize, String)>> = BTreeMap::new();
    let mut left: Vec<(Hit, String)> = Vec::new();
    let (mut named, mut followed) = (0usize, 0usize);
    for h in sw.hits {
        let at = (h.path.clone(), h.line);
        let reason = if skip.contains(&at) {
            Some("skipped as asked".to_string())
        } else if site_lines.contains(&at) {
            None
        } else {
            match follow.get(&h.text) {
                Some((files, _)) if !files.contains(&h.path) => {
                    Some("the map ties nothing in this file to it".into())
                }
                Some((_, is_type)) if h.qualifier && !is_type => {
                    Some("names a module or type here, not the symbol".into())
                }
                Some(_) => None,
                None => why_not.get(&h.text).cloned(),
            }
        };
        match reason {
            None => {
                if site_lines.contains(&at) {
                    named += 1;
                } else {
                    followed += 1;
                }
                edits
                    .entry(h.path.clone())
                    .or_default()
                    .push((h.start, h.end, renames[&h.text].clone()));
            }
            Some(r) => left.push((h, r)),
        }
    }
    if edits.is_empty() {
        return Err(format!("nothing to rename - every `{}` site was skipped", rs.id));
    }
    let files = edits.len();
    for (path, mut list) in edits {
        let text = sw.texts.get(&path).cloned().unwrap_or_default();
        let new = splice(&path, &text, &mut list)?;
        put(ctx.root, cs, &path, Some(new))?;
    }
    let pairs: Vec<String> = renames.iter().map(|(o, n)| format!("`{o}` -> `{n}`")).collect();
    cs.ops.push(format!("rename {} ({})", pairs.join(", "), rs.id));
    cs.renames.extend(renames.clone());

    let mut out = format!(
        "# edit_rename {} -> changeset `{}` (staged, nothing written yet)\n{}\n{} identifier(s) in {files} file(s): {named} on the handle's own lines, {followed} followed through files the map ties to the symbol\n",
        rs.id,
        cs.id,
        pairs.join(", "),
        named + followed,
    );
    for new in renames.values() {
        for (p, l, k) in definitions(ctx.caches, new) {
            out.push_str(&format!("warning: `{new}` is already a {k} at {p}:{l} - the rename collides with it\n"));
        }
    }
    section(&mut out, "sites with no matching identifier", &unmatched);
    if !left.is_empty() {
        let sites: Vec<Site> = left
            .iter()
            .map(|(h, _)| Site {
                path: h.path.clone(),
                line: h.line,
                name: Some(h.text.clone()),
                kind: kind_at(ctx.caches, &h.path, h.line, &h.text).into(),
            })
            .collect();
        let handle = store.record(rs.derived("leftovers", sites));
        out.push_str(&format!(
            "\n## left alone - same name, not changed ({})\nhandle `{handle}` holds them: `edit_rename` result=\"{handle}\" to=\"{to}\" changeset=\"{}\" changes them too\n",
            left.len(),
            cs.id
        ));
        for (h, why) in left.iter().take(LIST_CAP) {
            out.push_str(&format!("{}:{} `{}` - {why}\n", h.path, h.line, h.quote));
        }
        if left.len() > LIST_CAP {
            out.push_str(&format!("… and {} more\n", left.len() - LIST_CAP));
        }
    }
    let spelled: Vec<String> = sw
        .mentions
        .iter()
        .map(|(p, l, n, q)| format!("{p}:{l} `{q}` - spells `{n}`"))
        .collect();
    section(&mut out, "comments and strings that spell an old name - not changed", &spelled);
    Ok(out)
}

// replaces the token, line or definition each site of a handle names with `text`
pub fn replace(
    ctx: &Ctx,
    store: &mut Store,
    result: &str,
    text: &str,
    target: &str,
    stage: &Stage,
) -> Result<Outcome, String> {
    let rs = store.result(result)?;
    usable(&rs)?;
    if !matches!(target, "token" | "line" | "definition") {
        return Err(format!("target `{target}` is not one of token | line | definition"));
    }
    let skip = parse_skip(&stage.skip)?;
    staged(ctx, store, stage, |ctx, _, cs| {
        let mut count = 0;
        let mut refused: Vec<String> = Vec::new();
        for (path, sites) in by_path(&rs.sites) {
            let sites: Vec<&Site> = sites
                .into_iter()
                .filter(|s| !skip.contains(&(s.path.clone(), s.line)))
                .collect();
            if sites.is_empty() {
                continue;
            }
            lines_stable(cs, &path)?;
            let Some(src) = text_of(ctx.root, cs, &path)? else {
                refused.push(format!("{path} - the file no longer exists"));
                continue;
            };
            let mut edits: Vec<(usize, usize, String)> = Vec::new();
            match target {
                "line" => {
                    for s in &sites {
                        let a = row_start(&src, s.line - 1);
                        let b = line_end(&src, a);
                        let nl = if src[a..b].ends_with('\n') { "\n" } else { "" };
                        edits.push((a, b, in_style(&src, format!("{}{nl}", reindent(text, &indent_at(&src, a))))));
                    }
                }
                _ => {
                    let (lang, tree) = parsed(&path, &src)?;
                    let toks = idents(&tree, &src, &|t| {
                        rs.matches(t) || sites.iter().any(|s| s.name.as_deref() == Some(t))
                    });
                    for s in &sites {
                        if target == "token" {
                            let found = site_toks(&toks, s, &rs);
                            if found.is_empty() {
                                refused.push(format!("{path}:{} `{}` - no matching identifier", s.line, quote(&src, s.line)));
                            }
                            edits.extend(found.into_iter().map(|t| (t.start, t.end, text.to_string())));
                            continue;
                        }
                        let Some(node) = definition(&tree, &src, lang, s) else {
                            refused.push(format!("{path}:{} `{}` - not a definition", s.line, quote(&src, s.line)));
                            continue;
                        };
                        let (start, end) = (node.start_byte(), node.end_byte());
                        let eol = src[end..].find('\n').map_or(src.len(), |i| end + i);
                        if starts_line(&src, start) && src[end..eol].trim().is_empty() {
                            let a = line_start(&src, start);
                            edits.push((a, eol, in_style(&src, reindent(text, &indent_at(&src, a)))));
                        } else {
                            edits.push((start, end, text.to_string()));
                        }
                    }
                }
            }
            if edits.is_empty() {
                continue;
            }
            count += edits.len();
            let new = splice(&path, &src, &mut edits)?;
            put(ctx.root, cs, &path, Some(new))?;
        }
        if count == 0 {
            let mut e = format!("nothing replaced through `{}`", rs.id);
            for r in &refused {
                e.push_str(&format!("\n{r}"));
            }
            return Err(e);
        }
        cs.ops.push(format!("replace {target} ({})", rs.id));
        let mut out = format!(
            "# edit_replace {} target={target} -> changeset `{}` (staged, nothing written yet)\n{count} replacement(s)\n",
            rs.id, cs.id
        );
        section(&mut out, "sites left alone", &refused);
        Ok(out)
    })
}

// removes what each site of a handle names, refusing while anything still names a removed definition
pub fn delete(ctx: &Ctx, store: &mut Store, result: &str, force: bool, stage: &Stage) -> Result<Outcome, String> {
    let rs = store.result(result)?;
    usable(&rs)?;
    let skip = parse_skip(&stage.skip)?;
    staged(ctx, store, stage, |ctx, store, cs| {
        // path -> the text the ranges were measured on, and the ranges
        let mut planned: BTreeMap<String, (String, Vec<Splice>)> = BTreeMap::new();
        let mut gone: BTreeSet<String> = BTreeSet::new();
        let mut refused: Vec<String> = Vec::new();
        let mut removed = 0;
        for (path, sites) in by_path(&rs.sites) {
            let sites: Vec<&Site> = sites
                .into_iter()
                .filter(|s| !skip.contains(&(s.path.clone(), s.line)))
                .collect();
            if sites.is_empty() {
                continue;
            }
            lines_stable(cs, &path)?;
            let Some(src) = text_of(ctx.root, cs, &path)? else {
                refused.push(format!("{path} - the file no longer exists"));
                continue;
            };
            let (lang, tree) = parsed(&path, &src)?;
            let mut ranges: Vec<(usize, usize, String)> = Vec::new();
            for s in &sites {
                let why = |e: String| format!("{path}:{} `{}` - {e}", s.line, quote(&src, s.line));
                let range = match s.kind.as_str() {
                    "func" | "const" | "type" => match definition(&tree, &src, lang, s) {
                        Some(node) => {
                            if let Some(n) = &s.name {
                                gone.insert(n.clone());
                            }
                            let lead = row_start(&src, lead_row(node, &src, lang));
                            Ok(removal(&src, lead.min(node.start_byte()), node.end_byte()))
                        }
                        None => Err("no definition starts there".to_string()),
                    },
                    "call" => call_statement(&tree, &src, lang, s).map(|n| removal(&src, n.start_byte(), n.end_byte())),
                    "import" | "reexport" => {
                        import_statement(&tree, &src, lang, s).map(|n| removal(&src, n.start_byte(), n.end_byte()))
                    }
                    "note" => kind_on_row(&tree, s.line - 1, lang.comment_kinds())
                        .map(|n| {
                            if starts_line(&src, n.start_byte()) {
                                removal(&src, n.start_byte(), n.end_byte())
                            } else {
                                // a trailing comment - take it and the spaces before it, keep the code
                                let before = src[..n.start_byte()].trim_end_matches([' ', '\t']).len();
                                (before, n.end_byte())
                            }
                        })
                        .ok_or_else(|| "no comment starts on that line".to_string()),
                    _ => Err("a usage inside an expression - `edit_replace` target=line instead".to_string()),
                };
                match range {
                    Ok((a, b)) => ranges.push((a, b, String::new())),
                    Err(e) => refused.push(why(e)),
                }
            }
            // a definition nested in another removed one goes with it
            ranges.sort_by(|a, b| (a.0, std::cmp::Reverse(a.1)).cmp(&(b.0, std::cmp::Reverse(b.1))));
            let mut kept: Vec<(usize, usize, String)> = Vec::new();
            for r in ranges {
                match kept.last_mut() {
                    Some(last) if r.0 < last.1 => last.1 = last.1.max(r.1),
                    _ => kept.push(r),
                }
            }
            removed += kept.len();
            if !kept.is_empty() {
                planned.insert(path, (src, kept));
            }
        }
        if planned.is_empty() {
            let mut e = format!("nothing deleted through `{}`", rs.id);
            for r in &refused {
                e.push_str(&format!("\n{r}"));
            }
            return Err(e);
        }

        // anything left naming a removed definition would no longer build
        if !gone.is_empty() {
            let sw = sweep(ctx.root, cs, &gone);
            let live: Vec<Hit> = sw
                .hits
                .into_iter()
                .filter(|h| !h.qualifier)
                .filter(|h| {
                    !planned
                        .get(&h.path)
                        .is_some_and(|(_, rs)| rs.iter().any(|r| r.0 <= h.start && h.end <= r.1))
                })
                .collect();
            if !live.is_empty() && !force {
                let sites: Vec<Site> = live
                    .iter()
                    .map(|h| Site {
                        path: h.path.clone(),
                        line: h.line,
                        name: Some(h.text.clone()),
                        kind: kind_at(ctx.caches, &h.path, h.line, &h.text).into(),
                    })
                    .collect();
                let handle = store.record(rs.derived("live", sites));
                let mut e = format!(
                    "refused - {} site(s) still name what this deletes, so the project would stop building.\n\
                     handle `{handle}` holds them: delete them in the same changeset first, rewrite them, or pass force=true\n",
                    live.len()
                );
                for h in live.iter().take(LIST_CAP) {
                    e.push_str(&format!("{}:{} `{}`\n", h.path, h.line, h.quote));
                }
                if live.len() > LIST_CAP {
                    e.push_str(&format!("… and {} more\n", live.len() - LIST_CAP));
                }
                return Err(e);
            }
        }

        let files = planned.len();
        for (path, (src, mut ranges)) in planned {
            let new = splice(&path, &src, &mut ranges)?;
            put(ctx.root, cs, &path, Some(new))?;
        }
        cs.ops.push(format!("delete ({})", rs.id));
        let mut out = format!(
            "# edit_delete {} -> changeset `{}` (staged, nothing written yet)\n{removed} removal(s) in {files} file(s){}\n",
            rs.id,
            cs.id,
            if force { " - forced past live references" } else { "" }
        );
        section(&mut out, "sites left alone", &refused);
        Ok(out)
    })
}

// adds `text` before or after the one site a handle names
pub fn insert(
    ctx: &Ctx,
    store: &mut Store,
    result: &str,
    text: &str,
    position: &str,
    stage: &Stage,
) -> Result<Outcome, String> {
    let rs = store.result(result)?;
    usable(&rs)?;
    if !matches!(position, "before" | "after") {
        return Err(format!("position `{position}` is not one of before | after"));
    }
    if text.trim().is_empty() {
        return Err("`text` is empty".into());
    }
    let skip = parse_skip(&stage.skip)?;
    let sites: Vec<Site> = rs
        .sites
        .iter()
        .filter(|s| !skip.contains(&(s.path.clone(), s.line)))
        .cloned()
        .collect();
    if sites.len() != 1 {
        let mut e = format!(
            "`{}` names {} sites - an insert needs exactly one; `skip` the others or narrow the lookup",
            rs.id,
            sites.len()
        );
        for s in sites.iter().take(LIST_CAP) {
            e.push_str(&format!("\n{}:{} {} {}", s.path, s.line, s.kind, s.name.as_deref().unwrap_or("")));
        }
        return Err(e);
    }
    let site = sites[0].clone();
    staged(ctx, store, stage, |ctx, _, cs| {
        let path = site.path.clone();
        lines_stable(cs, &path)?;
        let src = text_of(ctx.root, cs, &path)?.ok_or_else(|| format!("{path} no longer exists"))?;
        let def = if matches!(site.kind.as_str(), "func" | "const" | "type") {
            let (lang, tree) = parsed(&path, &src)?;
            definition(&tree, &src, lang, &site).map(|n| {
                let lead = row_start(&src, lead_row(n, &src, lang)).min(n.start_byte());
                (lead, line_end(&src, n.end_byte()), n.start_byte())
            })
        } else {
            None
        };
        let edit = match (def, position) {
            // beside a definition - a blank line between it and the new code
            (Some((_, end, first)), "after") => {
                let body = reindent(text, &indent_at(&src, first));
                let lead = if src[..end].ends_with('\n') { "\n" } else { "\n\n" };
                let next = &src[end..line_end(&src, end)];
                let trail = if end < src.len() && !next.trim().is_empty() { "\n\n" } else { "\n" };
                (end, end, format!("{lead}{body}{trail}"))
            }
            (Some((lead, _, first)), _) => {
                let body = reindent(text, &indent_at(&src, first));
                (lead, lead, format!("{body}\n\n"))
            }
            // beside any other line - on the line after or before it
            (None, _) => {
                let a = row_start(&src, site.line - 1);
                let body = reindent(text, &indent_at(&src, a));
                if position == "after" {
                    let b = line_end(&src, a);
                    let lead = if src[..b].ends_with('\n') { "" } else { "\n" };
                    (b, b, format!("{lead}{body}\n"))
                } else {
                    (a, a, format!("{body}\n"))
                }
            }
        };
        let mut edits = vec![(edit.0, edit.1, in_style(&src, edit.2))];
        let new = splice(&path, &src, &mut edits)?;
        put(ctx.root, cs, &path, Some(new))?;
        cs.ops.push(format!("insert {position} {}:{} ({})", path, site.line, rs.id));
        Ok(format!(
            "# edit_insert {} {position} {}:{} -> changeset `{}` (staged, nothing written yet)\n",
            rs.id, path, site.line, cs.id
        ))
    })
}

// any other write - exact text replaced, a whole file written, or a file removed
#[allow(clippy::too_many_arguments)]
pub fn text(
    ctx: &Ctx,
    store: &mut Store,
    path: &str,
    old: Option<&str>,
    new: Option<&str>,
    all: bool,
    delete: bool,
    stage: &Stage,
) -> Result<Outcome, String> {
    staged(ctx, store, stage, |ctx, _, cs| {
        let (rel, _) = confine(ctx.root, path)?;
        let current = text_of(ctx.root, cs, &rel)?;
        let (after, what) = if delete {
            if current.is_none() {
                return Err(format!("{rel} does not exist"));
            }
            (None, "remove".to_string())
        } else {
            let new = new.ok_or("`new` is missing - pass the text to write, or delete=true")?;
            match old.filter(|o| !o.is_empty()) {
                Some(old) => {
                    let cur = current.ok_or_else(|| format!("{rel} does not exist - leave `old` out to create it"))?;
                    let n = cur.matches(old).count();
                    if n == 0 {
                        return Err(format!(
                            "`old` is not in {rel} - it must match exactly, whitespace and indentation included"
                        ));
                    }
                    if n > 1 && !all {
                        return Err(format!(
                            "`old` appears {n} times in {rel} - widen it with surrounding lines to pick one, or pass all=true"
                        ));
                    }
                    let replaced = if all { cur.replace(old, new) } else { cur.replacen(old, new, 1) };
                    (Some(replaced), format!("{} replacement(s)", if all { n } else { 1 }))
                }
                None => (
                    Some(new.to_string()),
                    if current.is_some() { "whole file rewritten" } else { "new file" }.to_string(),
                ),
            }
        };
        put(ctx.root, cs, &rel, after)?;
        cs.ops.push(format!("text {rel} ({what})"));
        Ok(format!(
            "# edit_text {rel} ({what}) -> changeset `{}` (staged, nothing written yet)\n",
            cs.id
        ))
    })
}

// writing

// every file of a changeset written or none - each checked against what is expected there, swapped in through a temp file
fn write_all(root: &Path, cs: &Changeset, forward: bool) -> Result<Vec<String>, String> {
    let mut plan: Vec<(String, PathBuf, Option<&String>, Option<&String>)> = Vec::new();
    for (rel, fc) in cs.files.iter().filter(|(_, fc)| fc.before != fc.after) {
        let (expect, next) = if forward {
            (fc.before.as_ref(), fc.after.as_ref())
        } else {
            (fc.after.as_ref(), fc.before.as_ref())
        };
        let (_, abs) = confine(root, rel)?;
        let now = read_opt(&abs).map_err(|e| format!("reading {rel}: {e}"))?;
        if now.as_ref() != expect {
            return Err(format!(
                "{rel} changed on disk since it was {} - nothing was written",
                if forward { "staged" } else { "applied" }
            ));
        }
        plan.push((rel.clone(), abs, expect, next));
    }

    // stage every new text beside its target first, so a full disk fails before anything lands
    let mut temps: Vec<Option<PathBuf>> = Vec::new();
    let drop_temps = |temps: &[Option<PathBuf>]| {
        for t in temps.iter().flatten() {
            let _ = fs::remove_file(t);
        }
    };
    for (rel, abs, _, next) in &plan {
        let Some(next) = next else {
            temps.push(None);
            continue;
        };
        let staged = (|| -> std::io::Result<PathBuf> {
            if let Some(dir) = abs.parent() {
                fs::create_dir_all(dir)?;
            }
            let name = abs.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let tmp = abs.with_file_name(format!(".{name}.{}.ccc-tmp", cs.id));
            fs::write(&tmp, next.as_bytes())?;
            if let Ok(meta) = fs::metadata(abs) {
                fs::set_permissions(&tmp, meta.permissions())?;
            }
            Ok(tmp)
        })();
        match staged {
            Ok(t) => temps.push(Some(t)),
            Err(e) => {
                drop_temps(&temps);
                return Err(format!("writing {rel}: {e} - nothing was written"));
            }
        }
    }

    // swap in, putting back what already landed if any swap fails
    let mut done: Vec<(&PathBuf, Option<&String>)> = Vec::new();
    for (i, (rel, abs, expect, _)) in plan.iter().enumerate() {
        let swapped = match &temps[i] {
            Some(tmp) => fs::rename(tmp, abs),
            None => fs::remove_file(abs),
        };
        if let Err(e) = swapped {
            for (abs, prev) in done.iter().rev() {
                let _ = match prev {
                    Some(t) => fs::write(abs, t.as_bytes()),
                    None => fs::remove_file(abs),
                };
            }
            drop_temps(&temps[i..]);
            return Err(format!("writing {rel}: {e} - every file was put back"));
        }
        done.push((abs, *expect));
    }
    Ok(plan.into_iter().map(|p| p.0).collect())
}

// writes a pending changeset, then checks every renamed name is gone
pub fn apply(root: &Path, store: &mut Store, id: &str) -> Result<Outcome, String> {
    let id = id.trim();
    let cs = store.pending.remove(id).ok_or_else(|| {
        if store.applied.iter().any(|c| c.id == id) {
            format!("changeset `{id}` is already applied")
        } else {
            format!("no pending changeset `{id}`")
        }
    })?;
    let written = match write_all(root, &cs, true) {
        Ok(w) => w,
        Err(e) => {
            store.pending.insert(id.to_string(), cs);
            return Err(format!("{e}\nchangeset `{id}` is still pending - `edit_discard` it and stage again"));
        }
    };
    let mut out = format!("# edit_apply - {APPLIED_PHRASE}`{id}`\n{} file(s) written:\n", written.len());
    for rel in &written {
        let fc = &cs.files[rel];
        let (adds, dels) = counts(fc.before.as_deref(), fc.after.as_deref());
        let what = match (&fc.before, &fc.after) {
            (None, _) => "new file".to_string(),
            (_, None) => "removed".to_string(),
            _ => format!("+{adds} -{dels}"),
        };
        out.push_str(&format!("  {rel} ({what})\n"));
    }
    if !cs.renames.is_empty() {
        let olds: BTreeSet<String> = cs.renames.keys().cloned().collect();
        let empty = Changeset {
            id: String::new(),
            ops: Vec::new(),
            files: BTreeMap::new(),
            renames: BTreeMap::new(),
            intents: Vec::new(),
            calls: Vec::new(),
        };
        let still: Vec<Hit> = sweep(root, &empty, &olds).hits;
        if still.is_empty() {
            let names: Vec<String> = olds.iter().map(|o| format!("`{o}`")).collect();
            out.push_str(&format!("verified: no identifier in the project is still named {}\n", names.join(", ")));
        } else {
            out.push_str(&format!(
                "still named the old way ({}) - left alone on purpose, or another symbol:\n",
                still.len()
            ));
            for h in still.iter().take(LIST_CAP) {
                out.push_str(&format!("{}:{} `{}`\n", h.path, h.line, h.quote));
            }
        }
    }
    if let Err(e) = ledger_append(root, &ledger_entry(&cs)) {
        out.push_str(&format!("note: the edit ledger was not written ({e}) - `prompts` will not see this change\n"));
    }
    out.push_str(&format!(
        "undo: `edit_revert` changeset=\"{id}\"\nnext: `test_triggers` names the tests this puts at risk\n"
    ));
    let activity = cs.steps("applied");
    store.applied.push(cs);
    if store.applied.len() > MAX_APPLIED {
        store.applied.remove(0);
    }
    Ok(Outcome { text: out, wrote: true, activity })
}

// puts back every file an applied changeset wrote, provided none changed since
pub fn revert(root: &Path, store: &mut Store, id: &str) -> Result<Outcome, String> {
    let id = id.trim();
    let at = store.applied.iter().rposition(|c| c.id == id).ok_or_else(|| {
        if store.pending.contains_key(id) {
            format!("changeset `{id}` was never applied - `edit_discard` drops it")
        } else {
            format!("no applied changeset `{id}` - the last {MAX_APPLIED} applied while this server runs can be reverted")
        }
    })?;
    let cs = store.applied.remove(at);
    let written = match write_all(root, &cs, false) {
        Ok(w) => w,
        Err(e) => {
            store.applied.insert(at, cs);
            return Err(e);
        }
    };
    let _ = ledger_append(
        root,
        &json!({"id": id, "ts": chrono::Utc::now().to_rfc3339(), "reverted": true}),
    );
    let mut out = format!("# edit_revert - reverted changeset `{id}`\n{} file(s) put back:\n", written.len());
    for rel in &written {
        out.push_str(&format!("  {rel}\n"));
    }
    Ok(Outcome { text: out, wrote: true, activity: cs.steps("reverted") })
}

// drops one pending changeset, or all of them
pub fn discard(store: &mut Store, id: Option<&str>) -> Result<Outcome, String> {
    let dropped: Vec<Changeset> = match id.map(str::trim).filter(|i| !i.is_empty()) {
        Some(id) => vec![store
            .pending
            .remove(id)
            .ok_or_else(|| format!("no pending changeset `{id}`"))?],
        None => std::mem::take(&mut store.pending).into_values().collect(),
    };
    let ids: Vec<&str> = dropped.iter().map(|c| c.id.as_str()).collect();
    Ok(Outcome {
        text: if dropped.is_empty() {
            "nothing pending - no changeset to discard\n".into()
        } else {
            format!("discarded {} - nothing was written\n", ids.join(", "))
        },
        wrote: false,
        activity: dropped.iter().map(|c| c.activity("discarded")).collect(),
    })
}

// the ledger

fn ledger_entry(cs: &Changeset) -> Value {
    let files: Vec<Value> = cs
        .files
        .iter()
        .filter(|(_, fc)| fc.before != fc.after)
        .map(|(rel, fc)| {
            let written = added_lines(fc.before.as_deref(), fc.after.as_deref());
            let written: String = written.chars().take(LEDGER_TEXT_CAP).collect();
            json!({"path": rel, "written": written})
        })
        .collect();
    json!({
        "id": cs.id,
        "ts": chrono::Utc::now().to_rfc3339(),
        "ops": cs.ops,
        "intent": cs.intents,
        "files": files,
    })
}

fn ledger_append(root: &Path, entry: &Value) -> Result<(), String> {
    let dir = root.join(".ccc");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LEDGER_NAME))
        .map_err(|e| e.to_string())?;
    writeln!(f, "{entry}").map_err(|e| e.to_string())?;
    crate::prompts::ignore_path(
        root,
        &format!("/.ccc/{LEDGER_NAME}"),
        "edits applied through ccc's edit tools, read back by `ccc prompts`",
    )
    .map_err(|e| e.to_string())
}

// what each applied changeset wrote, by id, without the ones reverted since - for `prompts`
pub fn ledger(root: &Path) -> BTreeMap<String, Vec<(String, String)>> {
    let mut out = BTreeMap::new();
    let Ok(raw) = fs::read_to_string(root.join(".ccc").join(LEDGER_NAME)) else {
        return out;
    };
    for v in raw.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        let Some(id) = v.get("id").and_then(|i| i.as_str()) else {
            continue;
        };
        if v.get("reverted").and_then(|r| r.as_bool()).unwrap_or(false) {
            out.remove(id);
            continue;
        }
        let files: Vec<(String, String)> = v
            .get("files")
            .and_then(|f| f.as_array())
            .into_iter()
            .flatten()
            .filter_map(|f| {
                Some((
                    f.get("path")?.as_str()?.to_string(),
                    f.get("written").and_then(|w| w.as_str()).unwrap_or_default().to_string(),
                ))
            })
            .collect();
        out.insert(id.to_string(), files);
    }
    out
}

// One ledger line as any server reads it back: a changeset applied - what it
// did, why, and what it wrote to each file - or one reverted.
#[derive(Debug, Clone)]
pub struct LedgerLine {
    pub id: String,
    pub reverted: bool,
    pub ops: Vec<String>,
    pub intent: Vec<String>,
    // each file, and the lines the changeset added to it
    pub files: Vec<(String, String)>,
}

// how far the ledger runs now - a reader starting here sees only what comes next
pub fn ledger_len(root: &Path) -> u64 {
    fs::metadata(root.join(".ccc").join(LEDGER_NAME)).map_or(0, |m| m.len())
}

// The ledger lines past byte `from`, and the byte they end at. A line still
// being written waits for the next read; a ledger shorter than `from` was
// started again, so it is read from the top.
pub fn ledger_since(root: &Path, from: u64) -> (Vec<LedgerLine>, u64) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = fs::File::open(root.join(".ccc").join(LEDGER_NAME)) else {
        return (Vec::new(), 0);
    };
    let from = if f.metadata().map_or(0, |m| m.len()) < from { 0 } else { from };
    let mut tail = Vec::new();
    if f.seek(SeekFrom::Start(from)).is_err() || f.read_to_end(&mut tail).is_err() {
        return (Vec::new(), from);
    }
    let end = tail.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let strs = |v: &Value, k: &str| -> Vec<String> {
        v.get(k)
            .and_then(|a| a.as_array())
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect()
    };
    let lines = String::from_utf8_lossy(&tail[..end])
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            Some(LedgerLine {
                id: v.get("id")?.as_str()?.to_string(),
                reverted: v.get("reverted").and_then(|r| r.as_bool()).unwrap_or(false),
                ops: strs(&v, "ops"),
                intent: strs(&v, "intent"),
                files: v
                    .get("files")
                    .and_then(|f| f.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|f| {
                        let written = f.get("written").and_then(|w| w.as_str()).unwrap_or_default();
                        Some((f.get("path")?.as_str()?.to_string(), written.to_string()))
                    })
                    .collect(),
            })
        })
        .collect();
    (lines, from + end as u64)
}

// every changeset id an answer names - staged, applied, reverted or dropped
pub fn changeset_ids(answer: &str) -> Vec<String> {
    let mut out: Vec<String> = answer
        .match_indices("changeset `")
        .map(|(i, m)| {
            answer[i + m.len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect::<String>()
        })
        .filter(|id| !id.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

// the changeset ids an answer reports as applied
pub fn applied_ids(answer: &str) -> Vec<String> {
    answer
        .match_indices(APPLIED_PHRASE)
        .map(|(i, _)| {
            answer[i + APPLIED_PHRASE.len()..]
                .trim_start_matches('`')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .collect::<String>()
        })
        .filter(|id| !id.is_empty())
        .collect()
}

// diffs

#[derive(Debug, Clone, Copy, PartialEq)]
enum Line {
    Same(usize, usize),
    Gone(usize),
    Added(usize),
}

fn diff_lines(a: &[&str], b: &[&str]) -> Vec<Line> {
    // shared head and tail first - most edits touch a few lines of a long file
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf] {
        suf += 1;
    }
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut out: Vec<Line> = (0..pre).map(|i| Line::Same(i, i)).collect();
    match myers(am, bm) {
        Some(mid) => out.extend(mid.into_iter().map(|l| match l {
            Line::Same(i, j) => Line::Same(i + pre, j + pre),
            Line::Gone(i) => Line::Gone(i + pre),
            Line::Added(j) => Line::Added(j + pre),
        })),
        None => {
            out.extend((0..am.len()).map(|i| Line::Gone(pre + i)));
            out.extend((0..bm.len()).map(|j| Line::Added(pre + j)));
        }
    }
    out.extend((0..suf).map(|i| Line::Same(a.len() - suf + i, b.len() - suf + i)));
    out
}

// myers' shortest edit script, or none past MAX_DIFF_D
fn myers(a: &[&str], b: &[&str]) -> Option<Vec<Line>> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max = (n + m) as usize;
    let off = max as isize + 1;
    let mut v = vec![0isize; 2 * max + 3];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found = None;
    'outer: for d in 0..=(max.min(MAX_DIFF_D) as isize) {
        trace.push(v[(off - d) as usize..=(off + d) as usize].to_vec());
        let mut k = -d;
        while k <= d {
            let i = (off + k) as usize;
            let mut x = if k == -d || (k != d && v[i - 1] < v[i + 1]) { v[i + 1] } else { v[i - 1] + 1 };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[i] = x;
            if x >= n && y >= m {
                found = Some(d);
                break 'outer;
            }
            k += 2;
        }
    }
    let last = found?;
    let (mut x, mut y) = (n, m);
    let mut rev = Vec::new();
    for d in (0..=last).rev() {
        let vd = &trace[d as usize];
        let at = |k: isize| vd[(k + d) as usize];
        let k = x - y;
        let prev_k = if k == -d || (k != d && at(k - 1) < at(k + 1)) { k + 1 } else { k - 1 };
        let prev_x = if d == 0 { 0 } else { at(prev_k) };
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            rev.push(Line::Same((x - 1) as usize, (y - 1) as usize));
            x -= 1;
            y -= 1;
        }
        if d > 0 {
            if x == prev_x {
                rev.push(Line::Added((y - 1) as usize));
            } else {
                rev.push(Line::Gone((x - 1) as usize));
            }
        }
        x = prev_x;
        y = prev_y;
    }
    rev.reverse();
    Some(rev)
}

fn split(t: Option<&str>) -> Vec<&str> {
    t.map(|t| t.lines().collect()).unwrap_or_default()
}

fn counts(before: Option<&str>, after: Option<&str>) -> (usize, usize) {
    let (a, b) = (split(before), split(after));
    diff_lines(&a, &b).iter().fold((0, 0), |(ad, de), l| match l {
        Line::Added(_) => (ad + 1, de),
        Line::Gone(_) => (ad, de + 1),
        Line::Same(..) => (ad, de),
    })
}

// the lines an edit wrote, joined - what `prompts` looks for again later
fn added_lines(before: Option<&str>, after: Option<&str>) -> String {
    let (a, b) = (split(before), split(after));
    diff_lines(&a, &b)
        .iter()
        .filter_map(|l| match l {
            Line::Added(j) => Some(b[*j]),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// One run of change, as `(start, len)` on each side, 1-based. An empty side
// starts at the line the run sits before, so a pure insertion still names
// where in `before` it lands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hunk {
    pub before: (usize, usize),
    pub after: (usize, usize),
}

impl Hunk {
    // the lines one side spans, inclusive - an empty side is its one anchor line
    pub fn span(side: (usize, usize)) -> (usize, usize) {
        (side.0, side.0 + side.1.max(1) - 1)
    }
}

// every run of lines a change touched, in order
pub fn hunks(before: Option<&str>, after: Option<&str>) -> Vec<Hunk> {
    let (a, b) = (split(before), split(after));
    let mut out: Vec<Hunk> = Vec::new();
    let mut open: Option<Hunk> = None;
    // the next line on each side, 0-based
    let (mut ni, mut nj) = (0, 0);
    for l in diff_lines(&a, &b) {
        match l {
            Line::Same(i, j) => {
                out.extend(open.take());
                (ni, nj) = (i + 1, j + 1);
            }
            Line::Gone(i) => {
                open.get_or_insert(Hunk { before: (i + 1, 0), after: (nj + 1, 0) }).before.1 += 1;
                ni = i + 1;
            }
            Line::Added(j) => {
                open.get_or_insert(Hunk { before: (ni + 1, 0), after: (j + 1, 0) }).after.1 += 1;
                nj = j + 1;
            }
        }
    }
    out.extend(open);
    out
}

// one file as a unified diff
pub(crate) fn unified(rel: &str, before: Option<&str>, after: Option<&str>) -> Vec<String> {
    let (a, b) = (split(before), split(after));
    let ops = diff_lines(&a, &b);
    let mut out = vec![
        format!("--- {}", if before.is_some() { format!("a/{rel}") } else { "/dev/null".into() }),
        format!("+++ {}", if after.is_some() { format!("b/{rel}") } else { "/dev/null".into() }),
    ];
    // where each op starts on either side
    let mut pos = Vec::with_capacity(ops.len());
    let (mut ai, mut bi) = (0, 0);
    for l in &ops {
        pos.push((ai, bi));
        match l {
            Line::Same(..) => {
                ai += 1;
                bi += 1;
            }
            Line::Gone(_) => ai += 1,
            Line::Added(_) => bi += 1,
        }
    }
    let changed: Vec<usize> = (0..ops.len()).filter(|&i| !matches!(ops[i], Line::Same(..))).collect();
    let mut i = 0;
    while i < changed.len() {
        let mut j = i;
        while j + 1 < changed.len() && changed[j + 1] - changed[j] <= 2 * CONTEXT + 1 {
            j += 1;
        }
        let from = changed[i].saturating_sub(CONTEXT);
        let to = (changed[j] + CONTEXT).min(ops.len() - 1);
        let (a0, b0) = pos[from];
        let (mut al, mut bl) = (0, 0);
        let mut body = Vec::new();
        for l in &ops[from..=to] {
            match *l {
                Line::Same(x, _) => {
                    al += 1;
                    bl += 1;
                    body.push(format!(" {}", a[x]));
                }
                Line::Gone(x) => {
                    al += 1;
                    body.push(format!("-{}", a[x]));
                }
                Line::Added(y) => {
                    bl += 1;
                    body.push(format!("+{}", b[y]));
                }
            }
        }
        let start = |s: usize, len: usize| if len == 0 { s } else { s + 1 };
        out.push(format!("@@ -{},{al} +{},{bl} @@", start(a0, al), start(b0, bl)));
        out.extend(body);
        i = j + 1;
    }
    out
}

// the whole changeset as a diff, capped
fn preview(cs: &Changeset) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (rel, fc) in cs.files.iter().filter(|(_, fc)| fc.before != fc.after) {
        lines.extend(unified(rel, fc.before.as_deref(), fc.after.as_deref()));
    }
    if lines.is_empty() {
        return "the changeset leaves every file as it is\n".into();
    }
    let total = lines.len();
    let mut out = String::from("```diff\n");
    for l in lines.iter().take(DIFF_CAP) {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str("```\n");
    if total > DIFF_CAP {
        out.push_str(&format!(
            "… {} more diff line(s) not shown - the whole change is staged\n",
            total - DIFF_CAP
        ));
    }
    out
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut s = Vec::new();
    loop {
        s.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    s.reverse();
    String::from_utf8(s).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    // a throwaway directory removed on drop
    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Dir {
            let p = std::env::temp_dir().join(format!("ccc-edit-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_path_stays_inside_the_root() {
        let d = Dir::new("confine");
        let root = &d.0;
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(confine(root, "src/a.rs").unwrap().0, "src/a.rs");
        assert_eq!(confine(root, "./src/../src/a.rs").map(|c| c.0).ok(), None, "`..` is refused even when it lands inside");
        assert_eq!(confine(root, &root.join("src/a.rs").to_string_lossy()).unwrap().0, "src/a.rs");
        assert!(confine(root, "/etc/passwd").is_err());
        assert!(confine(root, "../outside.rs").is_err());
        assert!(confine(root, ".git/config").is_err());
        assert!(confine(root, "src").is_err(), "a directory is not a file");
        assert!(confine(root, ".ccc/edits.jsonl").is_err(), "the ledgers are not hand-edited");
        assert!(confine(root, ".ccc/map.json").is_ok());
        assert!(confine(root, "").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_on_the_way_is_refused() {
        let d = Dir::new("symlink");
        let outside = Dir::new("symlink-target");
        std::os::unix::fs::symlink(&outside.0, d.0.join("escape")).unwrap();
        fs::write(outside.0.join("f.rs"), "fn f() {}\n").unwrap();
        std::os::unix::fs::symlink(outside.0.join("f.rs"), d.0.join("link.rs")).unwrap();
        let e = confine(&d.0, "escape/f.rs").unwrap_err();
        assert!(e.contains("symlink"), "{e}");
        assert!(confine(&d.0, "link.rs").unwrap_err().contains("symlink"));
        assert!(confine(&d.0, "escape/new.rs").is_err(), "nor a new file beyond one");
    }

    #[test]
    fn a_find_substring_keeps_each_identifiers_case_shape() {
        assert_eq!(substitute("hello_all", "hello", "hi"), "hi_all");
        assert_eq!(substitute("HelloWorld", "hello", "hi"), "HiWorld");
        assert_eq!(substitute("MAX_HELLO", "hello", "hi"), "MAX_HI");
        assert_eq!(substitute("say_hello_hello", "hello", "hi"), "say_hi_hi");
        assert!(valid_ident("hi_all") && valid_ident("_x") && valid_ident("$el"));
        assert!(!valid_ident("1x") && !valid_ident("a-b") && !valid_ident(""));
    }

    #[test]
    fn the_diff_shows_only_what_moved() {
        let before = "a\nb\nc\nd\ne\nf\ng\n";
        let after = "a\nb\nC\nd\ne\nf\ng\nh\n";
        let d = unified("x.rs", Some(before), Some(after)).join("\n");
        assert!(d.contains("@@ -2,3 +2,3 @@\n b\n-c\n+C\n d"), "{d}");
        assert!(d.contains("+h"), "{d}");
        assert!(!d.contains(" a\n"), "context is one line, not the file: {d}");
        assert_eq!(counts(Some(before), Some(after)), (2, 1));
        let new = unified("n.rs", None, Some("x\n")).join("\n");
        assert!(new.starts_with("--- /dev/null\n+++ b/n.rs\n@@ -0,0 +1,1 @@\n+x"), "{new}");
    }

    #[test]
    fn a_hunk_names_both_sides_even_when_one_is_empty() {
        let before = "a\nb\nc\nd\ne\n";
        let after = "a\nB\nc\nx\ny\nd\n";
        assert_eq!(
            hunks(Some(before), Some(after)),
            vec![
                // b -> B
                Hunk { before: (2, 1), after: (2, 1) },
                // x, y land before d
                Hunk { before: (4, 0), after: (4, 2) },
                // e goes, and the end of `after` is where it went
                Hunk { before: (5, 1), after: (7, 0) },
            ]
        );
        assert_eq!(Hunk::span((4, 0)), (4, 4));
        assert_eq!(Hunk::span((4, 2)), (4, 5));
        assert_eq!(hunks(None, Some("x\n")), vec![Hunk { before: (1, 0), after: (1, 1) }]);
        assert!(hunks(Some(before), Some(before)).is_empty());
    }

    #[test]
    fn a_step_carries_both_texts_and_a_revert_runs_backwards() {
        let file = |before: Option<&str>, after: Option<&str>| FileChange {
            before: before.map(str::to_string),
            after: after.map(str::to_string),
        };
        let cs = Changeset {
            id: "c1-x".into(),
            ops: vec!["text a.rs (1 replacement(s))".into()],
            files: BTreeMap::from([
                ("a.rs".to_string(), file(Some("fn a() {}\n"), Some("fn a() { b(); }\n"))),
                ("b.rs".to_string(), file(None, Some("fn b() {}\n"))),
                // staged and staged back - nothing moves, so the step leaves it out
                ("c.rs".to_string(), file(Some("x\n"), Some("x\n"))),
            ]),
            renames: BTreeMap::new(),
            intents: Vec::new(),
            calls: Vec::new(),
        };
        let applied = cs.activity("applied");
        assert_eq!((applied.changeset.as_str(), applied.status), ("c1-x", "applied"));
        let paths: Vec<&str> = applied.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.rs", "b.rs"]);
        assert_eq!(applied.files[0].after.as_deref(), Some("fn a() { b(); }\n"));
        assert_eq!(applied.files[1].before, None);

        let reverted = cs.activity("reverted");
        assert_eq!(reverted.files[0].before.as_deref(), Some("fn a() { b(); }\n"));
        assert_eq!(reverted.files[0].after.as_deref(), Some("fn a() {}\n"));
        // reverting a created file removes it
        assert_eq!(reverted.files[1].after, None);
    }

    // A changeset staged over several calls is applied as those calls, in the
    // order they came, and reverted as the same calls run backwards.
    #[test]
    fn a_changeset_replays_the_calls_that_built_it() {
        let dir = Dir::new("calls");
        fs::write(dir.0.join("a.rs"), "fn a() {}\n").unwrap();
        let caches = Vec::new();
        let ctx = Ctx { root: &dir.0, caches: &caches };
        let mut store = Store::new();
        let stage = |changeset: Option<&str>, intent: &str| Stage {
            changeset: changeset.map(str::to_string),
            intent: Some(intent.into()),
            ..Default::default()
        };
        let first = text(&ctx, &mut store, "a.rs", Some("fn a() {}"), Some("fn a() { b(); }"), false, false, &stage(None, "call b"))
            .unwrap();
        assert_eq!(first.activity.len(), 1);
        let id = first.activity[0].changeset.clone();
        let second = text(&ctx, &mut store, "a.rs", None, Some("fn a() { b(); }\n\nfn b() {}\n"), false, false, &stage(Some(&id), "add b"))
            .unwrap();
        // a staging call is its own change, not the changeset so far
        assert_eq!(second.activity[0].files[0].before.as_deref(), Some("fn a() { b(); }\n"));
        assert_eq!(second.activity[0].intent, ["add b"]);

        let applied = apply(&dir.0, &mut store, &id).unwrap().activity;
        let sides: Vec<(&str, Option<&str>, Option<&str>)> = applied
            .iter()
            .map(|s| (s.status, s.files[0].before.as_deref(), s.files[0].after.as_deref()))
            .collect();
        assert_eq!(
            sides,
            [
                ("applied", Some("fn a() {}\n"), Some("fn a() { b(); }\n")),
                ("applied", Some("fn a() { b(); }\n"), Some("fn a() { b(); }\n\nfn b() {}\n")),
            ]
        );
        let reverted = revert(&dir.0, &mut store, &id).unwrap().activity;
        let intents: Vec<&str> = reverted.iter().map(|s| s.intent[0].as_str()).collect();
        assert_eq!(intents, ["add b", "call b"]);
        assert_eq!(reverted[0].files[0].after.as_deref(), Some("fn a() { b(); }\n"));
        assert_eq!(reverted[1].files[0].after.as_deref(), Some("fn a() {}\n"));
    }

    #[test]
    fn an_answer_names_the_changesets_it_applied() {
        let text = format!("# edit_apply - {APPLIED_PHRASE}`c4-k2x9`\nnext");
        assert_eq!(applied_ids(&text), vec!["c4-k2x9"]);
        assert!(applied_ids("staged, nothing written yet").is_empty());
    }

    #[test]
    fn a_removed_definition_takes_its_lines_and_closes_the_gap() {
        let src = "fn a() {}\n\nfn b() {}\n\nfn c() {}\n";
        let start = src.find("fn b").unwrap();
        let (s, e) = removal(src, start, start + "fn b() {}".len());
        let mut out = src.to_string();
        out.replace_range(s..e, "");
        assert_eq!(out, "fn a() {}\n\nfn c() {}\n");
        // inline, the node and its separator go and the rest of the line stays
        let src = "enum M { Fast, Slow }";
        let start = src.find("Fast").unwrap();
        let (s, e) = removal(src, start, start + 4);
        let mut out = src.to_string();
        out.replace_range(s..e, "");
        assert_eq!(out, "enum M { Slow }");
    }
}
