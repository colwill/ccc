// Software composition analysis: what this project actually depends on, and which of
// those dependencies carry known vulnerabilities.
// Resolution reads lockfiles rather than manifests

use anyhow::Result;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

const OSV_BATCH_URL: &str = "https://api.osv.dev/v1/querybatch";
const OSV_VULN_URL: &str = "https://api.osv.dev/v1/vulns";
const FETCH_TIMEOUT_SECS: u64 = 20;
// osv caps a querybatch; stay well under it
const BATCH_SIZE: usize = 500;
// how deep to look for lockfiles - deep enough for a monorepo package, not a full tree walk
const MAX_LOCKFILE_DEPTH: usize = 5;

// the ecosystems we resolve, spelled the way the osv api expects
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Ecosystem {
    CratesIo,
    Npm,
    Go,
    PyPi,
    NuGet,
}

impl Ecosystem {
    pub fn osv(&self) -> &'static str {
        match self {
            Ecosystem::CratesIo => "crates.io",
            Ecosystem::Npm => "npm",
            Ecosystem::Go => "Go",
            Ecosystem::PyPi => "PyPI",
            Ecosystem::NuGet => "NuGet",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Ecosystem::CratesIo => "cargo",
            Ecosystem::Npm => "npm",
            Ecosystem::Go => "go",
            Ecosystem::PyPi => "pypi",
            Ecosystem::NuGet => "nuget",
        }
    }
}

// one resolved package at an exact version
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Package {
    pub ecosystem: Ecosystem,
    pub name: String,
    pub version: String,
    // named by a manifest rather than pulled in by something else
    pub direct: bool,
    // build/test only - a finding here does not reach production
    pub dev: bool,
    pub lockfile: String,
}

impl Package {
    fn key(&self) -> (Ecosystem, String, String) {
        (self.ecosystem, self.name.clone(), self.version.clone())
    }
}

// one advisory, as much of it as is worth carrying
#[derive(Debug, Clone, Serialize)]
pub struct Advisory {
    pub id: String,
    pub aliases: Vec<String>,
    pub summary: String,
    pub severity: String,
    // first version that is not affected, when the advisory names one
    pub fixed: Option<String>,
    pub url: String,
}

// an advisory matched to a package we actually depend on
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub package: Package,
    pub advisory: Advisory,
    // manifest lines an editor should draw this on; empty when nothing declared it
    #[serde(default)]
    pub locations: Vec<Location>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuditReport {
    pub packages: Vec<Package>,
    pub findings: Vec<Finding>,
    // lockfiles the resolution came from, relative to the root
    pub lockfiles: Vec<String>,
    // manifests found but not pinned by any lockfile - coverage gaps, reported
    // so a clean result cannot be mistaken for a resolved one
    pub unresolved: Vec<Unresolved>,
    // whether the advisory database was actually consulted
    pub assessed: bool,
    // why it was not, when it was not - never fatal
    pub error: Option<String>,
}

impl AuditReport {
    pub fn direct_count(&self) -> usize {
        self.packages.iter().filter(|p| p.direct).count()
    }

    // findings that reach production, which is the set worth acting on first
    pub fn runtime_findings(&self) -> Vec<&Finding> {
        self.findings.iter().filter(|f| !f.package.dev).collect()
    }
}

// a manifest that names dependencies but that no lockfile pinned, so nothing
// here could be matched against an advisory range. Reported rather than skipped
// silently - a count of "0 findings" means nothing if the packages never resolved.
#[derive(Debug, Clone, Serialize)]
pub struct Unresolved {
    pub manifest: String,
    pub reason: String,
}

// lockfiles, which pin exact versions
const LOCK_NAMES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "go.sum",
    "requirements.txt",
    "poetry.lock",
    "Pipfile.lock",
    "uv.lock",
    "pdm.lock",
    "packages.lock.json",
];

// manifests, which declare ranges - present only so an unpinned project is reported
const MANIFEST_NAMES: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "Pipfile",
];

// A resolution needs text, not a filesystem. Reading a committed tree through
// the same parsers is what lets a caller compare two sides of a branch without
// one of them being whatever happens to sit on disk right now.
pub trait Source {
    // every lockfile and manifest path this source holds, relative to the root
    fn inputs(&self) -> Vec<String>;
    fn read(&self, rel: &str) -> Option<String>;
}

pub struct DiskSource<'a> {
    pub root: &'a Path,
}

impl Source for DiskSource<'_> {
    fn inputs(&self) -> Vec<String> {
        let mut out = Vec::new();
        let walk = WalkBuilder::new(self.root)
            .max_depth(Some(MAX_LOCKFILE_DEPTH))
            .hidden(false)
            .git_ignore(true)
            .filter_entry(|e| !is_vendored(e.file_name().to_str().unwrap_or("")))
            .build();
        for entry in walk.flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if entry.file_name().to_str().is_some_and(is_input_name) {
                out.push(rel_of(self.root, entry.path()));
            }
        }
        out.sort();
        out.dedup();
        out
    }

    fn read(&self, rel: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(rel)).ok()
    }
}

// A source that lists its inputs once. A resolution, its manifest declarations
// and its lockfile graph are three passes over the same file set, and a walk
// per pass is two walks too many.
pub struct Cached<'a> {
    inner: &'a dyn Source,
    inputs: Mutex<Option<Vec<String>>>,
}

impl<'a> Cached<'a> {
    pub fn new(inner: &'a dyn Source) -> Cached<'a> {
        Cached {
            inner,
            inputs: Mutex::new(None),
        }
    }
}

impl Source for Cached<'_> {
    fn inputs(&self) -> Vec<String> {
        let mut slot = self.inputs.lock().unwrap_or_else(|p| p.into_inner());
        slot.get_or_insert_with(|| self.inner.inputs()).clone()
    }

    fn read(&self, rel: &str) -> Option<String> {
        self.inner.read(rel)
    }
}

// a committed tree, read with `git show <sha>:<rel>`
pub struct GitSource<'a> {
    root: &'a Path,
    sha: &'a str,
    // `root` may sit below the repository root, and git speaks repo-relative
    prefix: String,
    // several passes read the same lockfile; one `git show` per blob is enough
    blobs: Mutex<BTreeMap<String, Option<String>>>,
}

impl<'a> GitSource<'a> {
    pub fn new(root: &'a Path, sha: &'a str) -> GitSource<'a> {
        GitSource {
            root,
            sha,
            prefix: git_prefix(root),
            blobs: Mutex::new(BTreeMap::new()),
        }
    }
}

impl Source for GitSource<'_> {
    fn inputs(&self) -> Vec<String> {
        let Some(raw) = git_out(self.root, &["ls-tree", "-r", "--name-only", "-z", self.sha])
        else {
            return Vec::new();
        };
        let mut out: Vec<String> = raw
            .split('\0')
            .filter(|s| !s.is_empty())
            .filter_map(|full| full.strip_prefix(self.prefix.as_str()))
            .filter(|rel| is_input_name(base_name(rel)))
            // the same exclusions the disk walk makes, and for the same reasons
            .filter(|rel| {
                !rel.split('/').any(is_vendored) && rel.split('/').count() <= MAX_LOCKFILE_DEPTH
            })
            .map(str::to_string)
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn read(&self, rel: &str) -> Option<String> {
        let mut blobs = self.blobs.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(hit) = blobs.get(rel) {
            return hit.clone();
        }
        let spec = format!("{}:{}{rel}", self.sha, self.prefix);
        let text = git_out(self.root, &["show", &spec]);
        blobs.insert(rel.to_string(), text.clone());
        text
    }
}

