//! `ccc probe` - a copy of a project with ccc's runtime probes in: every function
//! counts its runs, every test names itself a fixture, and servers and clients
//! carry that fixture between services, so one recorded run yields the
//! `ccc-observed/1` records `observed.rs` reads.

use crate::changes::{is_test_fn_name, is_test_path, path_str};
use crate::extract;
use crate::languages::Language;
use crate::model::{Func, Import};
use crate::scan::{MAX_FILE_BYTES, SKIP_DIRS};
use anyhow::{bail, Context, Result};
use globset::{Glob, GlobMatcher};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use tree_sitter::{Node, Parser, Tree};

mod cpp;
mod go;
mod python;
mod rust;
mod typescript;

// where each language's runtime goes in the copy - hidden, so ccc never maps it as the project's own code
pub const RUNTIME_DIR: &str = ".ccc-probe";

// the fixture kinds `observed.rs` weighs
const KINDS: &[&str] = &["unit", "contract", "interop", "integration", "smoke", "load"];

// directories linked into the copy rather than copied - installed packages, never source
const LINKED_DIRS: &[&str] = &[".venv", "venv"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Go,
    Rust,
    Cpp,
    TypeScript,
    Python,
}

impl Target {
    pub fn parse(s: &str) -> Option<Target> {
        Some(match s.to_ascii_lowercase().as_str() {
            "go" | "golang" => Target::Go,
            "rust" | "rs" => Target::Rust,
            "c++" | "cpp" | "cxx" => Target::Cpp,
            "typescript" | "ts" => Target::TypeScript,
            "python" | "py" => Target::Python,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Target::Go => "go",
            Target::Rust => "rust",
            Target::Cpp => "cpp",
            Target::TypeScript => "typescript",
            Target::Python => "python",
        }
    }

    fn handles(self, lang: Language) -> bool {
        matches!(
            (self, lang),
            (Target::Go, Language::Go)
                | (Target::Rust, Language::Rust)
                | (Target::Cpp, Language::Cpp)
                | (Target::TypeScript, Language::TypeScript | Language::Tsx)
                | (Target::Python, Language::Python)
        )
    }
}

pub struct Options {
    // the languages to probe, each into the same copy
    pub targets: Vec<Target>,
    // the project, or the part of one, to instrument
    pub src: PathBuf,
    // where the instrumented copy goes
    pub out: PathBuf,
    // `GLOB=KIND` - the fixture kind of the tests a glob matches, over the default reading of their path
    pub kinds: Vec<String>,
    // write into an `out` that already holds files
    pub force: bool,
    // the directory records name files from - the nearest ccc project or repository above `src` when left out
    pub root: Option<PathBuf>,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub targets: Vec<&'static str>,
    pub src: String,
    pub out: String,
    // the path records name files by - `src` relative to its repository
    pub prefix: String,
    pub commit: Option<String>,
    pub copied: usize,
    // files the probes went into
    pub files: usize,
    // functions that count their runs
    pub functions: usize,
    // tests that name themselves fixtures
    pub fixtures: usize,
    // servers, clients and spawned work that carry a fixture
    pub wrapped: usize,
    // runtime files written into the copy
    pub runtime: Vec<String>,
    // build files changed so the copy finds the runtime
    pub edited: Vec<String>,
    // files left as they were, and why
    pub skipped: Vec<String>,
    // how to record with the copy
    pub next: Vec<String>,
    // how each language's suites run - folded into `next`
    #[serde(skip)]
    pub(crate) run: Vec<String>,
}

// one file's probes - the text with them in, and what went in
#[derive(Default)]
pub(crate) struct Rewrite {
    pub edits: Edits,
    pub functions: usize,
    pub fixtures: usize,
    pub wrapped: usize,
}

// what one language does to the copy - read its build files, probe each source file, then write the runtime
pub(crate) trait Prober {
    fn prepare(&mut self, cx: &Cx) -> Result<()>;
    fn rewrite(&mut self, cx: &Cx, file: &Source) -> Result<Rewrite>;
    fn finish(&mut self, cx: &Cx, report: &mut Report) -> Result<()>;
}

// what every language's rewrite reads about the run
pub(crate) struct Cx {
    pub target: Target,
    pub out: PathBuf,
    // prepended to a path inside the copy to name the file the way ccc maps it
    pub prefix: String,
    pub commit: Option<String>,
    // every regular file copied, by its path inside the copy
    pub files: BTreeSet<String>,
    rules: Vec<(GlobMatcher, &'static str)>,
}

impl Cx {
    // the path a record names a file by - the repository's path, not the copy's
    pub fn record_path(&self, rel: &str) -> String {
        format!("{}{rel}", self.prefix)
    }

