use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use codecache::{CheckReport, Encoding};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "ccc",
    about = "Scan a project and generate a ContextCodeCache (.ccc) directory",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    /// Human-readable summary (default).
    Text,
    /// Machine-readable JSON: { root, up_to_date, files[], changes[] }.
    Json,
}

#[derive(Subcommand)]
enum Command {
    /// scan a project and (re)generate the `.ccc` directory
    Scan {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        tokens: bool,
        #[arg(long, default_value = "o200k_base")]
        encoding: String,
    },
    /// verify `.ccc` is up to date; exit non-zero if it would change
    Check {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// output format: `text` (default) or `json` (changed files as an array)
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    Tokenize {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value = "o200k_base")]
        encoding: String,
    },
    /// install this `ccc` binary onto your PATH (Linux; defaults to ~/.local/bin)
    Install {
        /// directory to install into (default: ~/.local/bin)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// overwrite an existing `ccc` in the target directory
        #[arg(long)]
        force: bool,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ccc: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan {
            path,
            tokens,
            encoding,
        } => {
            let root = canonical(&path);
            let report = codecache::scan(&root)?;
            let t = report.totals;
            println!(
                "Wrote {} ({} files: {} funcs, {} consts, {} refs, {} notes)",
                report.ccc_dir.display(),
                report.files,
                t.funcs,
                t.consts,
                t.refs,
                t.notes
            );
            if tokens {
                run_tokenize(&root, &encoding)?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Check { path, format } => {
            // Canonicalize for the check itself (so results match `scan`, which
            // does the same), but keep the original `path` for building the
            // repo-relative cache paths reported in JSON.
            let report = codecache::check(&canonical(&path))?;
            match format {
                OutputFormat::Text => print_check_text(&report),
                OutputFormat::Json => print_check_json(&path, &report)?,
            }
            if report.up_to_date {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::FAILURE)
            }
        }
        Command::Tokenize { path, encoding } => {
            let root = canonical(&path);
            run_tokenize(&root, &encoding)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Install { dir, force } => run_install(dir, force),
    }
}

fn print_check_text(report: &CheckReport) {
    if report.up_to_date {
        println!(".ccc is up to date");
    } else {
        eprintln!(".ccc is out of date; run `ccc scan`:");
        for c in &report.changes {
            eprintln!("  {:9} {}", format!("{}:", c.kind.as_str()), c.file);
        }
    }
}

/// Emit `{ root, up_to_date, files[], changes[] }` as one JSON line. `files` is
/// the repo-relative paths of the changed cache entries — ready to hand to
/// another GitHub Action via `fromJSON(...)`.
fn print_check_json(root: &Path, report: &CheckReport) -> Result<()> {
    let ccc_rel = rel_join(root, Path::new(".ccc"));
    let changes: Vec<_> = report
        .changes
        .iter()
        .map(|c| {
            serde_json::json!({
                "status": c.kind.as_str(),
                "file": c.file,
                "path": path_str(&ccc_rel.join(&c.file)),
            })
        })
        .collect();
    let files: Vec<String> = report
        .changes
        .iter()
        .map(|c| path_str(&ccc_rel.join(&c.file)))
        .collect();
    let out = serde_json::json!({
        "root": path_str(root),
        "up_to_date": report.up_to_date,
        "files": files,
        "changes": changes,
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

/// Join `rest` onto `root`, dropping a leading `./` so paths read cleanly
/// (root "." + ".ccc/CCC.md" -> ".ccc/CCC.md").
fn rel_join(root: &Path, rest: &Path) -> PathBuf {
    let mut p = PathBuf::new();
    for c in root.components() {
        if !matches!(c, Component::CurDir) {
            p.push(c.as_os_str());
        }
    }
    p.push(rest);
    p
}

/// Path as a forward-slash string (stable for CI output regardless of platform).
fn path_str(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn run_tokenize(root: &Path, encoding: &str) -> Result<()> {
    let enc = Encoding::parse(encoding)
        .ok_or_else(|| anyhow!("unknown encoding '{encoding}' (use o200k_base or cl100k_base)"))?;
    let report = codecache::tokenize(root, enc)?;
    println!(
        "Wrote {} ({} tokens from {} files, {} bytes, {} encoding; round-trip verified)",
        report.bin_path.display(),
        report.total_tokens,
        report.files,
        report.bytes,
        report.encoding.name(),
    );
    eprintln!(
        "note: these are APPROXIMATE tiktoken IDs - not compatible with Claude/Anthropic \
         (see tokens.json). For exact Claude counts use the count_tokens endpoint."
    );
    Ok(())
}

/// Copy the running `ccc` binary into a directory on the user's PATH.
///
/// Defaults to `~/.local/bin` — the standard user-local bin dir on Linux, so no
/// root/sudo is needed. Warns (but still succeeds) if the target dir isn't on
/// `$PATH` so the user knows the shell won't find `ccc` until they add it.
fn run_install(dir: Option<PathBuf>, force: bool) -> Result<ExitCode> {
    let src = std::env::current_exe()
        .context("could not determine the path to the running ccc binary")?;

    let target_dir = match dir {
        Some(d) => expand_tilde(&d),
        None => default_bin_dir()?,
    };
    let dest = target_dir.join("ccc");

    // Guard against copying the binary onto itself (`std::fs::copy` would
    // truncate it to zero bytes): if we're already running from `dest`, we're
    // done.
    if same_file(&src, &dest) {
        println!("ccc is already installed at {}", dest.display());
        return Ok(ExitCode::SUCCESS);
    }

    if dest.exists() && !force {
        return Err(anyhow!(
            "{} already exists; re-run with --force to overwrite",
            dest.display()
        ));
    }

    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("could not create {}", target_dir.display()))?;
    std::fs::copy(&src, &dest)
        .with_context(|| format!("could not copy {} -> {}", src.display(), dest.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("could not mark {} executable", dest.display()))?;
    }

    println!("Installed ccc to {}", dest.display());
    if !dir_on_path(&target_dir) {
        let d = target_dir.display();
        eprintln!(
            "note: {d} is not on your PATH. Add it to your shell profile, e.g.:\n    \
             echo 'export PATH=\"{d}:$PATH\"' >> ~/.profile"
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `~/.local/bin` — the XDG user-local binary directory on Linux.
fn default_bin_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow!("HOME is not set; pass --dir to choose an install directory"))?;
    Ok(PathBuf::from(home).join(".local").join("bin"))
}

/// Expand a leading `~` (or `~/`) to `$HOME`; leave other paths untouched.
fn expand_tilde(p: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    p.to_path_buf()
}

/// True if both paths resolve to the same existing file.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// True if `dir` is one of the entries in `$PATH`.
fn dir_on_path(dir: &Path) -> bool {
    let canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths)
                .any(|p| p.canonicalize().unwrap_or(p) == canon)
        })
        .unwrap_or(false)
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