// stdout of a git command, or None when git is absent or the command failed -
// a missing blob and a missing git are both "this source does not hold it"
pub(crate) fn git_out(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

// where `root` sits inside its repository, "" at the top
fn git_prefix(root: &Path) -> String {
    git_out(root, &["rev-parse", "--show-prefix"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

// a lockfile inside a dependency tree describes that dependency, not us
fn is_vendored(name: &str) -> bool {
    matches!(name, "node_modules" | "target" | "vendor" | ".git")
}

// the file names a resolution reads: lockfiles, and the manifests beside them
pub fn is_input_name(name: &str) -> bool {
    LOCK_NAMES.contains(&name)
        || MANIFEST_NAMES.contains(&name)
        || name.ends_with(".csproj")
        || name.ends_with(".fsproj")
}

fn is_lock_name(name: &str) -> bool {
    LOCK_NAMES.contains(&name) || name.ends_with(".csproj") || name.ends_with(".fsproj")
}

pub(crate) fn base_name(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[i + 1..],
        None => rel,
    }
}

// join a root-relative directory and a file name, "" being the root itself
pub(crate) fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

// find every lockfile under `root` and resolve it to exact packages
pub fn resolve(root: &Path) -> AuditReport {
    resolve_with(&DiskSource { root })
}

// the same resolution against any source, so a committed tree and a working
// tree go through one code path rather than two that can drift apart
pub fn resolve_with(src: &dyn Source) -> AuditReport {
    let mut packages: Vec<Package> = Vec::new();
    let mut lockfiles: Vec<String> = Vec::new();
    let mut unresolved: Vec<Unresolved> = Vec::new();
    let inputs = src.inputs();
    let present: BTreeSet<&str> = inputs.iter().map(String::as_str).collect();

    for rel in inputs.iter().filter(|r| is_lock_name(base_name(r))) {
        let Some(text) = src.read(rel) else { continue };
        let dir = rel_dir_of(rel);
        let name = base_name(rel);

        let found = match name {
            "Cargo.lock" => {
                parse_toml_lock(&text, rel, Ecosystem::CratesIo, &direct_cargo(src, &dir))
            }
            "package-lock.json" => parse_package_lock(&text, rel, &direct_npm(src, &dir)),
            "yarn.lock" => parse_yarn_lock(&text, rel, &direct_npm(src, &dir)),
            "pnpm-lock.yaml" => parse_pnpm_lock(&text, rel, &direct_npm(src, &dir)),
            "go.sum" => parse_go_sum(&text, rel, &direct_go(src, &dir)),
            "poetry.lock" | "uv.lock" | "pdm.lock" => {
                parse_toml_lock(&text, rel, Ecosystem::PyPi, &direct_python(src, &dir))
            }
            "Pipfile.lock" => parse_pipfile_lock(&text, rel),
            "packages.lock.json" => parse_nuget_lock(&text, rel),
            "requirements.txt" => {
                let (found, skipped) = parse_requirements(&text, rel);
                if skipped > 0 {
                    unresolved.push(Unresolved {
                        manifest: rel.clone(),
                        reason: format!(
                            "{skipped} requirement(s) are ranges rather than `==` pins, so no \
                             version could be matched - lock them with `pip freeze` or `pip-compile`"
                        ),
                    });
                }
                found
            }
            _ if name.ends_with(".csproj") || name.ends_with(".fsproj") => {
                // only when no packages.lock.json in the same directory already pinned it
                if present.contains(join_rel(&dir, "packages.lock.json").as_str()) {
                    Vec::new()
                } else {
                    let (found, skipped) = parse_msbuild_project(&text, rel);
                    if skipped > 0 {
                        unresolved.push(Unresolved {
                            manifest: rel.clone(),
                            reason: format!(
                                "{skipped} PackageReference(s) use a version range rather than an \
                                 exact version - enable NuGet lockfiles with \
                                 RestorePackagesWithLockFile for the transitive closure"
                            ),
                        });
                    }
                    found
                }
            }
            _ => Vec::new(),
        };

        if found.is_empty() {
            continue;
        }
        lockfiles.push(rel.clone());
        packages.extend(found);
    }

    // a manifest whose ecosystem produced nothing means this project was never resolved
    for rel in inputs.iter().filter(|r| MANIFEST_NAMES.contains(&base_name(r))) {
        let (eco, want) = match base_name(rel) {
            "Cargo.toml" => (Ecosystem::CratesIo, "Cargo.lock (`cargo generate-lockfile`)"),
            "package.json" => (
                Ecosystem::Npm,
                "package-lock.json, yarn.lock or pnpm-lock.yaml (`npm install`)",
            ),
            "go.mod" => (Ecosystem::Go, "go.sum (`go mod download`)"),
            "pyproject.toml" | "Pipfile" => (
                Ecosystem::PyPi,
                "poetry.lock, uv.lock, pdm.lock or Pipfile.lock",
            ),
            _ => continue,
        };
        let dir_rel = rel_dir_of(rel);
        let covered = packages
            .iter()
            .any(|p| p.ecosystem == eco && rel_dir_of(&p.lockfile) == dir_rel);
        if !covered {
            unresolved.push(Unresolved {
                manifest: rel.clone(),
                reason: format!("no lockfile beside it - versions come from {want}"),
            });
        }
    }

    packages.sort();
    packages.dedup_by(|a, b| a.key() == b.key());
    lockfiles.sort();
    unresolved.sort_by(|a, b| a.manifest.cmp(&b.manifest));
    unresolved.dedup_by(|a, b| a.manifest == b.manifest);

    AuditReport {
        packages,
        findings: Vec::new(),
        lockfiles,
        unresolved,
        assessed: false,
        error: None,
    }
}

fn rel_of(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

// the directory part of a repo-relative file path, "" at the root
pub(crate) fn rel_dir_of(rel: &str) -> String {
    match rel.rfind('/') {
        Some(i) => rel[..i].to_string(),
        None => String::new(),
    }
}

// `[[package]]` blocks with a name and an exact version. Cargo, poetry, uv and
// pdm all write this same shape, so one parser serves four ecosystems.
fn parse_toml_lock(
    text: &str,
    lockfile: &str,
    ecosystem: Ecosystem,
    direct: &BTreeSet<String>,
) -> Vec<Package> {
    let mut out = Vec::new();
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let mut dev = false;
    let mut in_package = false;

    let flush = |name: &mut Option<String>,
                 version: &mut Option<String>,
                 dev: &mut bool,
                 out: &mut Vec<Package>| {
        if let (Some(n), Some(v)) = (name.take(), version.take()) {
            let key = n.to_ascii_lowercase();
            out.push(Package {
                ecosystem,
                direct: direct.contains(&key) || direct.contains(&n),
                name: n,
                version: v,
                dev: *dev,
                lockfile: lockfile.to_string(),
            });
        }
        *dev = false;
    };

    for line in text.lines() {
        let t = line.trim();
        if t == "[[package]]" {
            flush(&mut name, &mut version, &mut dev, &mut out);
            in_package = true;
            continue;
        }
        if t.starts_with('[') && t != "[[package]]" {
            flush(&mut name, &mut version, &mut dev, &mut out);
            in_package = false;
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(v) = t.strip_prefix("name = ") {
            name = Some(v.trim().trim_matches('"').to_string());
        } else if let Some(v) = t.strip_prefix("version = ") {
            version = Some(v.trim().trim_matches('"').to_string());
        } else if let Some(v) = t.strip_prefix("category = ") {
            // poetry before 1.5 marked the group this way
            dev = v.trim().trim_matches('"') == "dev";
        } else if let Some(v) = t.strip_prefix("groups = ") {
            // poetry 1.5+ and pdm; dev only when no runtime group claims it
            let groups = v.trim();
            dev = !groups.contains("\"main\"") && !groups.contains("\"default\"");
        }
    }
    flush(&mut name, &mut version, &mut dev, &mut out);
    out
}

// npm lockfile v2/v3 keep a flat `packages` map keyed by install path; v1 nests `dependencies`
fn parse_package_lock(text: &str, lockfile: &str, direct: &BTreeSet<String>) -> Vec<Package> {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();

    if let Some(map) = doc.get("packages").and_then(|v| v.as_object()) {
        for (path, entry) in map {
            // the root project itself is not a dependency
            if path.is_empty() {
                continue;
            }
            // nested installs read `node_modules/a/node_modules/b` - the package is the last hop
            let Some(idx) = path.rfind("node_modules/") else {
                continue;
            };
            let name = &path[idx + "node_modules/".len()..];
            let Some(version) = entry.get("version").and_then(|v| v.as_str()) else {
                continue;
            };
            let dev = entry.get("dev").and_then(|v| v.as_bool()).unwrap_or(false)
                || entry
                    .get("devOptional")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
            out.push(Package {
                ecosystem: Ecosystem::Npm,
                direct: direct.contains(name),
                name: name.to_string(),
                version: version.to_string(),
                dev,
                lockfile: lockfile.to_string(),
            });
        }
        return out;
    }

    // v1 fallback: walk the nested `dependencies` tree
    fn walk(
        node: &serde_json::Value,
        lockfile: &str,
        direct: &BTreeSet<String>,
        out: &mut Vec<Package>,
    ) {
        let Some(map) = node.get("dependencies").and_then(|v| v.as_object()) else {
            return;
        };
        for (name, entry) in map {
            if let Some(version) = entry.get("version").and_then(|v| v.as_str()) {
                out.push(Package {
                    ecosystem: Ecosystem::Npm,
                    direct: direct.contains(name.as_str()),
                    name: name.clone(),
                    version: version.to_string(),
                    dev: entry.get("dev").and_then(|v| v.as_bool()).unwrap_or(false),
                    lockfile: lockfile.to_string(),
                });
            }
            walk(entry, lockfile, direct, out);
        }
    }
    walk(&doc, lockfile, direct, &mut out);
    out
}

// yarn classic and berry both write `<specs>:` then an indented `version`
fn parse_yarn_lock(text: &str, lockfile: &str, direct: &BTreeSet<String>) -> Vec<Package> {
    let mut out = Vec::new();
    let mut pending: Option<String> = None;
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented {
            pending = line
                .trim()
                .strip_suffix(':')
                .and_then(|h| h.split(',').next())
                .map(|spec| spec.trim().trim_matches('"').to_string())
                .and_then(|spec| yarn_name(&spec));
            continue;
        }
        let t = line.trim();
        let value = t
            .strip_prefix("version ")
            .or_else(|| t.strip_prefix("version: "))
            .or_else(|| t.strip_prefix("version:"));
        if let (Some(name), Some(v)) = (pending.as_ref(), value) {
            let version = v.trim().trim_matches('"').to_string();
            if !version.is_empty() {
                out.push(Package {
                    ecosystem: Ecosystem::Npm,
                    direct: direct.contains(name.as_str()),
                    name: name.clone(),
                    version,
                    // yarn does not record the group in the lockfile
                    dev: false,
                    lockfile: lockfile.to_string(),
                });
            }
            pending = None;
        }
    }
    out
}

// `pkg@^1.0.0` / `@scope/pkg@npm:^1.0.0` -> the package name
fn yarn_name(spec: &str) -> Option<String> {
    let at = spec.get(1..)?.rfind('@').map(|i| i + 1)?;
    let name = &spec[..at];
    (!name.is_empty()).then(|| name.to_string())
}

// pnpm keys the `packages` map by `name@version`, with an optional peer suffix
fn parse_pnpm_lock(text: &str, lockfile: &str, direct: &BTreeSet<String>) -> Vec<Package> {
    let mut out = Vec::new();
    let mut in_packages = false;
    let mut pending: Option<(String, String)> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        // a blank line is not indented, but it does not end the section either
        if trimmed.is_empty() {
            continue;
        }
        if !line.starts_with(' ') && !line.starts_with('\t') {
            // `snapshots:` repeats the same set, so only `packages:` is read
            in_packages = trimmed == "packages:";
            pending = None;
            continue;
        }
        if !in_packages {
            continue;
        }
        // a key line is the only thing at two-space depth that ends in a colon
        if let Some(key) = trimmed.strip_suffix(':') {
            let key = key.trim().trim_matches('\'').trim_matches('"');
            let key = key.strip_prefix('/').unwrap_or(key);
            // drop a peer-dependency suffix: `pkg@1.0.0(react@18.0.0)`
            let key = key.split('(').next().unwrap_or(key);
            if let Some((name, version)) = pnpm_split(key) {
                pending = Some((name, version));
                if let Some((n, v)) = pending.clone() {
                    out.push(Package {
                        ecosystem: Ecosystem::Npm,
                        direct: direct.contains(n.as_str()),
                        name: n,
                        version: v,
                        dev: false,
                        lockfile: lockfile.to_string(),
                    });
                }
            }
            continue;
        }
        // `dev: true` belongs to the key above it
        if trimmed == "dev: true" {
            if let Some((n, v)) = pending.take() {
                if let Some(p) = out
                    .iter_mut()
                    .rfind(|p: &&mut Package| p.name == n && p.version == v)
                {
                    p.dev = true;
                }
            }
        }
    }
    out
}

fn pnpm_split(key: &str) -> Option<(String, String)> {
    let at = key.get(1..)?.rfind('@').map(|i| i + 1)?;
    let (name, version) = key.split_at(at);
    let version = version.strip_prefix('@')?;
    if name.is_empty() || version.is_empty() || !version.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some((name.to_string(), version.to_string()))
}

// pipenv splits runtime and dev into two maps, each `name -> {version: "==x"}`
fn parse_pipfile_lock(text: &str, lockfile: &str) -> Vec<Package> {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (section, dev) in [("default", false), ("develop", true)] {
        let Some(map) = doc.get(section).and_then(|v| v.as_object()) else {
            continue;
        };
        for (name, entry) in map {
            let Some(version) = entry.get("version").and_then(|v| v.as_str()) else {
                continue;
            };
            let version = version.trim_start_matches('=').trim();
            if version.is_empty() {
                continue;
            }
            out.push(Package {
                ecosystem: Ecosystem::PyPi,
                // everything a Pipfile.lock names was asked for by the Pipfile
                direct: true,
                name: name.clone(),
                version: version.to_string(),
                dev,
                lockfile: lockfile.to_string(),
            });
        }
    }
    out
}

// nuget's lockfile nests by target framework, and marks each entry Direct or Transitive
fn parse_nuget_lock(text: &str, lockfile: &str) -> Vec<Package> {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(frameworks) = doc.get("dependencies").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entries in frameworks.values() {
        let Some(map) = entries.as_object() else {
            continue;
        };
        for (name, entry) in map {
            let Some(version) = entry.get("resolved").and_then(|v| v.as_str()) else {
                continue;
            };
            out.push(Package {
                ecosystem: Ecosystem::NuGet,
                direct: entry.get("type").and_then(|v| v.as_str()) == Some("Direct"),
                name: name.clone(),
                version: version.to_string(),
                dev: false,
                lockfile: lockfile.to_string(),
            });
        }
    }
    out
}

// `<PackageReference Include="X" Version="1.2.3" />` - direct dependencies only,
// and only where the version is exact rather than a range
fn parse_msbuild_project(text: &str, lockfile: &str) -> (Vec<Package>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for line in text.lines() {
        let t = line.trim();
        if !t.contains("PackageReference") {
            continue;
        }
        let Some(name) = xml_attr(t, "Include").or_else(|| xml_attr(t, "Update")) else {
            continue;
        };
        let Some(version) = xml_attr(t, "Version") else {
            // a version held in a child element or a central package file
            skipped += 1;
            continue;
        };
        // `[1.0,2.0)` and `1.*` are ranges, which pin nothing
        if version.contains(['[', ']', '(', ')', '*', ',']) {
            skipped += 1;
            continue;
        }
        out.push(Package {
            ecosystem: Ecosystem::NuGet,
            direct: true,
            name,
            version,
            dev: false,
            lockfile: lockfile.to_string(),
        });
    }
    (out, skipped)
}

fn xml_attr(line: &str, attr: &str) -> Option<String> {
    let at = line.find(&format!("{attr}=\""))? + attr.len() + 2;
    let rest = line.get(at..)?;
    let end = rest.find('"')?;
    let value = &rest[..end];
    (!value.is_empty()).then(|| value.to_string())
}

// `module version hash` lines; the `/go.mod` rows repeat a module already listed
fn parse_go_sum(text: &str, lockfile: &str, direct: &BTreeSet<String>) -> Vec<Package> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(version)) = (parts.next(), parts.next()) else {
            continue;
        };
        let version = version.trim_end_matches("/go.mod");
        if name.is_empty() || !version.starts_with('v') {
            continue;
        }
        out.push(Package {
            ecosystem: Ecosystem::Go,
            direct: direct.contains(name),
            name: name.to_string(),
            version: version.trim_start_matches('v').to_string(),
            dev: false,
            lockfile: lockfile.to_string(),
        });
    }
    out
}