    // the fixture kind of a test file - a `--kind` glob first, then what its path says
    pub fn kind(&self, rel: &str) -> &'static str {
        let path = self.record_path(rel);
        self.rules
            .iter()
            .find(|(glob, _)| glob.is_match(rel) || glob.is_match(&path))
            .map(|(_, kind)| *kind)
            .unwrap_or_else(|| default_kind(&path, self.target))
    }

    pub fn write(&self, rel: &str, text: &str) -> Result<()> {
        let to = self.out.join(rel);
        if let Some(dir) = to.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(&to, text).with_context(|| format!("writing {}", to.display()))
    }

    pub fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.out.join(rel)).ok()
    }
}

// one source file as the rewrite sees it
pub(crate) struct Source {
    // its path inside the copy
    pub rel: String,
    // its path as ccc maps it
    pub path: String,
    pub lang: Language,
    pub text: String,
    pub tree: Tree,
    pub funcs: Vec<Func>,
    pub imports: Vec<Import>,
    // a test file - its functions are tests or their helpers, so none counts its runs
    pub test: bool,
    pub kind: &'static str,
}

impl Source {
    pub fn root(&self) -> Node<'_> {
        self.tree.root_node()
    }

    pub fn text_of(&self, node: Node) -> &str {
        &self.text[node.byte_range()]
    }

    // every function the map names, with its node - the map's own name and owner are what the records use
    pub fn defs(&self) -> Vec<Def<'_>> {
        let mut nodes = Vec::new();
        let kinds = self.lang.func_kinds();
        collect(self.root(), &mut |n| {
            if kinds.contains(&n.kind()) {
                nodes.push(n);
            }
        });
        let starts = line_starts(&self.text);
        let offset = |line: usize, col: usize| starts.get(line.saturating_sub(1)).map_or(0, |s| s + col.saturating_sub(1));
        let mut out = Vec::new();
        let mut taken = BTreeSet::new();
        for node in nodes {
            let (start, end) = (node.start_position().row + 1, node.end_position().row + 1);
            let at = node.start_byte();
            // the name nearest the node's start - just inside it for a declaration, just before it for a bound lambda
            let best = (0..self.funcs.len())
                .filter(|&k| self.funcs[k].start_line == start && self.funcs[k].end_line == end && !taken.contains(&k))
                .min_by_key(|&k| offset(self.funcs[k].line, self.funcs[k].col).abs_diff(at));
            if let Some(k) = best {
                taken.insert(k);
                out.push(Def { node, func: &self.funcs[k] });
            }
        }
        out
    }

    // the name a module path is bound to in this file, if it is imported
    pub fn import_name(&self, module: &str) -> Option<String> {
        let import = self.imports.iter().find(|i| i.module == module)?;
        Some(import.names.first().cloned().unwrap_or_else(|| module.rsplit('/').next().unwrap_or(module).to_string()))
    }
}

pub(crate) struct Def<'a> {
    pub node: Node<'a>,
    pub func: &'a Func,
}

impl Def<'_> {
    // `Owner<sep>name` - the qualified name `observed.rs` splits back into owner and name
    pub fn qualified(&self, sep: &str) -> String {
        match &self.func.owner {
            Some(owner) if !owner.is_empty() => format!("{owner}{sep}{}", self.func.name),
            _ => self.func.name.clone(),
        }
    }

    pub fn is_test(&self) -> bool {
        self.func.test_ctx || is_test_fn_name(&self.func.name)
    }
}

// text to put in at offsets of the source - only a replacement cuts anything, so every comment stays where it was
#[derive(Default)]
pub(crate) struct Edits(Vec<(usize, i64, String, usize)>);

impl Edits {
    pub fn insert(&mut self, at: usize, text: impl Into<String>) {
        let n = self.0.len() as i64;
        self.0.push((at, n, text.into(), at));
    }

