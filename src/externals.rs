//! Cross-repo links.
//!
//! calls do not stop at the process: a gateway calls a billing
//! service that lives in another repository, in another language, behind an
//! HTTP or gRPC hop that no parser can follow. `.ccc/map.json` names those
//! peers under `externals`, and `ccc:serves` / `ccc:calls` comments name the
//! key both ends agree on. Matching keys become real edges of the service
//! graph, with a file and line at each end.
//!
//! A peer is reached one of two ways, and both reduce to the same [`Surface`]:
//!
//!   - `path` - a directory: a sibling checkout, or another corner of a
//!     monorepo. ccc parses it and derives the surface itself.
//!   - `surface` - a file or URL holding a surface this peer published with
//!     `ccc export`. No source, no toolchain for its language, no clone.
//!
//! Peer files deliberately never join `caches`
//! everything downstream keys on paths relative to *this* root

use crate::model::{Boundary, FileCache};
use crate::scan;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SURFACE_SCHEMA: &str = "ccc-surface/1";
// the conventional file name so `surface` can name a directory
pub const SURFACE_NAME: &str = "ccc-surface.json";
// a published surface *should* be small, anything larger considered a pebkac
const MAX_SURFACE_BYTES: u64 = 8 * 1024 * 1024;
const FETCH_TIMEOUT_SECS: u64 = 20;

// One end of a boundary crossing: a handler a repo publishes, or a call it
// makes out. `key` is the rendezvous string both ends must write identically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub key: String,
    pub transport: String,
    pub function: String,
    pub file: String,
    pub line: usize,
    // the service inside that repo when it names more than one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

// what a repository publishes and consumes and nothing else
//
// the cross-repo contract: no bodies, call graph orprivate symbols
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Surface {
    pub schema: String,
    pub name: String,
    pub generated: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub provides: Vec<Endpoint>,
    #[serde(default)]
    pub consumes: Vec<Endpoint>,
}

impl Surface {
    // derive a surface from an already-parsed tree
    pub fn from_caches(name: &str, generated: &str, caches: &[FileCache]) -> Surface {
        let mut provides = Vec::new();
        let mut consumes = Vec::new();
        let mut languages = BTreeSet::new();

        for cache in caches {
            if cache.annotations.is_empty() {
                continue;
            }
            languages.insert(cache.language.as_str().to_string());
            let file = crate::changes::path_str(&cache.rel_path);
            for ann in &cache.annotations {
                let endpoint = Endpoint {
                    key: ann.key.clone(),
                    transport: ann.transport.clone(),
                    function: ann.function.clone(),
                    file: file.clone(),
                    line: ann.line,
                    service: None,
                };
                match ann.boundary {
                    Boundary::Serves => provides.push(endpoint),
                    Boundary::Calls => consumes.push(endpoint),
                }
            }
        }
        provides.sort_by(|a, b| (&a.key, &a.file, a.line).cmp(&(&b.key, &b.file, b.line)));
        consumes.sort_by(|a, b| (&a.key, &a.file, a.line).cmp(&(&b.key, &b.file, b.line)));

        Surface {
            schema: SURFACE_SCHEMA.to_string(),
            name: name.to_string(),
            generated: generated.to_string(),
            repo: None,
            languages: languages.into_iter().collect(),
            provides,
            consumes,
        }
    }

    fn parse(raw: &str, origin: &str) -> Result<Surface> {
        let surface: Surface =
            serde_json::from_str(raw).with_context(|| format!("parsing surface from {origin}"))?;
        if surface.schema != SURFACE_SCHEMA {
            bail!(
                "{origin} declares schema '{}', expected '{SURFACE_SCHEMA}'",
                surface.schema
            );
        }
        Ok(surface)
    }
}

// a peer as named in `.ccc/map.json`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExternalRepo {
    // where the code lives, for display: "acme/billing"
    #[serde(default)]
    pub repo: Option<String>,
    // the peer's language, for display when no surface is reachable
    #[serde(default)]
    pub lang: Option<String>,
    // a directory to parse: a sibling checkout, or a monorepo subtree
    #[serde(default)]
    pub path: Option<String>,
    // a file, directory or URL holding a published surface
    #[serde(default)]
    pub surface: Option<String>,
    // `env:NAME` - the variable holding a bearer token for a private URL
    #[serde(default)]
    pub auth: Option<String>,
}

// a peer after resolution, which may have failed
#[derive(Debug, Clone)]
pub struct ExternalService {
    pub name: String,
    pub config: ExternalRepo,
    // how it was reached, for the report: "path ../billing"
    pub source: String,
    pub surface: Option<Surface>,
    pub error: Option<String>,
}

impl ExternalService {
    pub fn json(&self) -> serde_json::Value {
        let surface = self.surface.as_ref();
        serde_json::json!({
            "name": self.name,
            "repo": self.config.repo,
            "language": self.config.lang.clone()
                .or_else(|| surface.and_then(|s| s.languages.first().cloned())),
            "source": self.source,
            "resolved": self.surface.is_some(),
            "error": self.error,
            "generated": surface.map(|s| s.generated.clone()),
            "provides": surface.map(|s| s.provides.len()).unwrap_or(0),
            "consumes": surface.map(|s| s.consumes.len()).unwrap_or(0),
        })
    }
}

// Resolve every peer named in the config. Errors are captured per peer.
pub fn resolve_all(
    root: &Path,
    externals: &BTreeMap<String, ExternalRepo>,
) -> Vec<ExternalService> {
    externals
        .iter()
        .map(|(name, config)| resolve_one(root, name, config))
        .collect()
}