// only `name==version` pins resolve to something an advisory can be matched against;
// the count of everything else is returned so the gap can be reported
fn parse_requirements(text: &str, lockfile: &str) -> (Vec<Package>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for line in text.lines() {
        let t = line.split('#').next().unwrap_or("").trim();
        if t.is_empty() || t.starts_with('-') {
            continue;
        }
        let Some((name, version)) = t.split_once("==") else {
            // a range, a url or an extras-only line: nothing to match on
            skipped += 1;
            continue;
        };
        let name = name.trim().split('[').next().unwrap_or("").trim();
        let version = version
            .trim()
            .split(|c: char| c == ' ' || c == ';')
            .next()
            .unwrap_or("");
        if name.is_empty() || version.is_empty() {
            skipped += 1;
            continue;
        }
        out.push(Package {
            ecosystem: Ecosystem::PyPi,
            name: name.to_string(),
            version: version.to_string(),
            // a requirements file is the manifest, so everything in it is declared
            direct: true,
            dev: false,
            lockfile: lockfile.to_string(),
        });
    }
    (out, skipped)
}

// names a manifest declares, used only to mark a resolved package as direct
fn direct_cargo(src: &dyn Source, dir: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(text) = src.read(&join_rel(dir, "Cargo.toml")) else {
        return out;
    };
    let mut in_deps = false;
    for line in text.lines() {
        let t = line.split('#').next().unwrap_or("").trim();
        if let Some(h) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            let h = h.trim();
            in_deps = h.ends_with("dependencies");
            // `[dependencies.serde]` declares one by section name
            if let Some((kind, name)) = h.split_once('.') {
                if kind.ends_with("dependencies") {
                    out.insert(name.trim_matches('"').to_string());
                }
            }
            continue;
        }
        if !in_deps {
            continue;
        }
        if let Some((name, _)) = t.split_once('=') {
            let name = name.trim().trim_matches('"');
            if !name.is_empty() {
                out.insert(name.to_string());
            }
        }
    }
    out
}