    // closing text - at one offset it goes before any opening text, the last made first, so wraps nest
    pub fn close(&mut self, at: usize, text: impl Into<String>) {
        let n = self.0.len() as i64;
        self.0.push((at, -n - 1, text.into(), at));
    }

    // a node's text swapped for another - a callee renamed, never a span another edit lands inside
    pub fn replace(&mut self, node: Node, text: impl Into<String>) {
        let n = self.0.len() as i64;
        self.0.push((node.start_byte(), n, text.into(), node.end_byte()));
    }

    // `open` before a node and `close` after it
    pub fn wrap(&mut self, node: Node, open: impl Into<String>, close: impl Into<String>) {
        self.insert(node.start_byte(), open);
        self.close(node.end_byte(), close);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn apply(mut self, src: &str) -> String {
        self.0.sort_by_key(|(at, order, _, _)| (*at, *order));
        let mut out = String::with_capacity(src.len() + self.0.iter().map(|(_, _, t, _)| t.len()).sum::<usize>());
        let mut last = 0;
        for (at, _, text, end) in &self.0 {
            if *at >= last {
                out.push_str(&src[last..*at]);
                last = *at;
            }
            out.push_str(text);
            last = last.max(*end);
        }
        out.push_str(&src[last..]);
        out
    }
}

// a double-quoted string literal every target language reads the same way
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// `to` relative to the directory `from` - both paths inside the copy, `/`-separated
pub(crate) fn relative(from: &str, to: &str) -> String {
    let from: Vec<&str> = from.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    let to: Vec<&str> = to.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<&str> = vec![".."; from.len() - common];
    parts.extend(&to[common..]);
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

// the directory a path inside the copy sits in - empty at the top
pub(crate) fn dir_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or("", |(d, _)| d)
}

// every node under `node`, depth first
pub(crate) fn collect<'t>(node: Node<'t>, f: &mut impl FnMut(Node<'t>)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, f);
    }
}

// the first direct child of a kind
pub(crate) fn child_of_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).find(|c| c.kind() == kind);
    found
}

pub(crate) fn named_children(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).filter(|c| !c.is_extra()).collect()
}

fn line_starts(text: &str) -> Vec<usize> {
    std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect()
}

// the kind a test's path says it is - its own name first, then its directories innermost out
pub(crate) fn default_kind(path: &str, target: Target) -> &'static str {
    let lower = path.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('/').collect();
    let (dirs, file) = parts.split_at(parts.len() - 1);
    for word in file[0].split(['.', '_', '-']) {
        let kind = match word {
            "contract" | "contracts" | "pact" => "contract",
            "interop" => "interop",
            "integration" | "e2e" => "integration",
            "smoke" => "smoke",
            _ => continue,
        };
        return kind;
    }
    for dir in dirs.iter().rev() {
        if *dir == "it" {
            return "integration";
        }
        for word in dir.split(['.', '_', '-']) {
            let kind = match word {
                "contract" | "contracts" | "pact" | "pacts" => "contract",
                "interop" => "interop",
                "smoke" => "smoke",
                "load" | "perf" | "performance" | "stress" | "soak" | "benchmark" | "benchmarks" | "bench" | "benches" => "load",
                "integration" | "integrations" | "e2e" | "end2end" | "acceptance" | "functional" => "integration",
                _ => continue,
            };
            return kind;
        }
    }
    // a crate's `tests/` directory is where rust keeps its integration tests
    if target == Target::Rust && dirs.contains(&"tests") {
        return "integration";
    }
    "unit"
}

fn parse_rules(kinds: &[String]) -> Result<Vec<(GlobMatcher, &'static str)>> {
    kinds
        .iter()
        .map(|rule| {
            let (glob, kind) = rule.rsplit_once('=').with_context(|| format!("--kind wants GLOB=KIND, got '{rule}'"))?;
            let kind = KINDS
                .iter()
                .find(|k| **k == kind.trim())
                .with_context(|| format!("--kind '{rule}': the kind must be one of {}", KINDS.join(", ")))?;
            let glob = Glob::new(glob.trim()).with_context(|| format!("--kind '{rule}'"))?.compile_matcher();
            Ok((glob, *kind))
        })
        .collect()
}