fn resolve_one(root: &Path, name: &str, config: &ExternalRepo) -> ExternalService {
    let mut service = ExternalService {
        name: name.to_string(),
        config: config.clone(),
        source: String::new(),
        surface: None,
        error: None,
    };

    // A local checkout wins when it is actually there: it is the freshest view
    // of a peer, and in a monorepo it is the only one.
    if let Some(path) = &config.path {
        let dir = resolve_path(root, path);
        service.source = format!("path {}", path);
        if dir.is_dir() {
            match surface_from_dir(name, &dir) {
                Ok(surface) => {
                    service.surface = Some(surface);
                    return service;
                }
                Err(err) => service.error = Some(format!("{err:#}")),
            }
        } else if config.surface.is_none() {
            service.error = Some(format!("{} is not a directory", dir.display()));
            return service;
        }
    }

    if let Some(location) = &config.surface {
        service.source = format!("surface {location}");
        match load_surface(root, location, config.auth.as_deref()) {
            Ok(surface) => {
                service.surface = Some(surface);
                service.error = None;
            }
            Err(err) => service.error = Some(format!("{err:#}")),
        }
        return service;
    }

    if service.source.is_empty() {
        service.source = "unconfigured".to_string();
        service.error = Some(format!(
            "external '{name}' names neither `path` nor `surface`, so there is nothing to read"
        ));
    }
    service
}

fn resolve_path(root: &Path, path: &str) -> PathBuf {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() {
        candidate
    } else {
        root.join(candidate)
    }
}

// Parse a peer checkout and reduce it to its surface.
fn surface_from_dir(name: &str, dir: &Path) -> Result<Surface> {
    // a checkout that already publishes one is cheaper, and is what its owners
    // consider their contract
    let published = dir.join(".ccc").join(SURFACE_NAME);
    if published.is_file() {
        let raw = std::fs::read_to_string(&published)
            .with_context(|| format!("reading {}", published.display()))?;
        if let Ok(mut surface) = Surface::parse(&raw, &published.display().to_string()) {
            surface.name = name.to_string();
            return Ok(surface);
        }
        // a stale or hand-edited file is not a reason to give up: parse instead
    }

    let files = scan::collect_files(dir)
        .with_context(|| format!("scanning external '{name}' at {}", dir.display()))?;
    let caches = scan::build_caches(dir, &files);
    Ok(Surface::from_caches(name, &crate::render::now_ts(), &caches))
}

// Read a surface from a file, a directory holding one, or a URL.
fn load_surface(root: &Path, location: &str, auth: Option<&str>) -> Result<Surface> {
    if location.starts_with("http://") || location.starts_with("https://") {
        let body = fetch(location, auth)?;
        return Surface::parse(&body, location);
    }
    let mut path = resolve_path(root, location.trim_start_matches("file://"));
    if path.is_dir() {
        path = path.join(SURFACE_NAME);
    }
    let meta = std::fs::metadata(&path).with_context(|| format!("reading {}", path.display()))?;
    if meta.len() > MAX_SURFACE_BYTES {
        bail!(
            "{} is {} bytes, over the {MAX_SURFACE_BYTES}-byte surface limit",
            path.display(),
            meta.len()
        );
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    Surface::parse(&raw, &path.display().to_string())
}

// fetch over HTTP by shelling out to curl.
// im not adding a http/tls client just for a few requests
fn fetch(url: &str, auth: Option<&str>) -> Result<String> {
    let token = match auth {
        Some(spec) => Some(read_auth(spec)?),
        None => None,
    };
    let mut cmd = Command::new("curl");
    cmd.args([
        "--silent",
        "--show-error",
        "--location",
        "--fail",
        "--max-time",
        &FETCH_TIMEOUT_SECS.to_string(),
        "--max-filesize",
        &MAX_SURFACE_BYTES.to_string(),
    ]);
    if let Some(token) = &token {
        // via a header argument rather than the URL, so the token cannot end
        // up in a redirect, a proxy log or an error message
        cmd.arg("--header")
            .arg(format!("Authorization: Bearer {token}"));
    }
    cmd.arg(url);

    let output = cmd
        .output()
        .with_context(|| format!("running curl for {url} (is curl installed?)"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "fetching {url} failed ({}): {}",
            output.status,
            stderr.trim()
        );
    }
    String::from_utf8(output.stdout).with_context(|| format!("{url} returned invalid UTF-8"))
}

// Only `env:NAME` is accepted: a literal token in a file that belongs in git
// is a mistake worth refusing rather than supporting.
fn read_auth(spec: &str) -> Result<String> {
    let Some(var) = spec.strip_prefix("env:") else {
        bail!("auth must be written `env:VARIABLE`, got '{spec}'");
    };
    let var = var.trim();
    std::env::var(var).with_context(|| format!("reading auth token from ${var}"))
}

// matched pair; a call here, a handler there.
#[derive(Debug, Clone)]
pub struct Crossing {
    pub key: String,
    pub transport: String,
    pub from: String,
    pub to: String,
    pub file: String,
    pub line: usize,
    pub function: String,
    pub remote: Option<Endpoint>,
    // the peer is another repository rather than a service in this one
    pub external: bool,
}

// index a peers endpoints by key
pub fn index_by_key(endpoints: &[Endpoint]) -> BTreeMap<&str, Vec<&Endpoint>> {
    let mut map: BTreeMap<&str, Vec<&Endpoint>> = BTreeMap::new();
    for endpoint in endpoints {
        map.entry(endpoint.key.as_str()).or_default().push(endpoint);
    }
    map
}

// normalise a key for matching
pub fn norm_key(key: &str) -> String {
    key.trim().to_ascii_lowercase()
}