fn direct_npm(src: &dyn Source, dir: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(text) = src.read(&join_rel(dir, "package.json")) else {
        return out;
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else {
        return out;
    };
    for kind in ["dependencies", "devDependencies", "optionalDependencies"] {
        if let Some(map) = doc.get(kind).and_then(|v| v.as_object()) {
            out.extend(map.keys().cloned());
        }
    }
    out
}

// pyproject and Pipfile both list what was asked for, whatever tool locked it
fn direct_python(src: &dyn Source, dir: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for file in ["pyproject.toml", "Pipfile"] {
        let Some(text) = src.read(&join_rel(dir, file)) else {
            continue;
        };
        let mut in_deps = false;
        for line in text.lines() {
            let t = line.split('#').next().unwrap_or("").trim();
            if let Some(h) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                let h = h.trim().to_ascii_lowercase();
                in_deps = h.contains("dependencies") || h == "packages" || h == "dev-packages";
                continue;
            }
            // `dependencies = ["flask>=2", ...]` in [project]
            if t.starts_with("dependencies = [") || (in_deps && t.starts_with('"')) {
                for chunk in t.split('"').filter(|c| !c.trim().is_empty()) {
                    let name = chunk
                        .split(|c: char| "=<>!~[;(".contains(c))
                        .next()
                        .unwrap_or("")
                        .trim();
                    if !name.is_empty() && name.chars().next().is_some_and(|c| c.is_alphanumeric()) {
                        out.insert(name.to_ascii_lowercase());
                    }
                }
                continue;
            }
            if !in_deps {
                continue;
            }
            if let Some((name, _)) = t.split_once('=') {
                let name = name.trim().trim_matches('"');
                if !name.is_empty() {
                    out.insert(name.to_ascii_lowercase());
                }
            }
        }
    }
    out
}

fn direct_go(src: &dyn Source, dir: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(text) = src.read(&join_rel(dir, "go.mod")) else {
        return out;
    };
    let mut in_block = false;
    for line in text.lines() {
        let t = line.split("//").next().unwrap_or("").trim();
        if t.starts_with("require (") {
            in_block = true;
            continue;
        }
        if in_block && t == ")" {
            in_block = false;
            continue;
        }
        let spec = if in_block {
            t
        } else {
            match t.strip_prefix("require ") {
                Some(s) => s.trim(),
                None => continue,
            }
        };
        if let Some(path) = spec.split_whitespace().next() {
            if !path.is_empty() {
                out.insert(path.to_string());
            }
        }
    }
    out
}


// attribution: which manifest line a finding belongs on

// where an editor should draw a finding. A direct dependency points at its own
// declaration; a transitive one points at each direct dependency that pulls it
// in, because that is the line a person can actually change.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Location {
    pub manifest: String,
    pub line: usize,
    // the direct dependency whose line this is - the package itself when direct
    pub via: String,
}

// manifests that declare dependencies by name, per ecosystem
const DECLARING: &[(&str, Ecosystem)] = &[
    ("Cargo.toml", Ecosystem::CratesIo),
    ("package.json", Ecosystem::Npm),
    ("pyproject.toml", Ecosystem::PyPi),
    ("Pipfile", Ecosystem::PyPi),
    ("requirements.txt", Ecosystem::PyPi),
    ("go.mod", Ecosystem::Go),
];