// `dir`'s path inside the project ccc maps it from - `root` when given, else the nearest ccc project or repository above it
fn repo_prefix(dir: &Path, root: Option<&Path>) -> Result<(String, Option<PathBuf>)> {
    let found = match root {
        Some(root) => Some(root.canonicalize().with_context(|| format!("reading --root {}", root.display()))?),
        None => dir.ancestors().find(|a| a.join(".ccc").is_dir() || a.join(".git").exists()).map(Path::to_path_buf),
    };
    let Some(root) = found else {
        return Ok((String::new(), None));
    };
    if !dir.starts_with(&root) {
        bail!("--root {} is not above {}", root.display(), dir.display());
    }
    let prefix = match dir.strip_prefix(&root) {
        Ok(rel) if !rel.as_os_str().is_empty() => format!("{}/", path_str(rel)),
        _ => String::new(),
    };
    Ok((prefix, Some(root)))
}

// the commit the copy is made from - only where records name files from the repository's own root, as `git diff` names them
fn root_commit(root: Option<&Path>) -> Option<String> {
    root.filter(|r| r.join(".git").exists()).and_then(head_commit)
}

fn head_commit(dir: &Path) -> Option<String> {
    let out = Command::new("git").args(["rev-parse", "HEAD"]).current_dir(dir).output().ok()?;
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && id.bytes().all(|b| b.is_ascii_hexdigit()) && !id.is_empty()).then_some(id)
}

// a file the map holds - outside every directory ccc's scan skips, and small enough to parse
fn mapped(rel: &str, len: u64) -> bool {
    len <= MAX_FILE_BYTES
        && dir_of(rel).split('/').filter(|s| !s.is_empty()).all(|d| !d.starts_with('.') && !SKIP_DIRS.contains(&d))
}

// generated code - its functions are the generator's, so no record should name them
fn generated(rel: &str, text: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        ".pb.go", "_pb2.py", "_pb2_grpc.py", ".pb.cc", ".pb.h", ".pb.ts", "_pb.ts", "_grpc_pb.ts", ".pb.rs",
    ];
    if SUFFIXES.iter().any(|s| rel.ends_with(s)) {
        return true;
    }
    let head = &text[..text.len().min(2048)];
    (head.contains("Code generated") && head.contains("DO NOT EDIT")) || head.contains("@generated")
}

// copy `src` into `out` the way git sees it - ignored files left behind, installed packages linked rather than copied
fn copy_tree(src: &Path, out: &Path, files: &mut BTreeSet<String>) -> Result<()> {
    let out_owned = out.to_path_buf();
    let walker = ignore::WalkBuilder::new(src)
        .hidden(false)
        .parents(true)
        .ignore(false)
        .git_global(false)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .filter_entry(move |e| {
            let name = e.file_name().to_string_lossy();
            let dir = e.file_type().is_some_and(|t| t.is_dir());
            // build output a repository without a .gitignore still holds - a fresh build in the copy remakes it
            let built = name == "__pycache__" || (name == "target" && e.path().parent().is_some_and(|p| p.join("Cargo.toml").is_file()));
            !(dir && (built || name == ".git" || name == "node_modules" || LINKED_DIRS.contains(&name.as_ref()) || e.path() == out_owned))
        })
        .build();
    for entry in walker {
        let entry = entry?;
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(src) else { continue };
        let rel = path_str(rel);
        let to = out.join(&rel);
        let Some(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            std::fs::create_dir_all(&to).with_context(|| format!("creating {}", to.display()))?;
            link_installed(path, &to)?;
        } else if kind.is_symlink() {
            let target = std::fs::read_link(path)?;
            let _ = std::fs::remove_file(&to);
            symlink(&target, &to)?;
        } else if kind.is_file() {
            std::fs::copy(path, &to).with_context(|| format!("copying {}", path.display()))?;
            files.insert(rel);
        }
    }
    Ok(())
}

