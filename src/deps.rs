//! `ccc changes` and `ccc deps` - what this branch did to the dependency tree.
//!
//! `audit` answers what we depend on right now, and never looks at git. This
//! answers the other half: which packages entered the tree, which left, which
//! moved version, and whether that brought in an advisory that was not there
//! before.
//!
//! Both sides resolve through the same lockfile parsers `audit` uses, reading
//! text out of a committed tree rather than off disk, so the committed view a
//! CI run wants cannot be contaminated by a dirty working copy. Only the delta
//! is checked against the advisory database - the standing set is `audit`'s
//! job, and a branch that bumps one package should cost one bounded query.

use crate::audit::{
    self, base_name, join_rel, rel_dir_of, Cached, DiskSource, Ecosystem, Finding, GitSource,
    Location, Package, Source, Unresolved,
};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const SCHEMA: &str = "ccc-deps/1";

// the lockfiles a manifest is regenerated into, so an edit to one without the
// other can be named
const LOCKS_FOR: &[(&str, &[&str])] = &[
    ("Cargo.toml", &["Cargo.lock"]),
    (
        "package.json",
        &["package-lock.json", "yarn.lock", "pnpm-lock.yaml"],
    ),
    ("go.mod", &["go.sum"]),
    (
        "pyproject.toml",
        &[
            "poetry.lock",
            "uv.lock",
            "pdm.lock",
            "Pipfile.lock",
            "requirements.txt",
        ],
    ),
    ("Pipfile", &["Pipfile.lock", "requirements.txt"]),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DepChangeKind {
    Added,
    Removed,
    Upgraded,
    Downgraded,
    // npm's multi-version reality: many in, many out, reported with both lists
    // rather than flattened into a bump that did not happen
    VersionsChanged,
    Promoted,
    Demoted,
    NowShips,
    NoLongerShips,
}

impl DepChangeKind {
    pub fn label(&self) -> &'static str {
        match self {
            DepChangeKind::Added => "added",
            DepChangeKind::Removed => "removed",
            DepChangeKind::Upgraded => "upgraded",
            DepChangeKind::Downgraded => "downgraded",
            DepChangeKind::VersionsChanged => "versions-changed",
            DepChangeKind::Promoted => "promoted",
            DepChangeKind::Demoted => "demoted",
            DepChangeKind::NowShips => "now-ships",
            DepChangeKind::NoLongerShips => "no-longer-ships",
        }
    }

    // the marker the text report draws it with
    fn marker(&self) -> char {
        match self {
            DepChangeKind::Added => '+',
            DepChangeKind::Removed => '-',
            DepChangeKind::Upgraded | DepChangeKind::Downgraded | DepChangeKind::VersionsChanged => {
                '~'
            }
            _ => '*',
        }
    }

    // report order: the kinds a reader acts on first
    fn rank(&self) -> u8 {
        match self {
            DepChangeKind::Added => 0,
            DepChangeKind::Removed => 1,
            DepChangeKind::Upgraded => 2,
            DepChangeKind::Downgraded => 3,
            _ => 4,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DepChange {
    pub kind: DepChangeKind,
    pub ecosystem: Ecosystem,
    pub name: String,
    pub lockfile: String,
    // versions at base, and at head
    pub from: Vec<String>,
    pub to: Vec<String>,
    // as of the head side, or the base side for a package head no longer holds
    pub direct: bool,
    pub dev: bool,
    // head manifest lines to draw this on; empty for a removed package nothing declares
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct DepsCounts {
    pub added: usize,
    pub removed: usize,
    pub upgraded: usize,
    pub downgraded: usize,
    pub other: usize,
    pub introduced: usize,
    pub resolved: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DepsReport {
    pub schema: &'static str,
    pub base_sha: String,
    pub head_sha: String,
    // false when no manifest or lockfile was touched: everything below is empty
    pub changed: bool,
    // the base side had no lockfile at all - the whole closure reads as `added`
    pub baseline: bool,
    pub changes: Vec<DepChange>,
    // advisories this change brings in, and ones it clears
    pub introduced: Vec<Finding>,
    pub resolved: Vec<Finding>,
    // manifests edited without their lockfile being regenerated, and lockfiles
    // that could not be read at one end
    pub drift: Vec<Unresolved>,
    pub assessed: bool,
    pub error: Option<String>,
    pub counts: DepsCounts,
}

impl DepsReport {
    // whether a gate should fail: an advisory came in, or the branch moved
    // dependencies and we could not check - "we could not check" is not "it is
    // fine"
    pub fn gates(&self) -> bool {
        !self.introduced.is_empty() || (self.changed && !self.assessed)
    }
}

pub struct DepsOptions<'a> {
    pub base_sha: &'a str,
    pub head_sha: &'a str,
    // the head side is the working tree rather than the committed head
    pub worktree: bool,
    // (status, path) rows from the branch diff; only the manifest and lockfile
    // rows are read, and an empty result stops the pass before any blob is read
    pub touched: &'a [(String, String)],
    // resolve without consulting the advisory database
    pub offline: bool,
}

// ---------------------------------------------------------------------------
// the pass
// ---------------------------------------------------------------------------

pub fn analyse(root: &Path, opts: &DepsOptions) -> DepsReport {
    let touched: Vec<&str> = opts
        .touched
        .iter()
        .map(|(_, p)| p.as_str())
        .filter(|p| audit::is_input_name(base_name(p)))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    // a branch that touched no manifest costs nothing, which is most branches
    if touched.is_empty() {
        return empty(opts, None);
    }
    if audit::git_out(root, &["rev-parse", "--git-dir"]).is_none() {
        return empty(
            opts,
            Some("git could not be run, so the base side could not be read".into()),
        );
    }

    let base = audit::resolve_with(&GitSource::new(root, opts.base_sha));
    // the head side is read four times over - resolution, the input set, and
    // the two halves of the locator - so it lists itself once
    let head_inner: Box<dyn Source> = if opts.worktree {
        Box::new(DiskSource { root })
    } else {
        Box::new(GitSource::new(root, opts.head_sha))
    };
    let head_src = Cached::new(head_inner.as_ref());
    let head = audit::resolve_with(&head_src);
    let head_inputs: BTreeSet<String> = head_src.inputs().into_iter().collect();

    // a moved lockfile is a move, not a wholesale remove plus add
    let renames = rename_map(root, opts, &touched);
    let base_sides = sides(&base.packages, &renames);
    let head_sides = sides(&head.packages, &BTreeMap::new());

    let keys: BTreeSet<Key> = base_sides.keys().chain(head_sides.keys()).cloned().collect();
    let mut pending: Vec<(DepChange, Package)> = Vec::new();
    for key in &keys {
        let b = base_sides.get(key);
        let h = head_sides.get(key);
        let Some(kind) = classify(b, h) else { continue };
        let side = h.or(b).expect("a key exists on at least one side");
        let Some(sample) = side.packages.first().cloned() else {
            continue;
        };
        pending.push((
            DepChange {
                kind,
                ecosystem: key.0,
                name: key.2.clone(),
                lockfile: key.1.clone(),
                from: b.map(|s| s.versions.iter().cloned().collect()).unwrap_or_default(),
                to: h.map(|s| s.versions.iter().cloned().collect()).unwrap_or_default(),
                direct: side.direct,
                dev: side.dev,
                locations: Vec::new(),
            },
            sample,
        ));
    }

    // only the versions that exist on exactly one side are worth a query: a
    // version present at both ends carried whatever it carried before this
    // branch, and that is the standing set `audit` already reports
    let mut base_only: BTreeSet<Pv> = BTreeSet::new();
    let mut head_only: BTreeSet<Pv> = BTreeSet::new();
    let mut probe: BTreeMap<Pv, Package> = BTreeMap::new();
    for key in &keys {
        let at_base = base_sides.get(key).map(|s| &s.versions);
        let at_head = head_sides.get(key).map(|s| &s.versions);
        for p in head_sides.get(key).map(|s| &s.packages).into_iter().flatten() {
            if !at_base.is_some_and(|v| v.contains(&p.version)) {
                head_only.insert(pv(p));
                probe.insert(pv(p), p.clone());
            }
        }
        for p in base_sides.get(key).map(|s| &s.packages).into_iter().flatten() {
            if !at_head.is_some_and(|v| v.contains(&p.version)) {
                base_only.insert(pv(p));
                // the head-side package is the one a reader can act on, so it wins
                probe.entry(pv(p)).or_insert_with(|| p.clone());
            }
        }
    }

    let mut probe_report = audit::AuditReport {
        packages: probe.into_values().collect(),
        findings: Vec::new(),
        lockfiles: Vec::new(),
        unresolved: Vec::new(),
        assessed: false,
        error: None,
    };
    if opts.offline {
        probe_report.error = Some("the advisory database was not consulted".into());
    } else {
        audit::assess(&mut probe_report);
    }

    let mut came: Vec<Finding> = Vec::new();
    let mut went: Vec<Finding> = Vec::new();
    for f in &probe_report.findings {
        if head_only.contains(&pv(&f.package)) {
            came.push(f.clone());
        }
        if base_only.contains(&pv(&f.package)) {
            went.push(f.clone());
        }
    }
    // an advisory that lands on both sides was already here: this branch
    // neither brought it in nor cleared it
    let on_head: BTreeSet<Ident> = came.iter().map(ident).collect();
    let on_base: BTreeSet<Ident> = went.iter().map(ident).collect();
    let mut introduced: Vec<Finding> = came
        .into_iter()
        .filter(|f| !on_base.contains(&ident(f)))
        .collect();
    let mut resolved: Vec<Finding> = went
        .into_iter()
        .filter(|f| !on_head.contains(&ident(f)))
        .collect();

    // one manifest scan and one lockfile graph for every package to be placed
    let loc = (!pending.is_empty()).then(|| audit::Locator::build(&head_src));
    let place = |pkg: &Package| -> Vec<Location> {
        loc.as_ref().map(|l| l.locate(pkg)).unwrap_or_default()
    };
    let mut changes: Vec<DepChange> = pending
        .into_iter()
        .map(|(mut c, sample)| {
            c.locations = place(&sample);
            c
        })
        .collect();
    changes.sort_by(|a, b| {
        (a.kind.rank(), a.ecosystem, &a.name, &a.lockfile)
            .cmp(&(b.kind.rank(), b.ecosystem, &b.name, &b.lockfile))
    });
    for f in introduced.iter_mut().chain(resolved.iter_mut()) {
        f.locations = place(&f.package);
    }

    let base_locks: BTreeSet<String> = base
        .lockfiles
        .iter()
        .map(|l| renames.get(l).cloned().unwrap_or_else(|| l.clone()))
        .collect();
    let drift = drift(&touched, &head, &head_inputs, &base_locks);

    let counts = DepsCounts {
        added: count(&changes, DepChangeKind::Added),
        removed: count(&changes, DepChangeKind::Removed),
        upgraded: count(&changes, DepChangeKind::Upgraded),
        downgraded: count(&changes, DepChangeKind::Downgraded),
        other: changes.iter().filter(|c| c.kind.rank() == 4).count(),
        introduced: introduced.len(),
        resolved: resolved.len(),
    };

    DepsReport {
        schema: SCHEMA,
        base_sha: opts.base_sha.to_string(),
        head_sha: opts.head_sha.to_string(),
        changed: true,
        // no lockfile at base at all: every package reads as added, and saying
        // so is the difference between that and a branch adding 1,204 packages
        baseline: base.lockfiles.is_empty() && !head.lockfiles.is_empty(),
        changes,
        introduced,
        resolved,
        drift,
        assessed: probe_report.assessed,
        error: probe_report.error,
        counts,
    }
}

fn empty(opts: &DepsOptions, error: Option<String>) -> DepsReport {
    DepsReport {
        schema: SCHEMA,
        base_sha: opts.base_sha.to_string(),
        head_sha: opts.head_sha.to_string(),
        changed: false,
        baseline: false,
        changes: Vec::new(),
        introduced: Vec::new(),
        resolved: Vec::new(),
        drift: Vec::new(),
        assessed: false,
        error,
        counts: DepsCounts::default(),
    }
}

fn count(changes: &[DepChange], kind: DepChangeKind) -> usize {
    changes.iter().filter(|c| c.kind == kind).count()
}

// ---------------------------------------------------------------------------
// keying and classification
// ---------------------------------------------------------------------------

// Keyed per lockfile, not per ecosystem: npm legitimately holds several
// versions of one package in one lockfile, and a monorepo holds one package at
// different versions in different lockfiles. Diffing version sets under this
// key keeps both honest.
type Key = (Ecosystem, String, String);
// a package at an exact version, which is what an advisory range matches
type Pv = (Ecosystem, String, String);
type Ident = (Ecosystem, String, String);

struct Side {
    versions: BTreeSet<String>,
    direct: bool,
    dev: bool,
    // lockfile paths already mapped through any rename
    packages: Vec<Package>,
}

fn pv(p: &Package) -> Pv {
    (p.ecosystem, p.name.clone(), p.version.clone())
}

fn ident(f: &Finding) -> Ident {
    (
        f.package.ecosystem,
        f.package.name.clone(),
        f.advisory.id.clone(),
    )
}

fn sides(pkgs: &[Package], renames: &BTreeMap<String, String>) -> BTreeMap<Key, Side> {
    let mut out: BTreeMap<Key, Side> = BTreeMap::new();
    for p in pkgs {
        let lockfile = renames
            .get(&p.lockfile)
            .cloned()
            .unwrap_or_else(|| p.lockfile.clone());
        let side = out
            .entry((p.ecosystem, lockfile.clone(), p.name.clone()))
            .or_insert_with(|| Side {
                versions: BTreeSet::new(),
                direct: false,
                // a key ships unless every version of it is dev-only
                dev: true,
                packages: Vec::new(),
            });
        side.versions.insert(p.version.clone());
        side.direct |= p.direct;
        side.dev &= p.dev;
        let mut mapped = p.clone();
        mapped.lockfile = lockfile;
        side.packages.push(mapped);
    }
    out
}

fn classify(base: Option<&Side>, head: Option<&Side>) -> Option<DepChangeKind> {
    match (base, head) {
        (None, Some(_)) => Some(DepChangeKind::Added),
        (Some(_), None) => Some(DepChangeKind::Removed),
        (Some(b), Some(h)) => {
            if b.versions != h.versions {
                let gone: Vec<&String> = b.versions.difference(&h.versions).collect();
                let came: Vec<&String> = h.versions.difference(&b.versions).collect();
                // the ordinary bump: exactly one version out, exactly one in
                if let ([from], [to]) = (gone.as_slice(), came.as_slice()) {
                    return Some(match version_direction(from, to) {
                        Some(Ordering::Less) => DepChangeKind::Upgraded,
                        Some(Ordering::Greater) => DepChangeKind::Downgraded,
                        // a pair this ecosystem does not order: still a change,
                        // with no claim about which way it went
                        _ => DepChangeKind::VersionsChanged,
                    });
                }
                return Some(DepChangeKind::VersionsChanged);
            }
            // a flag flip at the same version. Whether it ships outranks whether
            // a manifest names it: a dev advisory that never shipped now does.
            if b.dev != h.dev {
                return Some(if h.dev {
                    DepChangeKind::NoLongerShips
                } else {
                    DepChangeKind::NowShips
                });
            }
            if b.direct != h.direct {
                return Some(if h.direct {
                    DepChangeKind::Promoted
                } else {
                    DepChangeKind::Demoted
                });
            }
            None
        }
        (None, None) => None,
    }
}

// Which way a version moved, or None when the two cannot be ordered honestly.
// A plain `major.minor.patch` is the one shape all five ecosystems order the
// same way; a Go pseudo-version, a PyPI epoch, an npm prerelease tag and a
// NuGet build suffix each order by rules of their own, so they read as unknown
// rather than being guessed at.
fn version_direction(from: &str, to: &str) -> Option<Ordering> {
    Some(triple(from)?.cmp(&triple(to)?))
}

fn triple(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.split('.');
    let mut num = [0u64; 3];
    for slot in num.iter_mut() {
        let Some(part) = parts.next() else { break };
        *slot = part.parse().ok()?;
    }
    // a fourth component is a shape this comparison does not cover
    parts
        .next()
        .is_none()
        .then_some((num[0], num[1], num[2]))
}

// ---------------------------------------------------------------------------
// renames and drift
// ---------------------------------------------------------------------------

// lockfiles that moved, base path -> head path. `changes::parse_name_status`
// reads rename records but flattens them into `renamed(new)` + `deleted(old)`,
// which is all its callers need; this pass needs the pairing, so it asks git
// for it over the handful of paths involved rather than disturbing a hot parser.
fn rename_map(root: &Path, opts: &DepsOptions, touched: &[&str]) -> BTreeMap<String, String> {
    let mut args: Vec<&str> = vec![
        "diff",
        "--relative",
        "--name-status",
        "-z",
        "-M",
        opts.base_sha,
    ];
    if !opts.worktree {
        args.push(opts.head_sha);
    }
    args.push("--");
    args.extend(touched.iter().copied());
    let Some(raw) = audit::git_out(root, &args) else {
        return BTreeMap::new();
    };

    let mut out = BTreeMap::new();
    let mut it = raw.split('\0').filter(|s| !s.is_empty());
    while let Some(status) = it.next() {
        match status.chars().next().unwrap_or('?') {
            'R' => {
                let (Some(old), Some(new)) = (it.next(), it.next()) else {
                    break;
                };
                out.insert(old.to_string(), new.to_string());
            }
            // a copy leaves the original in place, so it renames nothing
            'C' => {
                if it.next().is_none() || it.next().is_none() {
                    break;
                }
            }
            _ => {
                if it.next().is_none() {
                    break;
                }
            }
        }
    }
    out
}

// A resolution that half-worked must never read as a clean result. Everything
// here is a gap between what the branch declared and what it pinned.
fn drift(
    touched: &[&str],
    head: &audit::AuditReport,
    head_inputs: &BTreeSet<String>,
    base_locks: &BTreeSet<String>,
) -> Vec<Unresolved> {
    let seen: BTreeSet<&str> = touched.iter().copied().collect();
    let mut out: Vec<Unresolved> = Vec::new();

    for path in touched {
        let name = base_name(path);
        let dir = rel_dir_of(path);
        let siblings: Vec<&str> = LOCKS_FOR
            .iter()
            .find(|(m, _)| *m == name)
            .map(|(_, l)| l.to_vec())
            .or_else(|| {
                (name.ends_with(".csproj") || name.ends_with(".fsproj"))
                    .then(|| vec!["packages.lock.json"])
            })
            .unwrap_or_default();
        if siblings.is_empty() {
            continue;
        }
        // the manifest moved and one of its lockfiles moved with it: nothing to say
        if siblings
            .iter()
            .any(|l| seen.contains(join_rel(&dir, l).as_str()))
        {
            continue;
        }
        // a manifest with no lockfile beside it is already an `unresolved` in
        // the head resolution, and reporting it twice says nothing new
        let Some(lock) = siblings
            .iter()
            .find(|l| head_inputs.contains(&join_rel(&dir, l)))
        else {
            continue;
        };
        out.push(Unresolved {
            manifest: path.to_string(),
            reason: format!(
                "{name} changed but {lock} did not - the declared range moved and the pinned \
                 version did not"
            ),
        });
    }

    for lock in base_locks {
        if head_inputs.contains(lock) {
            continue;
        }
        let dir = rel_dir_of(lock);
        let manifest = LOCKS_FOR
            .iter()
            .find(|(_, locks)| locks.contains(&base_name(lock)))
            .map(|(m, _)| join_rel(&dir, m))
            .filter(|m| head_inputs.contains(m));
        out.push(Unresolved {
            manifest: lock.clone(),
            reason: match manifest {
                Some(m) => format!("{lock} was removed - {m} now pins nothing"),
                None => format!("{lock} was removed - nothing pins the packages it held"),
            },
        });
    }

    // gaps the head resolution already found, for the files this branch touched
    for u in &head.unresolved {
        if seen.contains(u.manifest.as_str()) {
            out.push(u.clone());
        }
    }

    out.sort_by(|a, b| a.manifest.cmp(&b.manifest));
    out.dedup_by(|a, b| a.manifest == b.manifest && a.reason == b.reason);
    out
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

// the versions cell: `name 1.0.0`, or `name 1.0.0 -> 1.1.0` for a move
fn subject(c: &DepChange) -> String {
    let join = |v: &[String]| v.join(", ");
    match (c.from.is_empty(), c.to.is_empty()) {
        (true, _) => format!("{} {}", c.name, join(&c.to)),
        (_, true) => format!("{} {}", c.name, join(&c.from)),
        _ if c.from == c.to => format!("{} {}", c.name, join(&c.to)),
        _ => format!("{} {} -> {}", c.name, join(&c.from), join(&c.to)),
    }
}

// how it reaches us: the line a person can actually edit, and whether it ships
fn reach(c: &DepChange) -> String {
    let mut out = if c.direct {
        "direct".to_string()
    } else {
        match c.locations.first() {
            Some(l) => format!("transitive, via {}", l.via),
            None => "transitive".to_string(),
        }
    };
    if c.dev {
        out.push_str(", dev");
    }
    out
}

fn headline(r: &DepsReport, base: &str) -> String {
    if !r.changed {
        return format!("dependencies: unchanged against {base}");
    }
    let c = &r.counts;
    let parts: Vec<String> = [
        (c.added, "added"),
        (c.removed, "removed"),
        (c.upgraded, "upgraded"),
        (c.downgraded, "downgraded"),
        (c.other, "other"),
    ]
    .iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, label)| format!("{n} {label}"))
    .collect();
    let detail = if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    };
    format!(
        "dependencies: {} changed{detail} against {base}",
        r.changes.len()
    )
}

// one line per finding, plus the manifest lines it should be read at
fn finding_lines(out: &mut String, f: &Finding, indent: &str) {
    use std::fmt::Write;
    let fixed = match &f.advisory.fixed {
        Some(v) => format!(" - fixed in {v}"),
        None => " - no fixed version published".to_string(),
    };
    let _ = writeln!(
        out,
        "{indent}{}  {}  {} {}{fixed}",
        f.advisory.id, f.advisory.severity, f.package.name, f.package.version
    );
    for l in &f.locations {
        let _ = writeln!(out, "{indent}  {}:{} - via {}", l.manifest, l.line, l.via);
    }
}

// the `changes` text report's voice, appended to it
pub fn text(r: &DepsReport, base: &str) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "{}", headline(r, base));
    if let Some(err) = &r.error {
        // the resolution above still stands; only the assessment is missing
        let _ = writeln!(out, "  not assessed - {err}");
    }
    if !r.changed {
        return out;
    }
    if r.baseline {
        let _ = writeln!(
            out,
            "  no lockfile at the base, so the whole closure reads as added"
        );
    }

    let width = r
        .changes
        .iter()
        .map(|c| subject(c).chars().count())
        .max()
        .unwrap_or(0)
        .min(48);
    let eco = r
        .changes
        .iter()
        .map(|c| c.ecosystem.label().len())
        .max()
        .unwrap_or(0);
    let lock = r
        .changes
        .iter()
        .map(|c| c.lockfile.chars().count())
        .max()
        .unwrap_or(0)
        .min(40);
    for c in &r.changes {
        // `+`, `-` and `~` say what happened; `*` covers four kinds, so it names its own
        let kind = if c.kind.rank() == 4 {
            format!(", {}", c.kind.label())
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "  {} {:width$}  {:eco$}  {:lock$}  {}{kind}",
            c.kind.marker(),
            subject(c),
            c.ecosystem.label(),
            c.lockfile,
            reach(c)
        );
    }

    if !r.introduced.is_empty() {
        let _ = write!(out, "\nintroduced ({}):\n", r.introduced.len());
        for f in &r.introduced {
            finding_lines(&mut out, f, "  ");
        }
    }
    if !r.resolved.is_empty() {
        let _ = write!(out, "\nresolved ({}):\n", r.resolved.len());
        for f in &r.resolved {
            finding_lines(&mut out, f, "  ");
        }
    }
    if !r.drift.is_empty() {
        let _ = write!(out, "\ndrift ({}):\n", r.drift.len());
        for d in &r.drift {
            let _ = writeln!(out, "  {} - {}", d.manifest, d.reason);
        }
    }
    out
}

