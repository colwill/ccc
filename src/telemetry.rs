//! `ccc changes` telemetry - what this branch did to the OpenTelemetry metrics
//! the source defines.
//!
//! A metric name is a contract with everything downstream of the process: a
//! dashboard, an alert, a recording rule, an SLO. None of that lives in this
//! repository, and none of it moves when the instrument feeding it is renamed,
//! re-based into another unit, or deleted - the panel just goes quiet, and the
//! alert that was watching it never fires again. So the question here is not
//! "did an instrumentation file change", which the change set already answers
//! and which is mostly noise. It is: which metric names entered the tree, which
//! left, and which of the ones that stayed changed the shape of what they emit.
//!
//! Both sides are collected by the same rule, out of a committed tree rather
//! than off disk unless `--worktree` asks otherwise, so the committed view a CI
//! run wants cannot be contaminated by a dirty working copy. A file is parsed
//! only when it references OpenTelemetry, which is both what keeps the pass
//! cheap on a project that has none and what keeps a project's own unrelated
//! `createCounter` out of the report.
//!
//! What this does not do is follow an instrument to the sites that record on
//! it, so the attribute keys a metric carries are not part of the comparison.
//! That needs binding-to-call-site dataflow this pass deliberately does not
//! carry - the same line `sast` draws. A metric whose attributes changed and
//! whose name, instrument, unit and type did not reads here as unchanged.

use crate::changes::{is_test_fn_name, is_test_path};
use crate::languages::Language;
use crate::sast::{is_string_kind, rightmost, unquote};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use tree_sitter::{Node, Parser};

pub const SCHEMA: &str = "ccc-telemetry/1";

// A file earns a parse by naming the API it would be instrumenting with. This
// is the whole cost control of the pass - and it is a correctness rule too,
// because `createCounter` is not a reserved word and a project is entitled to
// its own.
const MARKERS: &[&str] = &["opentelemetry", "System.Diagnostics.Metrics"];

// directory names whose contents describe somebody else's instrumentation
const VENDORED: &[&str] = &[
    "node_modules",
    "vendor",
    "third_party",
    "target",
    "dist",
    "build",
    ".git",
];

// a fluent chain is the statement, and a statement is not a whole function
const MAX_STATEMENT: usize = 600;
const STATEMENT_DEPTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Instrument {
    Counter,
    UpDownCounter,
    Histogram,
    Gauge,
    ObservableCounter,
    ObservableUpDownCounter,
    ObservableGauge,
}

impl Instrument {
    pub fn label(&self) -> &'static str {
        match self {
            Instrument::Counter => "counter",
            Instrument::UpDownCounter => "up-down-counter",
            Instrument::Histogram => "histogram",
            Instrument::Gauge => "gauge",
            Instrument::ObservableCounter => "observable-counter",
            Instrument::ObservableUpDownCounter => "observable-up-down-counter",
            Instrument::ObservableGauge => "observable-gauge",
        }
    }
}

// where an instrument is created
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Site {
    pub file: String,
    pub line: usize,
    pub function: String,
    pub language: String,
}

// one metric, as one side of the branch defines it
#[derive(Debug, Clone, Serialize)]
pub struct Metric {
    pub name: String,
    pub instrument: Instrument,
    // as the API spells it - `u64`, `Int64`, the C# generic argument - and
    // empty where the binding does not encode a type at all
    pub value_type: String,
    pub unit: String,
    pub description: String,
    // every place this name is created, lowest file and line first
    pub sites: Vec<Site>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetricChangeKind {
    Added,
    Removed,
    Renamed,
    InstrumentChanged,
    TypeChanged,
    UnitChanged,
    DescriptionChanged,
}

impl MetricChangeKind {
    pub fn label(&self) -> &'static str {
        match self {
            MetricChangeKind::Added => "added",
            MetricChangeKind::Removed => "removed",
            MetricChangeKind::Renamed => "renamed",
            MetricChangeKind::InstrumentChanged => "instrument-changed",
            MetricChangeKind::TypeChanged => "type-changed",
            MetricChangeKind::UnitChanged => "unit-changed",
            MetricChangeKind::DescriptionChanged => "description-changed",
        }
    }

    // the marker the text report draws it with
    fn marker(&self) -> char {
        match self {
            MetricChangeKind::Added => '+',
            MetricChangeKind::Removed => '-',
            _ => '~',
        }
    }

    // Whether a query written against the base still returns the same series.
    // A new metric breaks nothing; every other kind here either takes a name
    // away or silently changes what the numbers under it mean. A reworded
    // description is the one property nothing downstream is keyed on.
    fn breaking(&self) -> bool {
        !matches!(
            self,
            MetricChangeKind::Added | MetricChangeKind::DescriptionChanged
        )
    }