// a directory's installed packages, reachable from the copy - node_modules entry by entry so the copy can add its own
fn link_installed(dir: &Path, to: &Path) -> Result<()> {
    for name in LINKED_DIRS {
        let from = dir.join(name);
        if from.is_dir() && !to.join(name).exists() {
            symlink(&from, &to.join(name))?;
        }
    }
    let modules = dir.join("node_modules");
    if !modules.is_dir() {
        return Ok(());
    }
    let into = to.join("node_modules");
    std::fs::create_dir_all(&into)?;
    for entry in std::fs::read_dir(&modules)? {
        let entry = entry?;
        let link = into.join(entry.file_name());
        if !link.exists() && link.symlink_metadata().is_err() {
            symlink(&entry.path(), &link)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link).with_context(|| format!("linking {}", link.display()))
}

#[cfg(not(unix))]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    if target.is_dir() {
        return Ok(());
    }
    std::fs::copy(target, link).map(|_| ()).with_context(|| format!("copying {}", target.display()))
}

// an error node somewhere below - a token the parser only had to assume, like `TEST_CASE("x") {`'s missing `;`, is not one
fn broken(node: Node) -> bool {
    if !node.has_error() {
        return false;
    }
    if node.is_error() {
        return true;
    }
    let mut cursor = node.walk();
    let found = node.children(&mut cursor).any(broken);
    found
}

// parse one copied file for the rewrite - `None` when there is nothing a probe could go into
fn source(cx: &Cx, rel: &str, lang: Language) -> Result<Option<Source>> {
    let Ok(text) = std::fs::read_to_string(cx.out.join(rel)) else {
        return Ok(None);
    };
    if generated(rel, &text) {
        return Ok(None);
    }
    let Some(extracted) = extract::extract(lang, &text) else {
        bail!("withdrawn by `ccc:skip`, or not parseable");
    };
    let mut parser = Parser::new();
    parser.set_language(&lang.ts_language())?;
    let tree = parser.parse(&text, None).context("not parseable")?;
    if broken(tree.root_node()) {
        bail!("has syntax the parser cannot read, so no probe goes in");
    }
    let path = cx.record_path(rel);
    Ok(Some(Source {
        test: is_test_path(&path),
        kind: cx.kind(rel),
        rel: rel.to_string(),
        path,
        lang,
        text,
        tree,
        funcs: extracted.funcs,
        imports: extracted.imports,
    }))
}

pub fn probe(opts: &Options) -> Result<Report> {
    let src = opts.src.canonicalize().with_context(|| format!("reading {}", opts.src.display()))?;
    if !src.is_dir() {
        bail!("{} is not a directory", src.display());
    }
    let out = if opts.out.is_absolute() { opts.out.clone() } else { std::env::current_dir()?.join(&opts.out) };
    if out.exists() {
        let out = out.canonicalize()?;
        if out == src || src.starts_with(&out) {
            bail!("--out must be somewhere other than {} and the directories above it", src.display());
        }
        if std::fs::read_dir(&out)?.next().is_some() && !opts.force {
            bail!("{} already holds files - pass --force to write over them", out.display());
        }
    }
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let out = out.canonicalize()?;

    if opts.targets.is_empty() {
        bail!("name a language to probe - go, rust, cpp, typescript or python");
    }
    let (prefix, root) = repo_prefix(&src, opts.root.as_deref())?;
    let mut cx = Cx {
        target: opts.targets[0],
        out: out.clone(),
        prefix,
        commit: root_commit(root.as_deref()),
        files: BTreeSet::new(),
        rules: parse_rules(&opts.kinds)?,
    };
    let mut files = BTreeSet::new();
    copy_tree(&src, &out, &mut files)?;
    cx.files = files;

    let mut report = Report {
        targets: opts.targets.iter().map(|t| t.label()).collect(),
        src: path_str(&src),
        out: path_str(&out),
        prefix: cx.prefix.clone(),
        commit: cx.commit.clone(),
        copied: cx.files.len(),
        ..Report::default()
    };
    for &target in &opts.targets {
        cx.target = target;
        probe_target(&cx, &mut report)?;
    }
    report.next = record_steps(&cx, &report.run);
    Ok(report)
}