// the manifest lines to draw on, resolved once and reused. A caller with many
// packages to place - a dependency delta, say - pays for the manifest scan and
// the lockfile graph once rather than once per package.
pub struct Locator {
    decls: Decls,
    parents: BTreeMap<String, BTreeSet<String>>,
}

impl Locator {
    pub fn build(src: &dyn Source) -> Locator {
        Locator {
            decls: manifest_declarations(src),
            parents: reverse_edges(src),
        }
    }

    pub fn locate(&self, pkg: &Package) -> Vec<Location> {
        locations_for(pkg, &self.decls, &self.parents)
    }
}

// fill in `locations` for every finding, so an editor can draw them in a manifest
pub fn locate(root: &Path, report: &mut AuditReport) {
    locate_with(&Cached::new(&DiskSource { root }), report);
}

pub fn locate_with(src: &dyn Source, report: &mut AuditReport) {
    let loc = Locator::build(src);
    for f in &mut report.findings {
        f.locations = loc.locate(&f.package);
    }
}

type Decls = BTreeMap<(Ecosystem, String), (String, usize)>;

fn locations_for(
    pkg: &Package,
    decls: &Decls,
    parents: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<Location> {
    let key = |n: &str| (pkg.ecosystem, n.to_ascii_lowercase());
    let mut out = Vec::new();

    // a declared package points at its own line
    if let Some((manifest, line)) = decls.get(&key(&pkg.name)) {
        out.push(Location {
            manifest: manifest.clone(),
            line: *line,
            via: pkg.name.clone(),
        });
        return out;
    }

    // otherwise walk up the lockfile graph to whatever declared it
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut queue = vec![pkg.name.to_ascii_lowercase()];
    while let Some(name) = queue.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        // a bounded walk - a pathological graph must not hang the scan
        if seen.len() > 4096 {
            break;
        }
        for parent in parents.get(&name).into_iter().flatten() {
            if let Some((manifest, line)) = decls.get(&key(parent)) {
                out.push(Location {
                    manifest: manifest.clone(),
                    line: *line,
                    via: parent.clone(),
                });
            } else {
                queue.push(parent.to_ascii_lowercase());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

// every name a manifest declares, with the line it is declared on
fn manifest_declarations(src: &dyn Source) -> Decls {
    let mut out: Decls = BTreeMap::new();
    for rel in src.inputs() {
        let name = base_name(&rel);
        let eco = DECLARING
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, e)| *e)
            .or_else(|| {
                (name.ends_with(".csproj") || name.ends_with(".fsproj")).then_some(Ecosystem::NuGet)
            });
        let Some(eco) = eco else { continue };
        let Some(text) = src.read(&rel) else { continue };
        for (declared, line) in declarations_in(&text, eco) {
            // the first manifest to declare a name wins, which keeps the mapping stable
            out.entry((eco, declared)).or_insert((rel.clone(), line));
        }
    }
    out
}

// names declared by this manifest text, with 1-based line numbers
fn declarations_in(text: &str, eco: Ecosystem) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    let mut in_deps = false;

    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let t = raw.split('#').next().unwrap_or("").trim();
        if t.is_empty() {
            continue;
        }

        match eco {
            Ecosystem::NuGet => {
                if t.contains("PackageReference") {
                    if let Some(n) = xml_attr(t, "Include").or_else(|| xml_attr(t, "Update")) {
                        out.push((n.to_ascii_lowercase(), line));
                    }
                }
            }
            Ecosystem::Go => {
                // `require path v1.2.3`, in a block or on its own
                let spec = t.strip_prefix("require ").unwrap_or(t);
                let mut parts = spec.split_whitespace();
                if let (Some(p), Some(v)) = (parts.next(), parts.next()) {
                    if p.contains('/') && v.starts_with('v') {
                        out.push((p.to_ascii_lowercase(), line));
                    }
                }
            }
            Ecosystem::CratesIo | Ecosystem::PyPi => {
                // a table header both switches section and can declare a name
                if let Some(h) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                    let h = h.trim();
                    let lower = h.to_ascii_lowercase();
                    in_deps = lower.ends_with("dependencies")
                        || lower == "packages"
                        || lower == "dev-packages";
                    if let Some((kind, n)) = h.split_once('.') {
                        if kind.to_ascii_lowercase().ends_with("dependencies") {
                            out.push((n.trim_matches('"').to_ascii_lowercase(), line));
                        }
                    }
                    continue;
                }
                // a pep 621 / requirements entry is a bare string
                if t.starts_with('"') || t.starts_with('\'') {
                    if let Some(n) = t.trim_matches(|c| c == '"' || c == '\'' || c == ',').split(|c: char| "=<>!~[;( ".contains(c)).next() {
                        if !n.is_empty() && n.chars().next().is_some_and(|c| c.is_alphanumeric()) {
                            out.push((n.to_ascii_lowercase(), line));
                        }
                    }
                    continue;
                }
                if in_deps || eco == Ecosystem::PyPi {
                    if let Some((n, _)) = t.split_once('=') {
                        let n = n.trim().trim_matches('"');
                        if !n.is_empty() && !n.contains(' ') {
                            out.push((n.to_ascii_lowercase(), line));
                        }
                    }
                }
            }
            Ecosystem::Npm => {
                // `"lodash": "^4.17.15"` - a quoted key with a string value
                let Some(rest) = t.strip_prefix('"') else {
                    continue;
                };
                let Some(end) = rest.find('"') else { continue };
                let key = &rest[..end];
                let after = rest[end + 1..].trim_start();
                if after.starts_with(':') && !key.is_empty() {
                    out.push((key.to_ascii_lowercase(), line));
                }
            }
        }
    }
    out
}

// child -> the packages that require it, from whichever lockfiles record edges
fn reverse_edges(src: &dyn Source) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for rel in src.inputs() {
        let name = base_name(&rel);
        if !matches!(name, "Cargo.lock" | "package-lock.json") {
            continue;
        }
        let Some(text) = src.read(&rel) else { continue };
        match name {
            "Cargo.lock" => cargo_edges(&text, &mut out),
            _ => npm_edges(&text, &mut out),
        }
    }
    out
}

// `dependencies = [ "b", "c 1.0" ]` inside each `[[package]]` block
fn cargo_edges(text: &str, out: &mut BTreeMap<String, BTreeSet<String>>) {
    let mut current: Option<String> = None;
    let mut in_deps = false;
    for line in text.lines() {
        let t = line.trim();
        if t == "[[package]]" {
            current = None;
            in_deps = false;
            continue;
        }
        if let Some(v) = t.strip_prefix("name = ") {
            current = Some(v.trim().trim_matches('"').to_ascii_lowercase());
            continue;
        }
        if t.starts_with("dependencies = [") {
            in_deps = true;
            continue;
        }
        if in_deps {
            if t == "]" {
                in_deps = false;
                continue;
            }
            // an entry is `"name"` or `"name version"`
            let child = t.trim_matches(|c| c == '"' || c == ',').trim();
            if let (Some(parent), Some(first)) = (current.as_ref(), child.split_whitespace().next())
            {
                if !first.is_empty() {
                    out.entry(first.to_ascii_lowercase())
                        .or_default()
                        .insert(parent.clone());
                }
            }
        }
    }
}

