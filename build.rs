//! Package the VS Code extension alongside the binary.
//!
//! `cargo build` also produces `dist/ccc-codecache.vsix` at the repo root, so
//! the analyser and the editor client that talks to it never drift apart.
//!
//! This is best-effort by design: a missing `npm`, a CI runner, or a source
//! tarball with no `extensions/` directory all skip the step with a warning
//! rather than failing the Rust build. Nothing in the crate depends on it.
//!
//! Skipped when:
//!   - `CCC_SKIP_VSIX` is set to anything
//!   - `CI` is set (release and CI builds stay lean; run `npm run package` in a
//!     dedicated job if you want the vsix there)
//!   - `DOCS_RS` is set
//!   - `extensions/vscode` is absent, or `npm` is not on PATH

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// Only these can change the vsix; without them cargo would rerun this script
/// on every touched Rust file and npm would run far more than it needs to.
const INPUTS: &[&str] = &[
    "extensions/vscode/src",
    "extensions/vscode/media",
    "extensions/vscode/package.json",
    "extensions/vscode/package-lock.json",
    "extensions/vscode/tsconfig.json",
    "extensions/vscode/esbuild.mjs",
    "extensions/vscode/.vscodeignore",
    "extensions/vscode/README.md",
];

const OUTPUT: &str = "dist/ccc-codecache.vsix";

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ext = root.join("extensions").join("vscode");

    for path in INPUTS {
        println!("cargo:rerun-if-changed={path}");
    }
    // The output is an input too. Cargo replays a build script's warnings from
    // cache when nothing tracked changed, so without this a deleted vsix would
    // be *reported* as packaged and never rebuilt. A missing path reads as
    // dirty, which is exactly the trigger we want — see stamp() for why the
    // freshly written file does not then rebuild on every subsequent build.
    println!("cargo:rerun-if-changed={OUTPUT}");
    println!("cargo:rerun-if-env-changed=CCC_SKIP_VSIX");

    if let Some(reason) = skip_reason(&ext) {
        println!("cargo:warning=vsix not packaged: {reason}");
        return;
    }

    if let Err(err) = package(&root, &ext) {
        // A broken extension build must not stop anyone compiling the analyser.
        println!("cargo:warning=vsix not packaged: {err}");
    }
}

fn skip_reason(ext: &Path) -> Option<String> {
    if std::env::var_os("CCC_SKIP_VSIX").is_some() {
        return Some("CCC_SKIP_VSIX is set".into());
    }
    if std::env::var_os("DOCS_RS").is_some() {
        return Some("building on docs.rs".into());
    }
    if std::env::var_os("CI").is_some() {
        return Some("running in CI (unset CI, or run `npm run package`, to build it)".into());
    }
    if !ext.join("package.json").is_file() {
        return Some(format!("{} has no package.json", ext.display()));
    }
    if which("npm").is_none() {
        return Some("npm is not on PATH".into());
    }
    None
}

fn package(root: &Path, ext: &Path) -> Result<(), String> {
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).map_err(|e| format!("could not create {}: {e}", dist.display()))?;
    let out = dist.join("ccc-codecache.vsix");

    if !ext.join("node_modules").is_dir() {
        // `npm ci` is the reproducible install, but it needs the lockfile to
        // match package.json exactly; fall back rather than fail the build.
        let install = if ext.join("package-lock.json").is_file() { "ci" } else { "install" };
        run(ext, &[install, "--no-audit", "--no-fund"])?;
    }

    run(ext, &["run", "build"])?;
    run(
        ext,
        &[
            "exec",
            "--",
            "vsce",
            "package",
            "--no-dependencies",
            "--allow-missing-repository",
            "-o",
            &out.to_string_lossy(),
        ],
    )?;

    stamp(root, &out);
    println!("cargo:warning=packaged {}", out.display());
    Ok(())
}

/// Date the vsix by its newest source, not by the moment it was written.
///
/// Cargo decides a build script is dirty by comparing tracked mtimes against a
/// timestamp it takes *before* running the script, so any file the script
/// writes itself looks newer and triggers a rerun — forever. Rolling the mtime
/// back to the newest input breaks that loop (an input that was already older
/// than this run stays older) while leaving a timestamp that still means
/// something: this vsix is as new as the sources it was built from.
///
/// Best-effort throughout. The worst case of a failure here is an extra npm run.
fn stamp(root: &Path, out: &Path) {
    let newest = INPUTS
        .iter()
        .filter_map(|input| newest_mtime(&root.join(input)))
        .max();
    let Some(time) = newest else { return };
    if let Ok(file) = std::fs::File::options().write(true).open(out) {
        let _ = file.set_modified(time);
    }
}

fn newest_mtime(path: &Path) -> Option<SystemTime> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_dir() {
        return meta.modified().ok();
    }
    std::fs::read_dir(path)
        .ok()?
        .filter_map(|entry| newest_mtime(&entry.ok()?.path()))
        .max()
}

fn run(cwd: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new(npm())
        .args(args)
        .current_dir(cwd)
        // npm inherits cargo's environment otherwise, and CARGO_* / RUSTC vars
        // confuse node-gyp style postinstall scripts.
        .env_remove("RUSTC")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .output()
        .map_err(|e| format!("could not run `npm {}`: {e}", args.join(" ")))?;

    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let tail: Vec<&str> = stderr.lines().rev().take(8).collect();
    Err(format!(
        "`npm {}` failed ({}): {}",
        args.join(" "),
        output.status,
        tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
    ))
}

fn npm() -> &'static str {
    // npm ships as a .cmd shim on Windows, which needs the shell to resolve.
    if cfg!(windows) {
        "npm.cmd"
    } else {
        "npm"
    }
}

fn which(bin: &str) -> Option<PathBuf> {
    let name = if cfg!(windows) { format!("{bin}.cmd") } else { bin.to_string() };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(&name);
        candidate.is_file().then_some(candidate)
    })
}