// one language's probes into the copy - its build files read, its sources rewritten, its runtime written
fn probe_target(cx: &Cx, report: &mut Report) -> Result<()> {
    let mut prober: Box<dyn Prober> = match cx.target {
        Target::Go => Box::new(go::Go::default()),
        Target::Rust => Box::new(rust::Rust::default()),
        Target::Cpp => Box::new(cpp::Cpp::default()),
        Target::TypeScript => Box::new(typescript::TypeScript),
        Target::Python => Box::new(python::Python::default()),
    };
    prober.prepare(cx)?;
    let candidates: Vec<(String, Language)> = cx
        .files
        .iter()
        .filter_map(|rel| {
            let lang = Language::from_path(Path::new(rel)).filter(|l| cx.target.handles(*l))?;
            let len = std::fs::metadata(cx.out.join(rel)).map(|m| m.len()).unwrap_or(u64::MAX);
            mapped(rel, len).then(|| (rel.clone(), lang))
        })
        .collect();
    for (rel, lang) in candidates {
        let file = match source(cx, &rel, lang) {
            Ok(Some(file)) => file,
            Ok(None) => continue,
            Err(e) => {
                report.skipped.push(format!("{rel}: {e:#}"));
                continue;
            }
        };
        match prober.rewrite(cx, &file) {
            Ok(rewrite) if !rewrite.edits.is_empty() => {
                report.functions += rewrite.functions;
                report.fixtures += rewrite.fixtures;
                report.wrapped += rewrite.wrapped;
                report.files += 1;
                cx.write(&rel, &rewrite.edits.apply(&file.text))?;
            }
            Ok(_) => {}
            Err(e) => report.skipped.push(format!("{rel}: {e:#}")),
        }
    }
    prober.finish(cx, report)
}

impl Report {
    pub fn describe(&self) -> String {
        let mut out = format!(
            "ccc probe {}: {} functions count their runs and {} tests name themselves fixtures, in {} files - {} servers, clients and spawns carry the fixture\ncopied {} files from {} into {}\n",
            self.targets.join(","),
            self.functions,
            self.fixtures,
            self.files,
            self.wrapped,
            self.copied,
            self.src,
            self.out
        );
        if !self.prefix.is_empty() {
            out.push_str(&format!("records name files from the repository root - under {}\n", self.prefix.trim_end_matches('/')));
        }
        for (title, rows) in [("runtime", &self.runtime), ("edited", &self.edited), ("left as they were", &self.skipped)] {
            if rows.is_empty() {
                continue;
            }
            out.push_str(&format!("{title}:\n"));
            for row in rows {
                out.push_str(&format!("  {row}\n"));
            }
        }
        if !self.next.is_empty() {
            out.push_str("to record:\n");
            for row in &self.next {
                out.push_str(&format!("  {row}\n"));
            }
        }
        out
    }
}