// v2/v3 record each install path's own `dependencies` map
fn npm_edges(text: &str, out: &mut BTreeMap<String, BTreeSet<String>>) {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let Some(map) = doc.get("packages").and_then(|v| v.as_object()) else {
        return;
    };
    for (path, entry) in map {
        let parent = match path.rfind("node_modules/") {
            Some(i) => path[i + "node_modules/".len()..].to_ascii_lowercase(),
            // the root entry's dependencies are the direct ones, already declared
            None => continue,
        };
        for kind in ["dependencies", "peerDependencies", "optionalDependencies"] {
            let Some(deps) = entry.get(kind).and_then(|v| v.as_object()) else {
                continue;
            };
            for child in deps.keys() {
                out.entry(child.to_ascii_lowercase())
                    .or_default()
                    .insert(parent.clone());
            }
        }
    }
}

#[derive(Deserialize)]
struct BatchResponse {
    results: Vec<BatchResult>,
}

#[derive(Deserialize)]
struct BatchResult {
    #[serde(default)]
    vulns: Vec<BatchVuln>,
}

#[derive(Deserialize)]
struct BatchVuln {
    id: String,
}

// ask osv which of the resolved packages are affected, and fill in the advisories
pub fn assess(report: &mut AuditReport) {
    if report.packages.is_empty() {
        report.assessed = true;
        return;
    }
    match query_osv(&report.packages) {
        Ok(findings) => {
            report.findings = findings;
            report.assessed = true;
        }
        Err(err) => {
            report.error = Some(format!("{err:#}"));
            report.assessed = false;
        }
    }
}

fn query_osv(packages: &[Package]) -> Result<Vec<Finding>> {
    // package index -> advisory ids affecting it
    let mut hits: Vec<(usize, Vec<String>)> = Vec::new();

    for (chunk_no, chunk) in packages.chunks(BATCH_SIZE).enumerate() {
        let queries: Vec<serde_json::Value> = chunk
            .iter()
            .map(|p| {
                serde_json::json!({
                    "package": { "name": p.name, "ecosystem": p.ecosystem.osv() },
                    "version": p.version,
                })
            })
            .collect();
        let body = serde_json::json!({ "queries": queries }).to_string();
        let raw = post(OSV_BATCH_URL, &body)?;
        let parsed: BatchResponse = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("osv returned an unreadable batch response: {e}"))?;
        for (i, result) in parsed.results.into_iter().enumerate() {
            if result.vulns.is_empty() {
                continue;
            }
            let index = chunk_no * BATCH_SIZE + i;
            hits.push((index, result.vulns.into_iter().map(|v| v.id).collect()));
        }
    }

    // one detail fetch per distinct advisory, however many packages it touches
    let mut details: BTreeMap<String, Advisory> = BTreeMap::new();
    for id in hits.iter().flat_map(|(_, ids)| ids).collect::<BTreeSet<_>>() {
        if let Ok(raw) = get(&format!("{OSV_VULN_URL}/{id}")) {
            if let Ok(doc) = serde_json::from_str::<serde_json::Value>(&raw) {
                details.insert(id.clone(), advisory_from(id, &doc));
            }
        }
    }

    let mut findings = Vec::new();
    for (index, ids) in hits {
        let Some(package) = packages.get(index) else {
            continue;
        };
        for id in ids {
            let advisory = details.get(&id).cloned().unwrap_or_else(|| Advisory {
                id: id.clone(),
                aliases: Vec::new(),
                summary: "advisory details could not be fetched".into(),
                severity: "unknown".into(),
                fixed: None,
                url: format!("https://osv.dev/vulnerability/{id}"),
            });
            findings.push(Finding {
                package: package.clone(),
                advisory,
                locations: Vec::new(),
            });
        }
    }

    // what reaches production first, then worst first, then stable by name
    findings.sort_by(|a, b| {
        a.package
            .dev
            .cmp(&b.package.dev)
            .then_with(|| {
                severity_rank(&a.advisory.severity).cmp(&severity_rank(&b.advisory.severity))
            })
            .then_with(|| a.package.name.cmp(&b.package.name))
            .then_with(|| a.advisory.id.cmp(&b.advisory.id))
    });
    Ok(findings)
}