    // report order: what a reader acts on first
    fn rank(&self) -> u8 {
        match self {
            MetricChangeKind::Removed => 0,
            MetricChangeKind::Renamed => 1,
            MetricChangeKind::InstrumentChanged => 2,
            MetricChangeKind::TypeChanged => 3,
            MetricChangeKind::UnitChanged => 4,
            MetricChangeKind::Added => 5,
            MetricChangeKind::DescriptionChanged => 6,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricChange {
    // The most severe property that moved. A metric can change its unit and its
    // description in one commit; `detail` carries every clause, and this names
    // the one worth reacting to.
    pub kind: MetricChangeKind,
    pub name: String,
    // the name at the base, set only on a rename
    #[serde(skip_serializing_if = "String::is_empty")]
    pub was: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<Metric>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<Metric>,
    // one clause per property that moved, in the report's own words
    pub detail: Vec<String>,
    // a query written against the base does not survive this change
    pub breaking: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct TelemetryCounts {
    // metrics the head side defines, which is the surface being emitted now
    pub metrics: usize,
    pub added: usize,
    pub removed: usize,
    pub renamed: usize,
    // metrics that kept their name and changed a property
    pub modified: usize,
    pub breaking: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TelemetryReport {
    pub schema: &'static str,
    pub base_sha: String,
    pub head_sha: String,
    // neither side creates an instrument, so everything below is empty and the
    // project is simply not instrumented with OpenTelemetry
    pub instrumented: bool,
    pub changes: Vec<MetricChange>,
    // the whole head-side surface, not just what moved
    pub metrics: Vec<Metric>,
    // set when a side could not be read; the rest of the report is then empty
    // rather than wrong, because half a collection reads as deletions
    pub error: Option<String>,
    pub counts: TelemetryCounts,
}

impl TelemetryReport {
    fn empty(base_sha: &str, head_sha: &str, error: Option<String>) -> TelemetryReport {
        TelemetryReport {
            schema: SCHEMA,
            base_sha: base_sha.to_string(),
            head_sha: head_sha.to_string(),
            instrumented: false,
            changes: Vec::new(),
            metrics: Vec::new(),
            error,
            counts: TelemetryCounts::default(),
        }
    }

    // whether a gate should fail: a query written against the base broke, or a
    // side could not be read - "we could not look" is not "nothing moved"
    pub fn gates(&self) -> bool {
        self.counts.breaking > 0 || self.error.is_some()
    }
}

pub struct TelemetryOptions<'a> {
    pub base_sha: &'a str,
    pub head_sha: &'a str,
    // the head side is the working tree rather than the committed head
    pub worktree: bool,
}

// ---------------------------------------------------------------------------
// the instruments
// ---------------------------------------------------------------------------

// The instrument constructors the OpenTelemetry metric APIs expose, keyed on
// the method name exactly as each binding spells it, with the value type that
// name encodes. Matched whole rather than by prefix or suffix: `Int64Counter`
// is a substring of `Int64UpDownCounter` at one end and of nothing at the
// other, and a loose rule folds the two together.
const INSTRUMENTS: &[(&str, Instrument, &str)] = &[
    // rust - opentelemetry::metrics::Meter
    ("u64_counter", Instrument::Counter, "u64"),
    ("f64_counter", Instrument::Counter, "f64"),
    ("u64_observable_counter", Instrument::ObservableCounter, "u64"),
    ("f64_observable_counter", Instrument::ObservableCounter, "f64"),
    ("i64_up_down_counter", Instrument::UpDownCounter, "i64"),
    ("f64_up_down_counter", Instrument::UpDownCounter, "f64"),
    (
        "i64_observable_up_down_counter",
        Instrument::ObservableUpDownCounter,
        "i64",
    ),
    (
        "f64_observable_up_down_counter",
        Instrument::ObservableUpDownCounter,
        "f64",
    ),
    ("u64_histogram", Instrument::Histogram, "u64"),
    ("f64_histogram", Instrument::Histogram, "f64"),
    ("u64_gauge", Instrument::Gauge, "u64"),
    ("i64_gauge", Instrument::Gauge, "i64"),
    ("f64_gauge", Instrument::Gauge, "f64"),
    ("u64_observable_gauge", Instrument::ObservableGauge, "u64"),
    ("i64_observable_gauge", Instrument::ObservableGauge, "i64"),
    ("f64_observable_gauge", Instrument::ObservableGauge, "f64"),
    // go - go.opentelemetry.io/otel/metric
    ("Int64Counter", Instrument::Counter, "int64"),
    ("Float64Counter", Instrument::Counter, "float64"),
    ("Int64UpDownCounter", Instrument::UpDownCounter, "int64"),
    ("Float64UpDownCounter", Instrument::UpDownCounter, "float64"),
    ("Int64Histogram", Instrument::Histogram, "int64"),
    ("Float64Histogram", Instrument::Histogram, "float64"),
    ("Int64Gauge", Instrument::Gauge, "int64"),
    ("Float64Gauge", Instrument::Gauge, "float64"),
    ("Int64ObservableCounter", Instrument::ObservableCounter, "int64"),
    (
        "Float64ObservableCounter",
        Instrument::ObservableCounter,
        "float64",
    ),
    (
        "Int64ObservableUpDownCounter",
        Instrument::ObservableUpDownCounter,
        "int64",
    ),
    (
        "Float64ObservableUpDownCounter",
        Instrument::ObservableUpDownCounter,
        "float64",
    ),
    ("Int64ObservableGauge", Instrument::ObservableGauge, "int64"),
    ("Float64ObservableGauge", Instrument::ObservableGauge, "float64"),
    // python - opentelemetry.metrics.Meter
    ("create_counter", Instrument::Counter, ""),
    ("create_up_down_counter", Instrument::UpDownCounter, ""),
    ("create_histogram", Instrument::Histogram, ""),
    ("create_gauge", Instrument::Gauge, ""),
    ("create_observable_counter", Instrument::ObservableCounter, ""),
    (
        "create_observable_up_down_counter",
        Instrument::ObservableUpDownCounter,
        "",
    ),
    ("create_observable_gauge", Instrument::ObservableGauge, ""),
    // javascript / typescript - @opentelemetry/api
    ("createCounter", Instrument::Counter, ""),
    ("createUpDownCounter", Instrument::UpDownCounter, ""),
    ("createHistogram", Instrument::Histogram, ""),
    ("createGauge", Instrument::Gauge, ""),
    ("createObservableCounter", Instrument::ObservableCounter, ""),
    (
        "createObservableUpDownCounter",
        Instrument::ObservableUpDownCounter,
        "",
    ),
    ("createObservableGauge", Instrument::ObservableGauge, ""),
    // c# - System.Diagnostics.Metrics.Meter, which is what OpenTelemetry .NET
    // collects from. The value type is a generic argument, read separately
    ("CreateCounter", Instrument::Counter, ""),
    ("CreateUpDownCounter", Instrument::UpDownCounter, ""),
    ("CreateHistogram", Instrument::Histogram, ""),
    ("CreateGauge", Instrument::Gauge, ""),
    ("CreateObservableCounter", Instrument::ObservableCounter, ""),
    (
        "CreateObservableUpDownCounter",
        Instrument::ObservableUpDownCounter,
        "",
    ),
    ("CreateObservableGauge", Instrument::ObservableGauge, ""),
    // c++ - opentelemetry::metrics::Meter, where the type is in the name
    ("CreateUInt64Counter", Instrument::Counter, "uint64"),
    ("CreateDoubleCounter", Instrument::Counter, "double"),
    ("CreateInt64UpDownCounter", Instrument::UpDownCounter, "int64"),
    ("CreateDoubleUpDownCounter", Instrument::UpDownCounter, "double"),
    ("CreateUInt64Histogram", Instrument::Histogram, "uint64"),
    ("CreateDoubleHistogram", Instrument::Histogram, "double"),
    ("CreateInt64Gauge", Instrument::Gauge, "int64"),
    ("CreateDoubleGauge", Instrument::Gauge, "double"),
    (
        "CreateInt64ObservableCounter",
        Instrument::ObservableCounter,
        "int64",
    ),
    (
        "CreateDoubleObservableCounter",
        Instrument::ObservableCounter,
        "double",
    ),
    (
        "CreateInt64ObservableUpDownCounter",
        Instrument::ObservableUpDownCounter,
        "int64",
    ),
    (
        "CreateDoubleObservableUpDownCounter",
        Instrument::ObservableUpDownCounter,
        "double",
    ),
    (
        "CreateInt64ObservableGauge",
        Instrument::ObservableGauge,
        "int64",
    ),
    (
        "CreateDoubleObservableGauge",
        Instrument::ObservableGauge,
        "double",
    ),
];

fn instrument_of(method: &str) -> Option<(Instrument, &'static str)> {
    INSTRUMENTS
        .iter()
        .find(|(m, _, _)| *m == method)
        .map(|(_, i, t)| (*i, *t))
}

// The two bindings that take the unit and the description by position, and
// disagree about the order. Everywhere else they are named, so nothing is read
// out of position there.
fn positional(lang: Language) -> Option<(usize, usize)> {
    match lang {
        // CreateCounter<T>(name, unit, description)
        Language::CSharp => Some((1, 2)),
        // CreateUInt64Counter(name, description, unit)
        Language::Cpp | Language::C => Some((2, 1)),
        _ => None,
    }
}

pub fn analyse(root: &Path, opts: &TelemetryOptions) -> TelemetryReport {
    let prefix = git_prefix(root);
    let base = match collect(root, Side::Commit(opts.base_sha), &prefix) {
        Ok(m) => m,
        Err(e) => return TelemetryReport::empty(opts.base_sha, opts.head_sha, Some(e)),
    };
    let head_side = if opts.worktree {
        Side::Worktree
    } else {
        Side::Commit(opts.head_sha)
    };
    let head = match collect(root, head_side, &prefix) {
        Ok(m) => m,
        Err(e) => return TelemetryReport::empty(opts.base_sha, opts.head_sha, Some(e)),
    };

    let changes = diff(&base, &head);
    let mut counts = TelemetryCounts {
        metrics: head.len(),
        ..TelemetryCounts::default()
    };
    for c in &changes {
        match c.kind {
            MetricChangeKind::Added => counts.added += 1,
            MetricChangeKind::Removed => counts.removed += 1,
            MetricChangeKind::Renamed => counts.renamed += 1,
            _ => counts.modified += 1,
        }
        if c.breaking {
            counts.breaking += 1;
        }
    }

    TelemetryReport {
        schema: SCHEMA,
        base_sha: opts.base_sha.to_string(),
        head_sha: opts.head_sha.to_string(),
        instrumented: !base.is_empty() || !head.is_empty(),
        changes,
        metrics: head.into_values().collect(),
        error: None,
        counts,
    }
}

// one side of the branch, and where it reads its source from
#[derive(Clone, Copy)]
enum Side<'a> {
    // the working tree, uncommitted edits and untracked files included
    Worktree,
    // a committed tree, read with `git show <sha>:<rel>`
    Commit(&'a str),
}

// Every metric one side defines, keyed by name. Both sides go through this same
// function - a difference in how the two are collected would show up as a
// change the branch never made.
fn collect(root: &Path, side: Side, prefix: &str) -> Result<BTreeMap<String, Metric>, String> {
    let mut args: Vec<&str> = vec!["grep", "-l", "-z", "-I", "-i", "--fixed-strings"];
    if matches!(side, Side::Worktree) {
        args.push("--untracked");
    }
    for m in MARKERS {
        args.push("-e");
        args.push(m);
    }
    if let Side::Commit(sha) = side {
        args.push(sha);
    }
    args.push("--");

    // searching a tree prints `<rev>:<path>`; searching the working tree prints
    // the path alone. Either way the path is relative to `root`, because git
    // resolves it against the directory it was run in
    let strip = match side {
        Side::Commit(sha) => format!("{sha}:"),
        Side::Worktree => String::new(),
    };
    let mut candidates: Vec<String> = git_grep(root, &args)?
        .into_iter()
        .map(|line| line.strip_prefix(&strip).unwrap_or(&line).to_string())
        .filter(|rel| !rel.split('/').any(|seg| VENDORED.contains(&seg)))
        // a metric defined by a test is a fixture, not something a dashboard reads
        .filter(|rel| !is_test_path(rel))
        .collect();
    candidates.sort();
    candidates.dedup();

    let mut out: BTreeMap<String, Metric> = BTreeMap::new();
    for rel in &candidates {
        let Some(lang) = Language::from_path(Path::new(rel)) else {
            continue;
        };
        let src = match side {
            Side::Worktree => std::fs::read_to_string(root.join(rel)).ok(),
            Side::Commit(sha) => crate::audit::git_out(root, &["show", &format!("{sha}:{prefix}{rel}")]),
        };
        let Some(src) = src else {
            continue;
        };
        for m in collect_file(lang, rel, &src) {
            merge(&mut out, m);
        }
    }
    Ok(out)
}

// The same name created in two places is one metric with two definition sites,
// not two metrics. The facts come from the site that sorts first, so the two
// sides of a branch cannot disagree about which one spoke.
fn merge(out: &mut BTreeMap<String, Metric>, m: Metric) {
    match out.get_mut(&m.name) {
        Some(existing) => {
            existing.sites.extend(m.sites);
            existing.sites.sort();
            existing.sites.dedup();
        }
        None => {
            out.insert(m.name.clone(), m);
        }
    }
}

fn collect_file(lang: Language, rel: &str, src: &str) -> Vec<Metric> {
    let mut parser = Parser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(src, None) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    walk(tree.root_node(), src, lang, rel, "<top>", false, &mut out);
    out
}

fn walk(
    node: Node,
    src: &str,
    lang: Language,
    rel: &str,
    function: &str,
    in_test: bool,
    out: &mut Vec<Metric>,
) {
    let kind = node.kind();

    // entering a function renames the scope every site below it reports
    let mut scope = function.to_string();
    let mut test_scope = in_test;
    if lang.func_kinds().contains(&kind) {
        if let Some(name) = node.child_by_field_name("name").and_then(|n| text_of(n, src)) {
            test_scope = test_scope || is_test_fn_name(&name);
            scope = name;
        }
    }
    if lang.module_kinds().contains(&kind) {
        if let Some(name) = node.child_by_field_name("name").and_then(|n| text_of(n, src)) {
            let n = name.to_ascii_lowercase();
            test_scope = test_scope || n == "tests" || n == "test";
        }
    }

    if !test_scope && lang.call_kinds().contains(&kind) {
        if let Some(m) = creation(node, src, lang, rel, &scope) {
            out.push(m);
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, src, lang, rel, &scope, test_scope, out);
    }
}

// One call node, if it creates an instrument. The metric's name is its first
// literal argument: every binding takes it there, and a name assembled at
// runtime is not a name this pass can report, so it is skipped rather than
// guessed at.
fn creation(node: Node, src: &str, lang: Language, rel: &str, scope: &str) -> Option<Metric> {
    let callee = node
        .child_by_field_name("function")
        .and_then(|n| text_of(n, src))
        .or_else(|| node.child(0).and_then(|n| text_of(n, src)))
        .unwrap_or_default();
    let method = rightmost(&callee);
    // `CreateCounter<long>` is the method plus the value type it was asked for
    let (method, generic) = match method.split_once('<') {
        Some((m, g)) => (m, g.trim_end_matches('>').trim().to_string()),
        None => (method, String::new()),
    };
    let (instrument, typed) = instrument_of(method)?;

    let args = string_args(node, src);
    let name = args.iter().flatten().next()?.clone();
    if name.is_empty() {
        return None;
    }

    let stmt = statement_of(node, src, lang);
    let mut unit = scrape(&stmt, UNIT_KEYS);
    let mut description = scrape(&stmt, DESCRIPTION_KEYS);
    if let Some((u, d)) = positional(lang) {
        if unit.is_empty() {
            unit = args.get(u).cloned().flatten().unwrap_or_default();
        }
        if description.is_empty() {
            description = args.get(d).cloned().flatten().unwrap_or_default();
        }
    }

    Some(Metric {
        name,
        instrument,
        value_type: if typed.is_empty() {
            generic
        } else {
            typed.to_string()
        },
        unit,
        description,
        sites: vec![Site {
            file: rel.to_string(),
            line: node.start_position().row + 1,
            function: scope.to_string(),
            language: lang.as_str().to_string(),
        }],
    })
}

// The call's arguments in order, each one the literal it is or None for
// anything else, so a position can be read without losing count of it.
fn string_args(node: Node, src: &str) -> Vec<Option<String>> {
    let mut find = node.walk();
    let list = node.child_by_field_name("arguments").or_else(|| {
        node.children(&mut find)
            .find(|c| c.kind().contains("argument"))
    });
    let Some(list) = list else {
        return Vec::new();
    };
    let mut cursor = list.walk();
    list.named_children(&mut cursor).map(|c| literal_of(c, src)).collect()
}

// The literal an argument is, if it is one. C# wraps every argument in an
// `argument` node and other grammars parenthesise, so a wrapper carrying one
// child is stepped through - but only a wrapper. A node with two children is a
// named argument or a call, and descending into it would read a value out of a
// position it was never in.
fn literal_of(node: Node, src: &str) -> Option<String> {
    let mut cur = node;
    for _ in 0..2 {
        if is_string_kind(cur.kind()) {
            return Some(unquote(&text_of(cur, src)?));
        }
        if cur.named_child_count() != 1 {
            return None;
        }
        cur = cur.named_child(0)?;
    }
    None
}

// The statement a creation sits in, which is where a fluent API leaves the unit
// and the description: `meter.f64_histogram("x").with_unit("ms").build()`. Walk
// up through the expression nodes the chain is made of, and stop before the
// block or the function that would swallow the whole body.
fn statement_of(node: Node, src: &str, lang: Language) -> String {
    let mut best = node;
    let mut cur = node;
    for _ in 0..STATEMENT_DEPTH {
        let Some(parent) = cur.parent() else { break };
        let k = parent.kind();
        if is_body_kind(k) || lang.func_kinds().contains(&k) {
            break;
        }
        best = parent;
        cur = parent;
        if k.ends_with("_statement") || k.ends_with("_declaration") {
            break;
        }
    }
    truncate(&text_of(best, src).unwrap_or_default(), MAX_STATEMENT)
}

fn is_body_kind(kind: &str) -> bool {
    matches!(
        kind,
        "block"
            | "statement_block"
            | "compound_statement"
            | "declaration_list"
            | "field_declaration_list"
            | "source_file"
            | "program"
            | "module"
            | "translation_unit"
    )
}

// Every binding puts the unit and the description behind a different spelling -
// a fluent `.with_unit("ms")`, a Go option `metric.WithUnit("ms")`, a Python
// keyword `unit="ms"`, a JS object key. All of them put the value in a literal
// immediately after the word, so this finds the word and reads the next
// literal, rather than teaching the pass six argument grammars.
const UNIT_KEYS: &[&str] = &[
    "with_unit(",
    "withunit(",
    "setunit(",
    "unit=",
    "unit =",
    "unit:",
    "unit :",
    "\"unit\"",
    "'unit'",
];

const DESCRIPTION_KEYS: &[&str] = &[
    "with_description(",
    "withdescription(",
    "setdescription(",
    "description=",
    "description =",
    "description:",
    "description :",
    "\"description\"",
    "'description'",
];

fn scrape(stmt: &str, keys: &[&str]) -> String {
    let low = stmt.to_ascii_lowercase();
    let at = keys
        .iter()
        .filter_map(|k| find_word(&low, k).map(|i| i + k.len()))
        .min();
    let Some(at) = at else {
        return String::new();
    };
    // The value has to be the very next token. A unit passed as a constant -
    // `.with_unit(MILLIS)` - leaves the next literal in the chain belonging to
    // something else entirely, and reporting that as the unit is worse than
    // reporting no unit at all.
    let mut rest = stmt[at..].trim_start();
    while let Some(t) = rest.strip_prefix([':', '=']) {
        rest = t.trim_start();
    }
    let mut chars = rest.chars();
    let Some(quote) = chars.next().filter(|c| matches!(c, '"' | '\'' | '`')) else {
        return String::new();
    };
    let body = chars.as_str();
    match body.find(quote) {
        Some(end) => body[..end].to_string(),
        None => String::new(),
    }
}

// `unit=` must not match inside `runit=`, and `description:` must not be found
// by way of a longer word ending in it
fn find_word(hay: &str, needle: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = hay[from..].find(needle) {
        let at = from + i;
        let before = hay[..at].chars().next_back();
        if !before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

fn text_of(node: Node, src: &str) -> Option<String> {
    src.get(node.byte_range()).map(|s| s.to_string())
}

fn truncate(s: &str, max: usize) -> String {
    let one_line = s.replace(['\n', '\r'], " ");
    if one_line.len() <= max {
        return one_line;
    }
    one_line.chars().take(max).collect()
}

fn diff(base: &BTreeMap<String, Metric>, head: &BTreeMap<String, Metric>) -> Vec<MetricChange> {
    let mut changes = Vec::new();
    let mut added: Vec<&Metric> = Vec::new();
    let mut removed: Vec<&Metric> = Vec::new();

    for (name, h) in head {
        match base.get(name) {
            None => added.push(h),
            Some(b) => {
                let moved = compare(b, h);
                if moved.is_empty() {
                    continue;
                }
                let kind = moved
                    .iter()
                    .map(|(k, _)| *k)
                    .min_by_key(|k| k.rank())
                    .unwrap_or(MetricChangeKind::DescriptionChanged);
                changes.push(MetricChange {
                    kind,
                    name: name.clone(),
                    was: String::new(),
                    from: Some(b.clone()),
                    to: Some(h.clone()),
                    detail: moved.into_iter().map(|(_, d)| d).collect(),
                    breaking: kind.breaking(),
                });
            }
        }
    }
    for (name, b) in base {
        if !head.contains_key(name) {
            removed.push(b);
        }
    }

    changes.extend(pair_renames(&mut added, &mut removed));
    for h in added {
        changes.push(MetricChange {
            kind: MetricChangeKind::Added,
            name: h.name.clone(),
            was: String::new(),
            from: None,
            to: Some(h.clone()),
            detail: Vec::new(),
            breaking: false,
        });
    }
    for b in removed {
        changes.push(MetricChange {
            kind: MetricChangeKind::Removed,
            name: b.name.clone(),
            was: String::new(),
            from: Some(b.clone()),
            to: None,
            detail: Vec::new(),
            breaking: true,
        });
    }

    changes.sort_by(|a, b| {
        b.breaking
            .cmp(&a.breaking)
            .then_with(|| a.kind.rank().cmp(&b.kind.rank()))
            .then_with(|| a.name.cmp(&b.name))
    });
    changes
}

// What moved between two definitions of the same name, one clause per property.
fn compare(b: &Metric, h: &Metric) -> Vec<(MetricChangeKind, String)> {
    let mut out = Vec::new();
    if b.instrument != h.instrument {
        out.push((
            MetricChangeKind::InstrumentChanged,
            format!(
                "instrument {} -> {}",
                b.instrument.label(),
                h.instrument.label()
            ),
        ));
    }
    if b.value_type != h.value_type {
        out.push((
            MetricChangeKind::TypeChanged,
            format!("type {} -> {}", shown(&b.value_type), shown(&h.value_type)),
        ));
    }
    if b.unit != h.unit {
        out.push((
            MetricChangeKind::UnitChanged,
            format!("unit {} -> {}", shown(&b.unit), shown(&h.unit)),
        ));
    }
    if b.description != h.description {
        out.push((
            MetricChangeKind::DescriptionChanged,
            "description reworded".to_string(),
        ));
    }
    out
}

fn shown(s: &str) -> &str {
    if s.is_empty() {
        "(none)"
    } else {
        s
    }
}

// A rename is a removal and an addition that are the same instrument created in
// the same function
fn pair_renames<'a>(
    added: &mut Vec<&'a Metric>,
    removed: &mut Vec<&'a Metric>,
) -> Vec<MetricChange> {
    type Key = (String, String, Instrument, String);
    fn key(m: &Metric) -> Option<Key> {
        let site = m.sites.first()?;
        Some((
            site.file.clone(),
            site.function.clone(),
            m.instrument,
            m.value_type.clone(),
        ))
    }

    let mut groups: BTreeMap<Key, (Vec<&Metric>, Vec<&Metric>)> = BTreeMap::new();
    for m in added.iter() {
        if let Some(k) = key(m) {
            groups.entry(k).or_default().0.push(m);
        }
    }
    for m in removed.iter() {
        if let Some(k) = key(m) {
            groups.entry(k).or_default().1.push(m);
        }
    }

    let mut out = Vec::new();
    let mut paired: Vec<(String, String)> = Vec::new();
    for (ins, del) in groups.into_values() {
        let ([h], [b]) = (&ins[..], &del[..]) else {
            continue;
        };
        let mut detail = vec![format!("name {} -> {}", b.name, h.name)];
        detail.extend(compare(b, h).into_iter().map(|(_, d)| d));
        out.push(MetricChange {
            kind: MetricChangeKind::Renamed,
            name: h.name.clone(),
            was: b.name.clone(),
            from: Some((*b).clone()),
            to: Some((*h).clone()),
            detail,
            breaking: true,
        });
        paired.push((h.name.clone(), b.name.clone()));
    }
    added.retain(|m| !paired.iter().any(|(a, _)| a == &m.name));
    removed.retain(|m| !paired.iter().any(|(_, r)| r == &m.name));
    out
}

fn git_prefix(root: &Path) -> String {
    crate::audit::git_out(root, &["rev-parse", "--show-prefix"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

// `git grep` says "nothing matched" with exit 1, which is not a failure
fn git_grep(root: &Path, args: &[&str]) -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| format!("running git grep: {e}"))?;
    if !matches!(out.status.code(), Some(0) | Some(1)) {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            "git grep failed".to_string()
        } else {
            err
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

fn headline(r: &TelemetryReport, base: &str) -> String {
    if !r.instrumented {
        return "telemetry: no OpenTelemetry instrument is created in this project".to_string();
    }
    if r.changes.is_empty() {
        return format!(
            "telemetry: {} metric(s), unchanged against {base}",
            r.counts.metrics
        );
    }
    let mut parts = Vec::new();
    for (n, label) in [
        (r.counts.added, "added"),
        (r.counts.removed, "removed"),
        (r.counts.renamed, "renamed"),
        (r.counts.modified, "modified"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {label}"));
        }
    }
    format!(
        "telemetry: {} metric(s), {} changed ({}) against {base}",
        r.counts.metrics,
        r.changes.len(),
        parts.join(", ")
    )
}

// where a change happened, head side first because that is where the fix goes
fn site_of(c: &MetricChange) -> String {
    let m = c.to.as_ref().or(c.from.as_ref());
    match m.and_then(|m| m.sites.first()) {
        Some(s) => format!("{}:{}", s.file, s.line),
        None => String::new(),
    }
}

fn instrument_of_change(c: &MetricChange) -> &'static str {
    match c.to.as_ref().or(c.from.as_ref()) {
        Some(m) => m.instrument.label(),
        None => "",
    }
}

// the `changes` text report's voice, appended to it
pub fn text(r: &TelemetryReport, base: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "{}", headline(r, base));
    if let Some(err) = &r.error {
        // an unread side would read as a wholesale deletion, so nothing is
        // reported rather than something wrong
        let _ = writeln!(out, "  not collected - {err}");
        return out;
    }
    if r.changes.is_empty() {
        return out;
    }
    let width = r
        .changes
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0)
        .min(48);
    let iw = r
        .changes
        .iter()
        .map(|c| instrument_of_change(c).len())
        .max()
        .unwrap_or(0);
    for c in &r.changes {
        let detail = if c.detail.is_empty() {
            String::new()
        } else {
            format!("  {}", c.detail.join(", "))
        };
        let _ = writeln!(
            out,
            "  {} {:width$}  {:iw$}  {}{detail}",
            c.kind.marker(),
            c.name,
            instrument_of_change(c),
            site_of(c)
        );
    }
    if r.counts.breaking > 0 {
        let _ = writeln!(
            out,
            "\n{} change(s) break a query written against {base} - a dashboard or an alert on \
             those names goes quiet rather than failing",
            r.counts.breaking
        );
    }
    out
}

// the same answer for an agent, in the tone `deps --markdown` uses
pub fn markdown(r: &TelemetryReport, base: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "# {}", headline(r, base));
    let _ = writeln!(
        out,
        "\nbase {} -> head {}",
        short(&r.base_sha),
        short(&r.head_sha)
    );
    if let Some(err) = &r.error {
        let _ = writeln!(out, "\nnot collected - {err}\n");
        return out;
    }
    if !r.instrumented {
        out.push_str(
            "\nNo file in this project creates an OpenTelemetry instrument, so there is no metric \
             surface to compare. A file is only read when it names the API, so an instrumentation \
             layer this pass cannot see is one that does not mention OpenTelemetry.\n",
        );
        return out;
    }
    if r.changes.is_empty() {
        out.push_str(
            "\nEvery metric this project defines carries the same name, instrument, type and unit \
             it did at the base. Attribute keys are not part of that comparison.\n",
        );
        return out;
    }

    let _ = write!(out, "\n## changes ({})\n", r.changes.len());
    for c in &r.changes {
        let detail = if c.detail.is_empty() {
            String::new()
        } else {
            format!(" - {}", c.detail.join(", "))
        };
        let _ = writeln!(
            out,
            "- {} `{}` ({}){detail} - {}",
            c.kind.label(),
            c.name,
            instrument_of_change(c),
            site_of(c)
        );
    }

    if r.counts.breaking == 0 {
        out.push_str("\nNothing here breaks a query written against the base.\n");
    } else {
        let _ = write!(
            out,
            "\n## breaking ({})\n\nA dashboard, alert or recording rule written against the base \
             will go quiet on these rather than fail loudly:\n",
            r.counts.breaking
        );
        for c in r.changes.iter().filter(|c| c.breaking) {
            let was = if c.was.is_empty() {
                String::new()
            } else {
                format!(" (was `{}`)", c.was)
            };
            let _ = writeln!(out, "- `{}`{was} - {}", c.name, c.kind.label());
        }
    }
    out.push_str(
        "\nThis compares metric names, instruments, value types, units and descriptions. It does \
         not follow an instrument to the sites that record on it, so a change to the attribute \
         keys a metric carries is not reported here.\n",
    );
    out
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(9)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;

    fn one(lang: Language, rel: &str, src: &str) -> Metric {
        let got = collect_file(lang, rel, src);
        assert_eq!(got.len(), 1, "{rel}: expected one metric, got {got:?}");
        got.into_iter().next().expect("checked above")
    }

    fn facts(m: &Metric) -> (String, &'static str, String, String, String) {
        (
            m.name.clone(),
            m.instrument.label(),
            m.value_type.clone(),
            m.unit.clone(),
            m.description.clone(),
        )
    }

    // The point of the pass: one metric expressed in six bindings is one metric,
    // and a monorepo that instruments a Go service and a TypeScript front end
    // gets one comparable answer rather than two shapes.
    #[test]
    fn every_binding_reads_the_same_metric_the_same_way() {
        let rust = one(
            Language::Rust,
            "src/m.rs",
            "fn go() {\n  let c = meter.u64_counter(\"http.server.requests\")\n\
             .with_unit(\"1\").with_description(\"requests served\").build();\n}\n",
        );
        assert_eq!(
            facts(&rust),
            (
                "http.server.requests".into(),
                "counter",
                "u64".into(),
                "1".into(),
                "requests served".into()
            )
        );

        let go = one(
            Language::Go,
            "src/m.go",
            "package m\nfunc go() {\n  c, _ := meter.Int64Counter(\"http.server.requests\", \
             metric.WithUnit(\"1\"), metric.WithDescription(\"requests served\"))\n}\n",
        );
        assert_eq!(
            facts(&go),
            (
                "http.server.requests".into(),
                "counter",
                "int64".into(),
                "1".into(),
                "requests served".into()
            )
        );

        let py = one(
            Language::Python,
            "src/m.py",
            "c = meter.create_counter(\"http.server.requests\", unit=\"1\", \
             description=\"requests served\")\n",
        );
        assert_eq!(
            facts(&py),
            (
                "http.server.requests".into(),
                "counter",
                // python names no type, and a type this pass cannot see is not
                // one it invents
                String::new(),
                "1".into(),
                "requests served".into()
            )
        );

        let ts = one(
            Language::TypeScript,
            "src/m.ts",
            "const c = meter.createCounter('http.server.requests', \
             { description: 'requests served', unit: '1' });\n",
        );
        assert_eq!(
            facts(&ts),
            (
                "http.server.requests".into(),
                "counter",
                String::new(),
                "1".into(),
                "requests served".into()
            )
        );
    }

    // The two bindings that take these by position disagree about the order, so
    // reading one with the other's rule silently swaps a unit for a sentence.
    #[test]
    fn the_positional_bindings_disagree_about_order_and_both_are_honoured() {
        let cs = one(
            Language::CSharp,
            "src/M.cs",
            "class M { static readonly Counter<long> C = \
             Meter.CreateCounter<long>(\"http.server.requests\", \"1\", \"requests served\"); }\n",
        );
        assert_eq!(
            facts(&cs),
            (
                "http.server.requests".into(),
                "counter",
                // the generic argument is the value type C# does not put in the name
                "long".into(),
                "1".into(),
                "requests served".into()
            )
        );

        let cpp = one(
            Language::Cpp,
            "src/m.cc",
            "void go() { auto c = meter->CreateUInt64Counter(\"http.server.requests\", \
             \"requests served\", \"1\"); }\n",
        );
        assert_eq!(
            facts(&cpp),
            (
                "http.server.requests".into(),
                "counter",
                "uint64".into(),
                "1".into(),
                "requests served".into()
            )
        );
    }

    #[test]
    fn an_instrument_is_matched_whole_rather_than_by_the_word_it_ends_in() {
        let m = one(
            Language::Go,
            "src/m.go",
            "package m\nfunc go() { q, _ := meter.Int64UpDownCounter(\"queue.depth\") }\n",
        );
        assert_eq!(m.instrument, Instrument::UpDownCounter);
        assert_eq!(m.value_type, "int64");

        // and a method that merely reads like one is not an instrument
        assert!(collect_file(
            Language::Rust,
            "src/m.rs",
            "fn go() { let c = meter.counter(\"nope\"); }\n",
        )
        .is_empty());
    }

    #[test]
    fn a_name_assembled_at_runtime_is_skipped_rather_than_guessed_at() {
        let got = collect_file(
            Language::Rust,
            "src/m.rs",
            "fn go() {\n  let a = meter.u64_counter(name).build();\n\
             \x20 let b = meter.u64_counter(format!(\"{prefix}.hits\")).build();\n}\n",
        );
        assert!(got.is_empty(), "got {got:?}");
    }

    // A unit given as a constant leaves the next literal in the chain belonging
    // to something else entirely, and reporting that as the unit is worse than
    // reporting no unit at all.
    #[test]
    fn a_unit_that_is_not_a_literal_is_left_empty() {
        let m = one(
            Language::Rust,
            "src/m.rs",
            "fn go() { let c = meter.f64_histogram(\"latency\").with_unit(MILLIS)\
             .with_description(\"how long\").build(); }\n",
        );
        assert_eq!(m.unit, "");
        assert_eq!(m.description, "how long");
    }

    #[test]
    fn a_metric_a_test_creates_is_a_fixture_not_a_surface() {
        assert!(collect_file(
            Language::Rust,
            "src/m.rs",
            "#[test]\nfn test_charges() { let c = meter.u64_counter(\"fixture.hits\").build(); }\n",
        )
        .is_empty());
        assert!(collect_file(
            Language::Rust,
            "src/m.rs",
            "mod tests {\n  fn helper() { let c = meter.u64_counter(\"fixture.hits\").build(); }\n}\n",
        )
        .is_empty());
    }

    fn metric(name: &str, ins: Instrument, ty: &str, unit: &str, file: &str, func: &str) -> Metric {
        Metric {
            name: name.into(),
            instrument: ins,
            value_type: ty.into(),
            unit: unit.into(),
            description: String::new(),
            sites: vec![Site {
                file: file.into(),
                line: 1,
                function: func.into(),
                language: "rust".into(),
            }],
        }
    }

    fn side(ms: &[Metric]) -> BTreeMap<String, Metric> {
        ms.iter().map(|m| (m.name.clone(), m.clone())).collect()
    }

    fn kinds(base: &[Metric], head: &[Metric]) -> BTreeMap<String, MetricChangeKind> {
        diff(&side(base), &side(head))
            .into_iter()
            .map(|c| (c.name.clone(), c.kind))
            .collect()
    }

    #[test]
    fn every_row_of_the_change_table() {
        use Instrument::{Counter, Histogram};
        let base = vec![
            metric("gone", Counter, "u64", "1", "a.rs", "a"),
            metric("reshaped", Counter, "u64", "1", "b.rs", "b"),
            metric("retyped", Counter, "u64", "1", "c.rs", "c"),
            metric("rescaled", Histogram, "f64", "ms", "d.rs", "d"),
            metric("still", Counter, "u64", "1", "e.rs", "e"),
        ];
        let mut described = metric("described", Counter, "u64", "1", "f.rs", "f");
        described.description = "before".into();
        let head = vec![
            metric("arrived", Counter, "u64", "1", "z.rs", "z"),
            metric("reshaped", Histogram, "u64", "1", "b.rs", "b"),
            metric("retyped", Counter, "f64", "1", "c.rs", "c"),
            metric("rescaled", Histogram, "f64", "s", "d.rs", "d"),
            metric("still", Counter, "u64", "1", "e.rs", "e"),
        ];
        let mut base = base;
        let mut head = head;
        base.push(described.clone());
        described.description = "after".into();
        head.push(described);

        let got = kinds(&base, &head);
        assert_eq!(got.get("arrived"), Some(&MetricChangeKind::Added));
        assert_eq!(got.get("gone"), Some(&MetricChangeKind::Removed));
        assert_eq!(
            got.get("reshaped"),
            Some(&MetricChangeKind::InstrumentChanged)
        );
        assert_eq!(got.get("retyped"), Some(&MetricChangeKind::TypeChanged));
        assert_eq!(got.get("rescaled"), Some(&MetricChangeKind::UnitChanged));
        assert_eq!(
            got.get("described"),
            Some(&MetricChangeKind::DescriptionChanged)
        );
        // an untouched metric is not a change, and must not be reported as one
        assert!(!got.contains_key("still"));

        // and only the ones a query cannot survive are breaking
        let breaking: BTreeSet<String> = diff(&side(&base), &side(&head))
            .into_iter()
            .filter(|c| c.breaking)
            .map(|c| c.name)
            .collect();
        assert_eq!(
            breaking,
            ["gone", "rescaled", "reshaped", "retyped"]
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<String>>()
        );
    }

    #[test]
    fn a_metric_that_only_moved_file_is_not_a_change() {
        let base = [metric("hits", Instrument::Counter, "u64", "1", "old.rs", "f")];
        let head = [metric("hits", Instrument::Counter, "u64", "1", "new.rs", "g")];
        assert!(kinds(&base, &head).is_empty());
    }

    #[test]
    fn a_rename_is_paired_only_when_the_pairing_is_unambiguous() {
        use Instrument::Counter;
        // one out, one in, same instrument in the same function: a rename
        let base = [metric("hits", Counter, "u64", "1", "a.rs", "install")];
        let head = [metric("cache.hits", Counter, "u64", "1", "a.rs", "install")];
        let got = diff(&side(&base), &side(&head));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, MetricChangeKind::Renamed);
        assert_eq!(got[0].was, "hits");
        assert_eq!(got[0].name, "cache.hits");
        assert!(got[0].breaking);

        // two out and two in under the same key: which became which is a guess,
        // and a wrong guess hides a real deletion
        let base = [
            metric("a", Counter, "u64", "1", "a.rs", "install"),
            metric("b", Counter, "u64", "1", "a.rs", "install"),
        ];
        let head = [
            metric("c", Counter, "u64", "1", "a.rs", "install"),
            metric("d", Counter, "u64", "1", "a.rs", "install"),
        ];
        let got = kinds(&base, &head);
        assert_eq!(got.get("a"), Some(&MetricChangeKind::Removed));
        assert_eq!(got.get("b"), Some(&MetricChangeKind::Removed));
        assert_eq!(got.get("c"), Some(&MetricChangeKind::Added));
        assert_eq!(got.get("d"), Some(&MetricChangeKind::Added));

        // a different instrument is a different metric, not a renamed one
        let base = [metric("hits", Counter, "u64", "1", "a.rs", "install")];
        let head = [metric(
            "duration",
            Instrument::Histogram,
            "u64",
            "1",
            "a.rs",
            "install",
        )];
        let got = kinds(&base, &head);
        assert_eq!(got.get("hits"), Some(&MetricChangeKind::Removed));
        assert_eq!(got.get("duration"), Some(&MetricChangeKind::Added));
    }

    #[test]
    fn one_name_created_twice_is_one_metric_with_two_sites() {
        let mut out = BTreeMap::new();
        merge(
            &mut out,
            metric("hits", Instrument::Counter, "u64", "1", "b.rs", "f"),
        );
        merge(
            &mut out,
            metric("hits", Instrument::Counter, "u64", "1", "a.rs", "g"),
        );
        assert_eq!(out.len(), 1);
        let m = &out["hits"];
        assert_eq!(m.sites.len(), 2);
        // sorted, so the two sides of a branch cannot disagree about which site
        // the facts came from
        assert_eq!(m.sites[0].file, "a.rs");
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
                "-c",
                "user.name=telemetry-test",
                "-c",
                "user.email=telemetry@test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                msg,
            ],
        );
    }

    fn rev_head(dir: &Path) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dir)
            .output()
            .expect("git rev-parse");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn telemetry_end_to_end_git() {
        let dir = std::env::temp_dir().join(format!("ccc-telemetry-e2e-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src")).expect("mkdir");
        run(&dir, &["git", "init", "-q"]);

        fs::write(
            dir.join("src/metrics.rs"),
            "use opentelemetry::global;\n\
             pub fn install() {\n\
             \x20   let c = meter.u64_counter(\"billing.charges\").with_unit(\"1\").build();\n\
             \x20   let h = meter.f64_histogram(\"billing.latency\").with_unit(\"ms\").build();\n\
             }\n",
        )
        .expect("write");
        // a file that never names the API is never read, so its `createCounter`
        // is its own business
        fs::write(
            dir.join("src/ui.ts"),
            "const meter = ours();\nexport const c = meter.createCounter('ui.clicks');\n",
        )
        .expect("write");
        commit_all(&dir, "base");
        let base_sha = rev_head(&dir);

        fs::write(
            dir.join("src/metrics.rs"),
            "use opentelemetry::global;\n\
             pub fn install() {\n\
             \x20   let c = meter.u64_counter(\"billing.charges.total\").with_unit(\"1\").build();\n\
             \x20   let h = meter.f64_histogram(\"billing.latency\").with_unit(\"s\").build();\n\
             }\n",
        )
        .expect("write");
        commit_all(&dir, "branch");
        let head_sha = rev_head(&dir);

        let r = analyse(
            &dir,
            &TelemetryOptions {
                base_sha: &base_sha,
                head_sha: &head_sha,
                worktree: false,
            },
        );
        assert!(r.error.is_none(), "{:?}", r.error);
        assert!(r.instrumented);
        // `ui.clicks` is not in the surface: the file never names OpenTelemetry
        assert_eq!(r.counts.metrics, 2, "{:?}", r.metrics);
        let by: BTreeMap<&str, &MetricChange> =
            r.changes.iter().map(|c| (c.name.as_str(), c)).collect();
        assert_eq!(
            by["billing.charges.total"].kind,
            MetricChangeKind::Renamed
        );
        assert_eq!(by["billing.charges.total"].was, "billing.charges");
        assert_eq!(by["billing.latency"].kind, MetricChangeKind::UnitChanged);
        assert_eq!(by["billing.latency"].detail, vec!["unit ms -> s"]);
        assert_eq!(r.counts.breaking, 2);

        // an uncommitted edit is invisible to the committed view a CI run wants,
        // and is the whole point of the other one
        fs::write(
            dir.join("src/edge.py"),
            "from opentelemetry import metrics\nq = meter.create_counter(\"edge.queued\")\n",
        )
        .expect("write");
        let committed = analyse(
            &dir,
            &TelemetryOptions {
                base_sha: &base_sha,
                head_sha: &head_sha,
                worktree: false,
            },
        );
        assert_eq!(committed.counts.metrics, 2);
        let live = analyse(
            &dir,
            &TelemetryOptions {
                base_sha: &base_sha,
                head_sha: &head_sha,
                worktree: true,
            },
        );
        assert_eq!(live.counts.metrics, 3);
        assert_eq!(live.counts.added, 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_side_that_could_not_be_read_reports_nothing_rather_than_deletions() {
        let dir = std::env::temp_dir().join(format!("ccc-telemetry-nogit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        // no repository, so neither side can be read at all
        let r = analyse(
            &dir,
            &TelemetryOptions {
                base_sha: "deadbeef",
                head_sha: "deadbeef",
                worktree: false,
            },
        );
        assert!(r.error.is_some());
        assert!(r.changes.is_empty());
        assert!(!r.instrumented);
        // "we could not look" is not "nothing moved"
        assert!(r.gates());
        let _ = fs::remove_dir_all(&dir);
    }
}