// the same answer for an agent, in the tone `vulnerabilities` uses
pub fn markdown(r: &DepsReport, base: &str) -> String {
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
        let _ = writeln!(out, "\nadvisories not assessed - {err}");
    }
    if !r.changed {
        out.push_str(
            "\nno manifest or lockfile changed on this branch, so nothing entered, left or moved \
             version. What the project depends on right now is `vulnerabilities`, not this.\n",
        );
        return out;
    }
    if r.baseline {
        out.push_str(
            "\nthe base side held no lockfile, so the whole resolved closure reads as added \
             rather than as a change this branch made\n",
        );
    }

    let _ = write!(out, "\n## changes ({})\n", r.changes.len());
    for c in &r.changes {
        let _ = writeln!(
            out,
            "- {} {} - {}, {}, {}",
            c.kind.label(),
            subject(c),
            c.ecosystem.label(),
            c.lockfile,
            reach(c)
        );
    }

    if r.introduced.is_empty() && r.assessed {
        out.push_str("\nno advisory was introduced by this change\n");
    }
    if !r.introduced.is_empty() {
        let _ = write!(out, "\n## introduced ({})\n", r.introduced.len());
        for f in &r.introduced {
            let _ = writeln!(out, "\n{}", f.advisory.summary);
            finding_lines(&mut out, f, "");
        }
    }
    if !r.resolved.is_empty() {
        let _ = write!(out, "\n## resolved ({})\n", r.resolved.len());
        for f in &r.resolved {
            finding_lines(&mut out, f, "");
        }
    }
    if !r.drift.is_empty() {
        let _ = write!(out, "\n## drift ({})\n", r.drift.len());
        for d in &r.drift {
            let _ = writeln!(out, "- {} - {}", d.manifest, d.reason);
        }
    }
    out.push_str(
        "\nOnly the versions that exist on exactly one side of this branch were checked against \
         the advisory database. Standing advisories against everything else are `vulnerabilities`.\n",
    );
    out
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(9)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, version: &str, direct: bool, dev: bool) -> Package {
        Package {
            ecosystem: Ecosystem::CratesIo,
            name: name.into(),
            version: version.into(),
            direct,
            dev,
            lockfile: "Cargo.lock".into(),
        }
    }

    fn kinds(base: &[Package], head: &[Package]) -> Vec<(String, DepChangeKind)> {
        let b = sides(base, &BTreeMap::new());
        let h = sides(head, &BTreeMap::new());
        let keys: BTreeSet<Key> = b.keys().chain(h.keys()).cloned().collect();
        keys.iter()
            .filter_map(|k| classify(b.get(k), h.get(k)).map(|kind| (k.2.clone(), kind)))
            .collect()
    }

    #[test]
    fn every_row_of_the_classification_table() {
        let base = vec![
            pkg("gone", "1.0.0", true, false),
            pkg("bumped", "1.0.0", true, false),
            pkg("dropped", "2.0.0", true, false),
            pkg("transitive-then-named", "1.0.0", false, false),
            pkg("named-then-transitive", "1.0.0", true, false),
            pkg("build-only", "1.0.0", true, true),
            pkg("shipped", "1.0.0", true, false),
            pkg("still", "1.0.0", true, false),
        ];
        let head = vec![
            pkg("arrived", "0.1.0", true, false),
            pkg("bumped", "1.1.0", true, false),
            pkg("dropped", "1.9.0", true, false),
            pkg("transitive-then-named", "1.0.0", true, false),
            pkg("named-then-transitive", "1.0.0", false, false),
            pkg("build-only", "1.0.0", true, false),
            pkg("shipped", "1.0.0", true, true),
            pkg("still", "1.0.0", true, false),
        ];
        let got: BTreeMap<String, DepChangeKind> = kinds(&base, &head).into_iter().collect();
        assert_eq!(got.get("arrived"), Some(&DepChangeKind::Added));
        assert_eq!(got.get("gone"), Some(&DepChangeKind::Removed));
        assert_eq!(got.get("bumped"), Some(&DepChangeKind::Upgraded));
        assert_eq!(got.get("dropped"), Some(&DepChangeKind::Downgraded));
        assert_eq!(
            got.get("transitive-then-named"),
            Some(&DepChangeKind::Promoted)
        );
        assert_eq!(
            got.get("named-then-transitive"),
            Some(&DepChangeKind::Demoted)
        );
        assert_eq!(got.get("build-only"), Some(&DepChangeKind::NowShips));
        assert_eq!(got.get("shipped"), Some(&DepChangeKind::NoLongerShips));
        // an untouched package is not a change, and must not be reported as one
        assert!(!got.contains_key("still"));
    }

    #[test]
    fn npm_multi_version_reports_both_lists_rather_than_a_fake_bump() {
        let npm = |v: &str| Package {
            ecosystem: Ecosystem::Npm,
            name: "lodash".into(),
            version: v.into(),
            direct: false,
            dev: false,
            lockfile: "package-lock.json".into(),
        };
        let base = vec![npm("4.17.20"), npm("3.10.1")];
        let head = vec![npm("4.17.21"), npm("3.10.2")];
        let got = kinds(&base, &head);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, DepChangeKind::VersionsChanged);

        // and the same package in two lockfiles is two keys, not one conflict
        let mut other = npm("4.17.21");
        other.lockfile = "web/package-lock.json".into();
        let got = kinds(&[npm("4.17.20")], &[npm("4.17.20"), other]);
        assert_eq!(got.len(), 1, "the second lockfile is an addition of its own");
        assert_eq!(got[0].1, DepChangeKind::Added);
    }

    #[test]
    fn a_version_pair_the_ecosystem_does_not_order_claims_no_direction() {
        // a go pseudo-version, a pypi epoch and an npm prerelease tag each order
        // by rules of their own
        assert!(version_direction("0.0.0-20191109021931-daa7c04131f5", "1.2.3").is_none());
        assert!(version_direction("1.0.0", "1.0.1-rc.1").is_none());
        assert!(version_direction("1.0.0", "1!2.0.0").is_none());
        assert!(version_direction("1.2.3", "1.2.3.4").is_none());
        // the shape every ecosystem here does order the same way
        assert_eq!(version_direction("1.0.0", "1.1.0"), Some(Ordering::Less));
        assert_eq!(version_direction("2.0.0", "1.9.9"), Some(Ordering::Greater));
        // a two-component version is the same version as its padded form
        assert_eq!(version_direction("2.0", "2.0.0"), Some(Ordering::Equal));

        // an unorderable pair is still reported as a change
        let got = kinds(
            &[pkg("mystery", "1.0.0", true, false)],
            &[pkg("mystery", "1.0.1-rc.1", true, false)],
        );
        assert_eq!(got[0].1, DepChangeKind::VersionsChanged);
    }

    #[test]
    fn a_moved_lockfile_is_a_move_rather_than_a_remove_and_an_add() {
        let mut moved = pkg("serde", "1.0.203", true, false);
        moved.lockfile = "crates/x/Cargo.lock".into();
        let renames: BTreeMap<String, String> = [(
            "Cargo.lock".to_string(),
            "crates/x/Cargo.lock".to_string(),
        )]
        .into_iter()
        .collect();

        let b = sides(&[pkg("serde", "1.0.203", true, false)], &renames);
        let h = sides(&[moved], &BTreeMap::new());
        let keys: BTreeSet<Key> = b.keys().chain(h.keys()).cloned().collect();
        assert_eq!(keys.len(), 1, "both sides key onto the head path");
        let key = keys.iter().next().unwrap();
        assert!(classify(b.get(key), h.get(key)).is_none());
    }

    #[test]
    fn a_branch_that_touched_no_manifest_reads_no_blob() {
        let touched = vec![
            ("modified".to_string(), "src/main.rs".to_string()),
            ("added".to_string(), "README.md".to_string()),
        ];
        let report = analyse(
            Path::new("/nonexistent-root-for-the-short-circuit"),
            &DepsOptions {
                base_sha: "base",
                head_sha: "head",
                worktree: false,
                touched: &touched,
                offline: true,
            },
        );
        assert!(!report.changed);
        assert!(report.changes.is_empty() && report.drift.is_empty());
        // the root does not exist, so any git call or blob read would have left
        // an error behind: a clean report is proof nothing was read
        assert!(report.error.is_none());
    }

    #[test]
    fn drift_names_a_manifest_whose_lockfile_did_not_move() {
        let head = audit::AuditReport {
            packages: Vec::new(),
            findings: Vec::new(),
            lockfiles: vec!["Cargo.lock".into()],
            unresolved: vec![Unresolved {
                manifest: "requirements.txt".into(),
                reason: "1 requirement(s) are ranges rather than `==` pins".into(),
            }],
            assessed: true,
            error: None,
        };
        let inputs: BTreeSet<String> = ["Cargo.toml", "Cargo.lock", "requirements.txt"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let got = drift(
            &["Cargo.toml", "requirements.txt"],
            &head,
            &inputs,
            &BTreeSet::from(["Cargo.lock".to_string(), "web/yarn.lock".to_string()]),
        );
        let by: BTreeMap<&str, &str> = got
            .iter()
            .map(|u| (u.manifest.as_str(), u.reason.as_str()))
            .collect();
        assert!(by["Cargo.toml"].contains("Cargo.lock did not"));
        // a lockfile the head no longer holds pins nothing
        assert!(by["web/yarn.lock"].contains("was removed"));
        // an unpinned requirements.txt is carried through unchanged
        assert!(by["requirements.txt"].contains("`==` pins"));
        // the lockfile that is still there is not drift
        assert!(!by.contains_key("Cargo.lock"));
    }

    // ------------------------------------------------------------------
    // end to end, over a real repository
    // ------------------------------------------------------------------

    const MANIFEST: &str = "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n";

    fn lock_with(version: &str) -> String {
        format!(
            "version = 4\n\n[[package]]\nname = \"serde\"\nversion = \"{version}\"\n\n\
             [[package]]\nname = \"serde_core\"\nversion = \"1.0.0\"\n"
        )
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(dir: &Path, msg: &str) -> String {
        git(dir, &["add", "-A"]);
        git(
            dir,
            &[
                "-c",
                "user.name=deps-test",
                "-c",
                "user.email=deps@test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                msg,
            ],
        );
        git(dir, &["rev-parse", "HEAD"])
    }

    fn write(dir: &Path, files: &[(&str, &str)]) {
        for (rel, text) in files {
            let to = dir.join(rel);
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::write(to, text).unwrap();
        }
    }

    fn repo(tag: &str, files: &[(&str, &str)]) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("ccc-deps-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, files);
        git(&dir, &["init", "-q"]);
        let sha = commit(&dir, "base");
        (dir, sha)
    }

    // the rows `changes` hands over: only the paths are read, so a status
    // label good enough to be distinguishable is good enough here
    fn touched(dir: &Path, base: &str, worktree: bool) -> Vec<(String, String)> {
        let mut args = vec!["diff", "--relative", "--name-status", "-z", "-M", base];
        if !worktree {
            args.push("HEAD");
        }
        let raw = audit::git_out(dir, &args).unwrap_or_default();
        let mut out = Vec::new();
        let mut it = raw.split('\0').filter(|s| !s.is_empty());
        while let Some(status) = it.next() {
            match status.chars().next().unwrap_or('?') {
                'R' | 'C' => {
                    let (Some(old), Some(new)) = (it.next(), it.next()) else {
                        break;
                    };
                    out.push(("renamed".to_string(), new.to_string()));
                    out.push(("deleted".to_string(), old.to_string()));
                }
                _ => {
                    let Some(path) = it.next() else { break };
                    out.push(("modified".to_string(), path.to_string()));
                }
            }
        }
        if worktree {
            let raw =
                audit::git_out(dir, &["ls-files", "--others", "--exclude-standard", "-z"])
                    .unwrap_or_default();
            out.extend(
                raw.split('\0')
                    .filter(|s| !s.is_empty())
                    .map(|p| ("added".to_string(), p.to_string())),
            );
        }
        out
    }

    fn run(dir: &Path, base: &str, worktree: bool) -> DepsReport {
        let head = git(dir, &["rev-parse", "HEAD"]);
        analyse(
            dir,
            &DepsOptions {
                base_sha: base,
                head_sha: &head,
                worktree,
                touched: &touched(dir, base, worktree),
                // the advisory database is a network round trip, and the
                // change set is what this asserts
                offline: true,
            },
        )
    }

    #[test]
    fn a_bump_on_a_branch_is_one_upgrade_and_nothing_else() {
        let (dir, base) = repo(
            "bump",
            &[("Cargo.toml", MANIFEST), ("Cargo.lock", &lock_with("1.0.203"))],
        );
        write(&dir, &[("Cargo.lock", &lock_with("1.0.210"))]);
        commit(&dir, "bump serde");

        let r = run(&dir, &base, false);
        assert!(r.changed && !r.baseline);
        assert_eq!(r.changes.len(), 1, "{:?}", r.changes);
        let c = &r.changes[0];
        assert_eq!(c.kind, DepChangeKind::Upgraded);
        assert_eq!(c.name, "serde");
        assert_eq!(c.from, vec!["1.0.203".to_string()]);
        assert_eq!(c.to, vec!["1.0.210".to_string()]);
        assert!(c.direct && !c.dev);
        assert_eq!(c.lockfile, "Cargo.lock");
        // the line a person can actually edit
        assert_eq!(c.locations.len(), 1);
        assert_eq!(c.locations[0].manifest, "Cargo.toml");
        assert_eq!(c.locations[0].line, 6);
        assert_eq!(r.counts.upgraded, 1);
        assert_eq!(r.counts.added + r.counts.removed + r.counts.other, 0);
        assert!(r.drift.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_bump_left_uncommitted_needs_worktree_to_be_seen() {
        let (dir, base) = repo(
            "worktree",
            &[("Cargo.toml", MANIFEST), ("Cargo.lock", &lock_with("1.0.203"))],
        );
        write(&dir, &[("Cargo.lock", &lock_with("1.0.210"))]);

        // the committed view is what CI wants, and nothing was committed
        let committed = run(&dir, &base, false);
        assert!(!committed.changed, "{:?}", committed.changes);

        let live = run(&dir, &base, true);
        assert_eq!(live.changes.len(), 1, "{:?}", live.changes);
        assert_eq!(live.changes[0].kind, DepChangeKind::Upgraded);
        assert_eq!(live.changes[0].to, vec!["1.0.210".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_first_lockfile_reads_as_a_baseline_rather_than_a_thousand_additions() {
        let (dir, base) = repo("baseline", &[("Cargo.toml", MANIFEST)]);
        write(&dir, &[("Cargo.lock", &lock_with("1.0.203"))]);
        commit(&dir, "lock it");

        let r = run(&dir, &base, false);
        assert!(r.changed && r.baseline);
        assert_eq!(r.counts.added, r.changes.len());
        assert!(r.changes.iter().all(|c| c.kind == DepChangeKind::Added));
        assert!(text(&r, "origin/main").contains("no lockfile at the base"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_manifest_edited_without_its_lockfile_is_drift_rather_than_a_change() {
        let (dir, base) = repo(
            "drift",
            &[("Cargo.toml", MANIFEST), ("Cargo.lock", &lock_with("1.0.203"))],
        );
        write(&dir, &[("Cargo.toml", &MANIFEST.replace("serde = \"1\"", "serde = \"2\""))]);
        commit(&dir, "widen the range");

        let r = run(&dir, &base, false);
        assert!(r.changed);
        // the declared range moved and the pinned version did not, which is
        // exactly nothing to the resolved closure
        assert!(r.changes.is_empty(), "{:?}", r.changes);
        assert_eq!(r.drift.len(), 1);
        assert_eq!(r.drift[0].manifest, "Cargo.toml");
        assert!(r.drift[0].reason.contains("Cargo.lock did not"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lockfile_that_moved_is_followed_rather_than_emptied_and_refilled() {
        let (dir, base) = repo(
            "rename",
            &[
                ("deps/Cargo.toml", MANIFEST),
                ("deps/Cargo.lock", &lock_with("1.0.203")),
            ],
        );
        std::fs::create_dir_all(dir.join("crates/x")).unwrap();
        git(&dir, &["mv", "deps/Cargo.toml", "crates/x/Cargo.toml"]);
        git(&dir, &["mv", "deps/Cargo.lock", "crates/x/Cargo.lock"]);
        commit(&dir, "move the crate");

        let r = run(&dir, &base, false);
        assert!(r.changed, "the lockfile path is a touched path");
        // the packages did not move, only the file did
        assert!(r.changes.is_empty(), "{:?}", r.changes);
        assert!(r.drift.is_empty(), "{:?}", r.drift);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_deleted_lockfile_removes_its_packages_and_says_nothing_pins_them() {
        let (dir, base) = repo(
            "deleted",
            &[("Cargo.toml", MANIFEST), ("Cargo.lock", &lock_with("1.0.203"))],
        );
        std::fs::remove_file(dir.join("Cargo.lock")).unwrap();
        commit(&dir, "drop the lockfile");

        let r = run(&dir, &base, false);
        assert!(r.changes.iter().all(|c| c.kind == DepChangeKind::Removed));
        assert_eq!(r.counts.removed, r.changes.len());
        assert!(r.counts.removed >= 2);
        let reasons: Vec<&str> = r.drift.iter().map(|d| d.reason.as_str()).collect();
        assert!(
            reasons.iter().any(|m| m.contains("Cargo.toml now pins nothing")),
            "{reasons:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unchanged_branch_says_so_in_one_line() {
        let r = empty(
            &DepsOptions {
                base_sha: "aaaaaaaaaaaa",
                head_sha: "bbbbbbbbbbbb",
                worktree: false,
                touched: &[],
                offline: false,
            },
            None,
        );
        assert_eq!(
            text(&r, "origin/main").trim(),
            "dependencies: unchanged against origin/main"
        );
        // and it gates nothing: nothing needed checking
        assert!(!r.gates());
    }

    #[test]
    fn an_unreachable_database_is_reported_and_still_gates() {
        let mut r = empty(
            &DepsOptions {
                base_sha: "a",
                head_sha: "b",
                worktree: false,
                touched: &[],
                offline: false,
            },
            Some("the advisory database is unreachable".into()),
        );
        r.changed = true;
        r.changes.push(DepChange {
            kind: DepChangeKind::Added,
            ecosystem: Ecosystem::CratesIo,
            name: "tokio".into(),
            lockfile: "Cargo.lock".into(),
            from: Vec::new(),
            to: vec!["1.40.0".into()],
            direct: true,
            dev: false,
            locations: Vec::new(),
        });
        // the change set is complete even though the assessment is not
        let out = text(&r, "origin/main");
        assert!(out.contains("not assessed - the advisory database is unreachable"));
        assert!(out.contains("+ tokio 1.40.0"));
        assert!(markdown(&r, "origin/main").contains("- added tokio 1.40.0"));
        // "we could not check" is not "it is fine"
        assert!(r.gates());
    }
}