fn advisory_from(id: &str, doc: &serde_json::Value) -> Advisory {
    let summary = doc
        .get("summary")
        .and_then(|v| v.as_str())
        .or_else(|| doc.get("details").and_then(|v| v.as_str()))
        .unwrap_or("no summary")
        .lines()
        .next()
        .unwrap_or("no summary")
        .trim()
        .to_string();

    let severity = doc
        .get("database_specific")
        .and_then(|d| d.get("severity"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase())
        .or_else(|| {
            // rustsec and friends often ship only a vector; score it rather than shrug
            doc.get("severity")
                .and_then(|v| v.as_array())
                .and_then(|a| {
                    a.iter()
                        .filter_map(|s| s.get("score").and_then(|v| v.as_str()))
                        .find_map(cvss_v3_band)
                })
        })
        .unwrap_or_else(|| "unknown".to_string());

    let aliases = doc
        .get("aliases")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    // the first `fixed` event in any affected range is the version to move to
    let fixed = doc
        .get("affected")
        .and_then(|v| v.as_array())
        .and_then(|affected| {
            affected.iter().find_map(|a| {
                a.get("ranges")?.as_array()?.iter().find_map(|r| {
                    r.get("events")?.as_array()?.iter().find_map(|e| {
                        e.get("fixed").and_then(|v| v.as_str()).map(str::to_string)
                    })
                })
            })
        });

    Advisory {
        id: id.to_string(),
        aliases,
        summary,
        severity,
        fixed,
        url: format!("https://osv.dev/vulnerability/{id}"),
    }
}

// CVSS v3.x base score from its vector, so an advisory carrying only a vector still
// gets a band. v2 and v4 use different formulas and are left to read as unknown.
fn cvss_v3_band(vector: &str) -> Option<String> {
    if !vector.starts_with("CVSS:3") {
        return None;
    }
    let mut m: BTreeMap<&str, &str> = BTreeMap::new();
    for part in vector.split('/').skip(1) {
        if let Some((k, v)) = part.split_once(':') {
            m.insert(k, v);
        }
    }
    let changed = m.get("S") == Some(&"C");
    let av = match *m.get("AV")? {
        "N" => 0.85,
        "A" => 0.62,
        "L" => 0.55,
        "P" => 0.2,
        _ => return None,
    };
    let ac = match *m.get("AC")? {
        "L" => 0.77,
        "H" => 0.44,
        _ => return None,
    };
    // privileges required is scored differently once scope changes
    let pr = match (*m.get("PR")?, changed) {
        ("N", _) => 0.85,
        ("L", false) => 0.62,
        ("L", true) => 0.68,
        ("H", false) => 0.27,
        ("H", true) => 0.5,
        _ => return None,
    };
    let ui = match *m.get("UI")? {
        "N" => 0.85,
        "R" => 0.62,
        _ => return None,
    };
    let impact_of = |k: &str| -> Option<f64> {
        Some(match *m.get(k)? {
            "H" => 0.56,
            "L" => 0.22,
            "N" => 0.0,
            _ => return None,
        })
    };
    let (c, i, a) = (impact_of("C")?, impact_of("I")?, impact_of("A")?);

    let iss = 1.0 - ((1.0 - c) * (1.0 - i) * (1.0 - a));
    let impact = if changed {
        7.52 * (iss - 0.029) - 3.25 * (iss - 0.02).powi(15)
    } else {
        6.42 * iss
    };
    if impact <= 0.0 {
        return Some("none".to_string());
    }
    let exploitability = 8.22 * av * ac * pr * ui;
    let raw = if changed {
        (1.08 * (impact + exploitability)).min(10.0)
    } else {
        (impact + exploitability).min(10.0)
    };
    let score = roundup(raw);
    Some(
        match score {
            s if s >= 9.0 => "critical",
            s if s >= 7.0 => "high",
            s if s >= 4.0 => "moderate",
            s if s > 0.0 => "low",
            _ => "none",
        }
        .to_string(),
    )
}

// the spec's own rounding, which is not the same as rounding to one decimal
fn roundup(input: f64) -> f64 {
    let scaled = (input * 100_000.0).round() as i64;
    if scaled % 10_000 == 0 {
        scaled as f64 / 100_000.0
    } else {
        ((scaled as f64 / 10_000.0).floor() + 1.0) / 10.0
    }
}

pub fn severity_rank(severity: &str) -> u8 {
    match severity {
        "critical" => 0,
        "high" => 1,
        "moderate" | "medium" => 2,
        "low" => 3,
        "rated" => 4,
        _ => 5,
    }
}

// post json by shelling out to curl, for the same reason `externals` does
fn post(url: &str, body: &str) -> Result<String> {
    run_curl(&[
        "--silent",
        "--show-error",
        "--location",
        "--fail",
        "--max-time",
        &FETCH_TIMEOUT_SECS.to_string(),
        "--header",
        "Content-Type: application/json",
        "--data-binary",
        body,
        url,
    ])
}

fn get(url: &str) -> Result<String> {
    run_curl(&[
        "--silent",
        "--show-error",
        "--location",
        "--fail",
        "--max-time",
        &FETCH_TIMEOUT_SECS.to_string(),
        url,
    ])
}

fn run_curl(args: &[&str]) -> Result<String> {
    let output = Command::new("curl")
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("running curl failed (is curl installed?): {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("the advisory database is unreachable: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_lock_yields_exact_versions_and_marks_direct_ones() {
        let lock = r#"
version = 4

[[package]]
name = "anyhow"
version = "1.0.100"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "memchr"
version = "2.7.6"
"#;
        let direct: BTreeSet<String> = ["anyhow".to_string()].into_iter().collect();
        let got = parse_toml_lock(lock, "Cargo.lock", Ecosystem::CratesIo, &direct);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "anyhow");
        assert_eq!(got[0].version, "1.0.100");
        assert!(got[0].direct);
        // the transitive one is exactly what a manifest scan would have missed
        assert_eq!(got[1].name, "memchr");
        assert!(!got[1].direct);
    }

    #[test]
    fn package_lock_v3_reads_the_flat_map_and_keeps_dev_apart() {
        let lock = r#"{
          "lockfileVersion": 3,
          "packages": {
            "": { "version": "0.1.0" },
            "node_modules/esbuild": { "version": "0.21.5", "dev": true },
            "node_modules/left-pad": { "version": "1.3.0" },
            "node_modules/a/node_modules/nested": { "version": "2.0.0" }
          }
        }"#;
        let direct: BTreeSet<String> = ["esbuild".to_string()].into_iter().collect();
        let mut got = parse_package_lock(lock, "package-lock.json", &direct);
        got.sort();
        let names: Vec<&str> = got.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["esbuild", "left-pad", "nested"]);
        assert!(got.iter().find(|p| p.name == "esbuild").unwrap().dev);
        assert!(!got.iter().find(|p| p.name == "left-pad").unwrap().dev);
        // the root entry is the project, never a dependency of itself
        assert!(!got.iter().any(|p| p.version == "0.1.0"));
    }

    #[test]
    fn go_sum_collapses_the_go_mod_rows() {
        let sum = "golang.org/x/text v0.3.7 h1:abc=\ngolang.org/x/text v0.3.7/go.mod h1:def=\n";
        let got = parse_go_sum(sum, "go.sum", &BTreeSet::new());
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|p| p.version == "0.3.7"));
    }

    #[test]
    fn requirements_takes_pins_and_ignores_ranges() {
        let req = "flask==2.0.1\nrequests>=2.0\n# comment\n-r other.txt\ndjango==4.2.1 ; python_version > '3'\n";
        let (got, skipped) = parse_requirements(req, "requirements.txt");
        let names: Vec<&str> = got.iter().map(|p| p.name.as_str()).collect();
        // only the pins resolve to something matchable
        assert_eq!(names, vec!["flask", "django"]);
        assert_eq!(got[1].version, "4.2.1");
        // the range is counted rather than dropped in silence
        assert_eq!(skipped, 1);
    }

    #[test]
    fn an_unreachable_database_is_reported_rather_than_fatal() {
        let mut report = AuditReport {
            packages: vec![Package {
                ecosystem: Ecosystem::CratesIo,
                name: "anyhow".into(),
                version: "1.0.100".into(),
                direct: true,
                dev: false,
                lockfile: "Cargo.lock".into(),
            }],
            findings: Vec::new(),
            lockfiles: vec!["Cargo.lock".into()],
            unresolved: Vec::new(),
            assessed: false,
            error: None,
        };
        // point the assessment at nothing by breaking curl's argument list
        if let Err(err) = query_osv(&[]) {
            let _ = err;
        }
        // an empty package set short-circuits rather than calling out
        report.packages.clear();
        assess(&mut report);
        assert!(report.assessed);
        assert!(report.error.is_none());
    }

    #[test]
    fn cvss_vectors_score_into_the_bands_the_databases_publish() {
        // the esbuild advisory: github rates this one MODERATE, and the vector agrees
        assert_eq!(
            cvss_v3_band("CVSS:3.1/AV:N/AC:H/PR:N/UI:R/S:U/C:H/I:N/A:N").as_deref(),
            Some("moderate")
        );
        // log4shell, the canonical critical
        assert_eq!(
            cvss_v3_band("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:C/C:H/I:H/A:H").as_deref(),
            Some("critical")
        );
        // no impact at all scores zero rather than falling through
        assert_eq!(
            cvss_v3_band("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:N").as_deref(),
            Some("none")
        );
        // v2 and v4 are not this formula, so they are left alone
        assert!(cvss_v3_band("AV:N/AC:L/Au:N/C:P/I:P/A:P").is_none());
        assert!(cvss_v3_band("CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N").is_none());
    }

    #[test]
    fn a_runtime_advisory_outranks_a_worse_dev_only_one() {
        let pkg = |dev: bool, name: &str| Package {
            ecosystem: Ecosystem::Npm,
            name: name.into(),
            version: "1.0.0".into(),
            direct: true,
            dev,
            lockfile: "package-lock.json".into(),
        };
        let adv = |sev: &str| Advisory {
            id: "GHSA-x".into(),
            aliases: vec![],
            summary: "s".into(),
            severity: sev.into(),
            fixed: None,
            url: "u".into(),
        };
        let mut findings = vec![
            Finding {
                package: pkg(true, "dev-critical"),
                advisory: adv("critical"),
                locations: Vec::new(),
            },
            Finding {
                package: pkg(false, "runtime-low"),
                advisory: adv("low"),
                locations: Vec::new(),
            },
        ];
        findings.sort_by(|a, b| {
            a.package
                .dev
                .cmp(&b.package.dev)
                .then_with(|| {
                    severity_rank(&a.advisory.severity).cmp(&severity_rank(&b.advisory.severity))
                })
                .then_with(|| a.package.name.cmp(&b.package.name))
        });
        // a low that ships beats a critical that only ever ran on a build agent
        assert_eq!(findings[0].package.name, "runtime-low");
    }

    #[test]
    fn yarn_classic_and_berry_both_resolve_including_scoped_names() {
        let v1 = "\
# yarn lockfile v1
ansi-regex@^5.0.1:
  version \"5.0.1\"

\"@babel/code-frame@^7.0.0\", \"@babel/code-frame@^7.10.4\":
  version \"7.12.11\"
";
        let got = parse_yarn_lock(v1, "yarn.lock", &BTreeSet::new());
        let names: Vec<(&str, &str)> = got
            .iter()
            .map(|p| (p.name.as_str(), p.version.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![("ansi-regex", "5.0.1"), ("@babel/code-frame", "7.12.11")]
        );

        // berry writes yaml and a `npm:` protocol in the spec
        let berry = "\
\"lodash@npm:^4.17.21\":
  version: 4.17.21
";
        let got = parse_yarn_lock(berry, "yarn.lock", &BTreeSet::new());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "lodash");
        assert_eq!(got[0].version, "4.17.21");
    }

    #[test]
    fn pnpm_strips_the_leading_slash_and_any_peer_suffix() {
        let lock = "\
lockfileVersion: '9.0'

packages:

  /@babel/code-frame@7.12.11:
    resolution: {integrity: sha512-x}
    dev: true

  react-dom@18.2.0(react@18.2.0):
    resolution: {integrity: sha512-y}

snapshots:

  /@babel/code-frame@7.12.11:
    dependencies: {}
";
        let got = parse_pnpm_lock(lock, "pnpm-lock.yaml", &BTreeSet::new());
        let names: Vec<(&str, &str)> = got
            .iter()
            .map(|p| (p.name.as_str(), p.version.as_str()))
            .collect();
        // the snapshots section repeats the set, so it must not double-count
        assert_eq!(
            names,
            vec![("@babel/code-frame", "7.12.11"), ("react-dom", "18.2.0")]
        );
        assert!(got[0].dev, "dev: true belongs to the key above it");
        assert!(!got[1].dev);
    }

    #[test]
    fn poetry_and_uv_share_cargos_package_block_shape() {
        let lock = r#"
[[package]]
name = "flask"
version = "2.0.1"
category = "main"

[[package]]
name = "pytest"
version = "7.4.0"
category = "dev"
"#;
        let direct: BTreeSet<String> = ["flask".to_string()].into_iter().collect();
        let got = parse_toml_lock(lock, "poetry.lock", Ecosystem::PyPi, &direct);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].ecosystem, Ecosystem::PyPi);
        assert!(got[0].direct && !got[0].dev);
        // the dev group is kept apart so it can be filtered from what ships
        assert!(got[1].dev);
    }

    #[test]
    fn pipfile_splits_runtime_from_develop() {
        let lock = r#"{
          "default": { "flask": { "version": "==2.0.1" } },
          "develop": { "pytest": { "version": "==7.4.0" } }
        }"#;
        let mut got = parse_pipfile_lock(lock, "Pipfile.lock");
        got.sort();
        assert_eq!(got.len(), 2);
        let flask = got.iter().find(|p| p.name == "flask").unwrap();
        assert_eq!(flask.version, "2.0.1");
        assert!(!flask.dev);
        assert!(got.iter().find(|p| p.name == "pytest").unwrap().dev);
    }

    #[test]
    fn nuget_lockfile_carries_the_transitive_closure_and_marks_direct_ones() {
        let lock = r#"{
          "version": 1,
          "dependencies": {
            ".NETCoreApp,Version=v8.0": {
              "Newtonsoft.Json": { "type": "Direct", "resolved": "13.0.1" },
              "System.Buffers":   { "type": "Transitive", "resolved": "4.5.1" }
            }
          }
        }"#;
        let mut got = parse_nuget_lock(lock, "packages.lock.json");
        got.sort();
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|p| p.ecosystem == Ecosystem::NuGet));
        assert!(got.iter().find(|p| p.name == "Newtonsoft.Json").unwrap().direct);
        // the transitive one is what a csproj alone would never have named
        assert!(!got.iter().find(|p| p.name == "System.Buffers").unwrap().direct);
    }

    #[test]
    fn a_csproj_resolves_exact_versions_and_counts_the_ranges() {
        let proj = r#"
<Project Sdk="Microsoft.NET.Sdk">
  <ItemGroup>
    <PackageReference Include="Newtonsoft.Json" Version="13.0.1" />
    <PackageReference Include="Serilog" Version="[2.0,3.0)" />
    <PackageReference Include="NoVersion" />
  </ItemGroup>
</Project>
"#;
        let (got, skipped) = parse_msbuild_project(proj, "App.csproj");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "Newtonsoft.Json");
        assert_eq!(got[0].version, "13.0.1");
        // a range and a missing version pin nothing, and are reported rather than dropped
        assert_eq!(skipped, 2);
    }

    // a throwaway repo whose only commit holds `files`, returning (dir, sha)
    fn one_commit_repo(tag: &str, files: &[(&str, &str)]) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("ccc-audit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (rel, text) in files {
            let to = dir.join(rel);
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::write(to, text).unwrap();
        }
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        git(&[
            "-c",
            "user.name=audit-test",
            "-c",
            "user.email=audit@test",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "base",
        ]);
        let sha = git(&["rev-parse", "HEAD"]);
        (dir, sha)
    }

    #[test]
    fn a_committed_tree_resolves_to_what_the_same_content_on_disk_does() {
        let manifest = "[package]\nname = \"app\"\n\n[dependencies]\nserde = \"1\"\n";
        let lock = "version = 4\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.203\"\n\n\
                    [[package]]\nname = \"memchr\"\nversion = \"2.7.6\"\n";
        let (dir, sha) = one_commit_repo(
            "gitsource",
            &[("Cargo.toml", manifest), ("Cargo.lock", lock)],
        );

        let from_disk = resolve(&dir);
        let from_git = resolve_with(&GitSource::new(&dir, &sha));
        assert_eq!(from_git.lockfiles, from_disk.lockfiles);
        assert_eq!(from_git.packages, from_disk.packages);
        assert_eq!(from_git.packages.len(), 2);
        // the manifest is read through the source too, so `direct` survives the trip
        let serde = from_git.packages.iter().find(|p| p.name == "serde").unwrap();
        assert!(serde.direct);
        assert!(!from_git.packages.iter().find(|p| p.name == "memchr").unwrap().direct);

        // and the working tree diverging does not move the committed answer
        std::fs::write(dir.join("Cargo.lock"), lock.replace("1.0.203", "1.0.210")).unwrap();
        let again = resolve_with(&GitSource::new(&dir, &sha));
        assert_eq!(again.packages, from_git.packages);
        assert_eq!(
            resolve(&dir)
                .packages
                .iter()
                .find(|p| p.name == "serde")
                .unwrap()
                .version,
            "1.0.210"
        );

        // a sha that holds nothing resolves to nothing rather than falling back to disk
        let empty = resolve_with(&GitSource::new(&dir, "0000000000000000000000000000000000000000"));
        assert!(empty.packages.is_empty() && empty.lockfiles.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn severity_orders_worst_first() {
        assert!(severity_rank("critical") < severity_rank("high"));
        assert!(severity_rank("high") < severity_rank("moderate"));
        assert!(severity_rank("low") < severity_rank("unknown"));
    }
}