// the shared "how to record" lines - each language adds how its own suites run
pub(crate) fn record_steps(cx: &Cx, run: &[String]) -> Vec<String> {
    let out = path_str(&cx.out);
    let mut steps = vec![format!("export CCC_OBSERVED_DIR=$PWD/observed   # nothing is recorded without it")];
    steps.extend(run.iter().map(|r| format!("(cd {out} && {r})")));
    steps.push("start each instrumented service with CCC_SERVICE=<name> and let it exit cleanly, or give it two seconds to flush".to_string());
    steps.push("cat observed/*.jsonl > baseline.jsonl - then point `observed` in .ccc/map.json at it, or pass --observed".to_string());
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_names_its_kind_by_its_own_name_then_its_innermost_directory() {
        assert_eq!(default_kind("services/storage/storage_test.go", Target::Go), "unit");
        assert_eq!(default_kind("integration/stack_test.go", Target::Go), "integration");
        assert_eq!(default_kind("qa/contract/test_shapes.py", Target::Python), "contract");
        assert_eq!(default_kind("qa/load/test_throughput.py", Target::Python), "load");
        assert_eq!(default_kind("tests/load_tests/checkout.spec.ts", Target::TypeScript), "load");
        assert_eq!(default_kind("web/checkout.e2e.spec.ts", Target::TypeScript), "integration");
        assert_eq!(default_kind("src/billing/smoke_test.go", Target::Go), "smoke");
        // `load` names a kind only as a directory - a file about loading config is a unit test
        assert_eq!(default_kind("config/load_config_test.go", Target::Go), "unit");
        assert_eq!(default_kind("payload/encode_test.go", Target::Go), "unit");
        assert_eq!(default_kind("billing/tests/refunds.rs", Target::Rust), "integration");
        assert_eq!(default_kind("billing/tests/refunds.py", Target::Python), "unit");
    }

    #[test]
    fn the_prefix_names_the_directory_inside_the_nearest_project() {
        let dir = std::env::temp_dir().join(format!("ccc-probe-root-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("svc/billing/.ccc")).unwrap();
        std::fs::create_dir_all(dir.join("svc/billing/api")).unwrap();
        let api = dir.join("svc/billing/api").canonicalize().unwrap();
        // the nearest ccc project wins over anything further up
        assert_eq!(repo_prefix(&api, None).unwrap().0, "api/");
        assert_eq!(repo_prefix(&api, Some(&dir)).unwrap().0, "svc/billing/api/");
        // no repository at that root, so no commit to stamp
        assert_eq!(root_commit(repo_prefix(&api, None).unwrap().1.as_deref()), None);
        assert!(repo_prefix(&dir, Some(&api)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_kind_rule_overrides_the_path() {
        let rules = parse_rules(&["qa/**=smoke".to_string()]).unwrap();
        let cx = Cx { target: Target::Python, out: PathBuf::new(), prefix: String::new(), commit: None, files: BTreeSet::new(), rules };
        assert_eq!(cx.kind("qa/integration/test_flow.py"), "smoke");
        assert_eq!(cx.kind("tests/integration/test_flow.py"), "integration");
        assert!(parse_rules(&["qa/**=nightly".to_string()]).is_err());
        assert!(parse_rules(&["qa/**".to_string()]).is_err());
    }

    #[test]
    fn edits_nest_where_wraps_share_an_offset() {
        let mut edits = Edits::default();
        edits.insert(0, "outer(");
        edits.insert(0, "inner(");
        edits.close(1, ")");
        edits.close(1, "]");
        edits.insert(1, "+");
        assert_eq!(edits.apply("x"), "outer(inner(x])+");
    }

    #[test]
    fn a_replacement_swaps_its_node_and_leaves_the_rest() {
        let src = "grpc::CreateChannel(a, b);";
        let mut parser = Parser::new();
        parser.set_language(&Language::Cpp.ts_language()).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let mut callee = None;
        collect(tree.root_node(), &mut |n| {
            if n.kind() == "qualified_identifier" && callee.is_none() {
                callee = Some(n);
            }
        });
        let mut edits = Edits::default();
        edits.replace(callee.unwrap(), "::ccc_probe::grpc_channel");
        edits.insert(src.len(), " // probed");
        assert_eq!(edits.apply(src), "::ccc_probe::grpc_channel(a, b); // probed");
    }

    #[test]
    fn a_missing_token_is_not_a_parse_error_but_an_error_node_is() {
        let mut parser = Parser::new();
        parser.set_language(&Language::Cpp.ts_language()).unwrap();
        let catch2 = parser.parse("TEST_CASE(\"prices\") { REQUIRE(true); }\n", None).unwrap();
        assert!(catch2.root_node().has_error() && !broken(catch2.root_node()));
        let garbage = parser.parse("int f( { ] }\n", None).unwrap();
        assert!(broken(garbage.root_node()));
    }

    #[test]
    fn relative_paths_climb_out_of_the_shared_prefix() {
        assert_eq!(relative("services/storage", ".ccc-probe/go"), "../../.ccc-probe/go");
        assert_eq!(relative("", ".ccc-probe/go"), ".ccc-probe/go");
        assert_eq!(relative("a/b", "a/c"), "../c");
        assert_eq!(relative("a", "a"), ".");
    }

    #[test]
    fn a_quoted_name_escapes_what_every_language_would_end_the_string_on() {
        assert_eq!(quote(r#"it("a \ b")"#), r#""it(\"a \\ b\")""#);
    }

    #[test]
    fn the_map_skips_what_the_probes_skip() {
        assert!(mapped("services/storage/storage.go", 10));
        assert!(!mapped("vendor/x/y.go", 10));
        assert!(!mapped(".ccc-probe/go/cccprobe.go", 10));
        assert!(!mapped("node_modules/a/index.ts", 10));
        assert!(!mapped("big.go", MAX_FILE_BYTES + 1));
    }
}
