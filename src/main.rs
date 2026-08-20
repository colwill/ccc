use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use codecache::{CheckReport, Encoding, ChangesOptions, ChangesReport};
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
    // Human-readable summary (default).
    Text,
    // Machine-readable JSON: { root, up_to_date, files[], changes[] }.
    Json,
}

#[derive(Subcommand)]
enum Command {
    // scan a project and (re)generate the `.ccc` directory
    Scan {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        tokens: bool,
        #[arg(long, default_value = "o200k_base")]
        encoding: String,
    },
    // verify `.ccc` is up to date; exit non-zero if it would change
    Check {
        #[arg(default_value = ".")]
        path: PathBuf,
        // output format: `text` (default) or `json` (changed files as an array)
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    Tokenize {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value = "o200k_base")]
        encoding: String,
    },
    // publish what this project serves and calls across process boundaries
    Export {
        #[arg(default_value = ".")]
        path: PathBuf,
        // name other repos will know this one by; defaults to the directory
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        // "owner/repo", recorded for display on the consuming side
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
        // where to write it; defaults to `<path>/.ccc/ccc-surface.json`.
        // `-` writes to stdout.
        #[arg(short, long, value_name = "FILE")]
        out: Option<PathBuf>,
    },
    // surface branch changes to a continuous-testing suite: which services
    // changed, who calls them, and what needs testing (JSON by default).
    // `surf` was the original name; kept so existing scripts keep working.
    #[command(alias = "surf")]
    Changes {
        #[arg(default_value = ".")]
        path: PathBuf,
        // base ref to diff against (default: merge-base with origin/main,
        // main, origin/master or master - first that exists)
        #[arg(long)]
        base: Option<String>,
        // define/extend a service inline: NAME=GLOB (repeatable; merged over
        // `.ccc/map.json`)
        #[arg(long = "service", value_name = "NAME=GLOB")]
        services: Vec<String>,
        // output format (default json - changes is built for pipelines)
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
        // exit non-zero when changed functions have no detected test reference
        #[arg(long)]
        fail_untested: bool,
        // write a starter `.ccc/map.json` inferred from top-level directories
        #[arg(long)]
        init: bool,
        // include uncommitted edits and untracked files in the diff. CI wants
        // the committed view (the default); a local run usually wants this
        #[arg(long)]
        worktree: bool,
        // also write a single-file HTML view of the report (Tailwind + HTMX
        // live-query panel against `ccc serve`), e.g. ccc-changes-rust.html
        #[arg(long, value_name = "FILE")]
        html: Option<PathBuf>,
        // render --html from an existing changes JSON report instead of running
        // the analysis (no git needed)
        #[arg(long, value_name = "REPORT.json", requires = "html")]
        from: Option<PathBuf>,
    },
    // serve the code map over HTTP for AI agents: REST endpoints
    // (/find /references /dependencies ...) + an MCP endpoint at /mcp
    Serve {
        #[arg(default_value = ".")]
        path: PathBuf,
        // bind address (loopback by default; think twice before widening)
        #[arg(long, default_value = "127.0.0.1")]
        addr: String,
        // port to listen on (0 picks a free port, printed on startup)
        #[arg(long, default_value_t = 6767)]
        port: u16,
        // seconds between file-watch polls; the map auto-refreshes on change
        #[arg(long, default_value_t = 2)]
        watch_interval: u64,
        // disable file watching (rescan only via POST /refresh)
        #[arg(long)]
        no_watch: bool,
        // also serve the human-facing insights UI at /insights
        #[arg(long)]
        html: bool,
    },
    // analyse the project and emit the insights payload
    Insights {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_name = "FILE")]
        html: Option<PathBuf>,
        // base ref for the test-trigger diff
        #[arg(long)]
        base: Option<String>,
    },
    // install this `ccc` binary onto your PATH (Linux; defaults to ~/.local/bin)
    Install {
        // directory to install into (default: ~/.local/bin)
        #[arg(long)]
        dir: Option<PathBuf>,
        // overwrite an existing `ccc` in the target directory
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
        Command::Export {
            path,
            name,
            repo,
            out,
        } => {
            let root = canonical(&path);
            let files = codecache::scan::collect_files(&root)?;
            let caches = codecache::scan::build_caches(&root, &files);
            let label = name.unwrap_or_else(|| path_str(&root));
            let mut surface = codecache::Surface::from_caches(
                &label,
                &codecache::render::now_ts(),
                &caches,
            );
            surface.repo = repo;
            let body = serde_json::to_string_pretty(&surface)?;

            match out.as_deref() {
                Some(p) if p == Path::new("-") => println!("{body}"),
                other => {
                    let target = match other {
                        Some(p) => p.to_path_buf(),
                        None => root
                            .join(".ccc")
                            .join(codecache::externals::SURFACE_NAME),
                    };
                    if let Some(dir) = target.parent() {
                        std::fs::create_dir_all(dir)
                            .with_context(|| format!("creating {}", dir.display()))?;
                    }
                    std::fs::write(&target, format!("{body}\n"))
                        .with_context(|| format!("writing {}", target.display()))?;
                    eprintln!(
                        "{}: {} provided, {} consumed -> {}",
                        surface.name,
                        surface.provides.len(),
                        surface.consumes.len(),
                        target.display()
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Tokenize { path, encoding } => {
            let root = canonical(&path);
            run_tokenize(&root, &encoding)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Changes {
            path,
            base,
            services,
            format,
            fail_untested,
            init,
            worktree,
            html,
            from,
        } => {
            let root = canonical(&path);
            if init {
                let cfg = codecache::init_config(&root)?;
                println!("Wrote {} - edit the service globs, then re-run `ccc changes`", cfg.display());
                return Ok(ExitCode::SUCCESS);
            }

            // render the HTML view from a saved report, no analysis
            if let Some(from) = from {
                let html_path = html.expect("clap enforces --from requires --html");
                let raw = std::fs::read_to_string(&from)
                    .with_context(|| format!("reading {}", from.display()))?;
                let report: serde_json::Value = serde_json::from_str(&raw)
                    .with_context(|| format!("parsing {} as a changes report", from.display()))?;
                codecache::html::write_changes_html(&html_path, &report, &html_title(&html_path))?;
                println!("Wrote {}", html_path.display());
                return Ok(ExitCode::SUCCESS);
            }
            let service_flags = services
                .iter()
                .map(|s| {
                    s.split_once('=')
                        .map(|(n, g)| (n.trim().to_string(), g.trim().to_string()))
                        .filter(|(n, g)| !n.is_empty() && !g.is_empty())
                        .ok_or_else(|| anyhow!("--service wants NAME=GLOB, got '{s}'"))
                })
                .collect::<Result<Vec<_>>>()?;
            let opts = ChangesOptions {
                worktree,
                base,
                service_flags,
            };
            let report = codecache::changes(&root, &path_str(&path), &opts)?;
            if let Some(html_path) = &html {
                let value = serde_json::to_value(&report)?;
                codecache::html::write_changes_html(html_path, &value, &html_title(html_path))?;
                // stderr so stdout stays pure JSON for pipelines
                eprintln!("wrote {}", html_path.display());
            }
            match format {
                OutputFormat::Json => println!("{}", serde_json::to_string(&report)?),
                OutputFormat::Text => print_changes_text(&report),
            }
            if fail_untested && !report.untested.is_empty() {
                eprintln!(
                    "changes: {} changed function(s) with no detected test reference",
                    report.untested.len()
                );
                return Ok(ExitCode::FAILURE);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Serve {
            path,
            addr,
            port,
            watch_interval,
            no_watch,
            html,
        } => {
            let watch = if no_watch || watch_interval == 0 {
                None
            } else {
                Some(std::time::Duration::from_secs(watch_interval))
            };
            let opts = codecache::ServeOptions { addr, port, watch, html };
            codecache::serve(&canonical(&path), &opts)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Insights { path, html, base } => {
            let root = canonical(&path);
            let label = root
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(".")
                .to_string();
            let report = codecache::insights::analyse(&root, &label, base.as_deref())?;
            match html {
                Some(file) => {
                    codecache::html::write_insights_html(&file, &label, &report)?;
                    println!("wrote {}", file.display());
                }
                None => println!("{}", serde_json::to_string(&report)?),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Install { dir, force } => run_install(dir, force),
    }
}

fn print_changes_text(r: &ChangesReport) {
    println!(
        "changes: {} service(s), base {} ({}..{})",
        r.services.len(),
        r.base,
        &r.base_sha[..r.base_sha.len().min(9)],
        &r.head_sha[..r.head_sha.len().min(9)]
    );
    println!(
        "changed: {} file(s), {} function(s)",
        r.counts.changed_files, r.counts.changed_functions
    );
    for e in &r.edges {
        // declared and detected are independent: declaring a dep never skips
        // the analysis, so an edge is often both
        let kind = match (e.declared, e.detected) {
            (true, true) => "declared+detected",
            (true, false) => "declared, no calls found",
            _ => "detected",
        };
        // name the evidence, so a reader can judge the edge
        let syms: Vec<String> = e
            .symbols
            .iter()
            .map(|s| format!("{} via {}", s.symbol, s.via))
            .collect();
        let syms = if syms.is_empty() {
            String::new()
        } else {
            format!(" ({})", syms.join(", "))
        };
        println!("edge: {} -> {} [{kind}]{syms}", e.from, e.to);
    }
    println!("test: {}", r.services_to_test.join(", "));
    for f in r.changed_functions.iter().filter(|f| !f.tested_by.is_empty()) {
        println!(
            "covered: {}::{} L{}-{} (tested by: {})",
            f.file,
            f.function,
            f.lines[0],
            f.lines[1],
            f.tested_by.join(", ")
        );
    }
    for f in &r.untested {
        println!(
            "untested: {}::{} L{}-{} (called from: {})",
            f.file,
            f.function,
            f.lines[0],
            f.lines[1],
            if f.called_from.is_empty() {
                "-".to_string()
            } else {
                f.called_from.join(", ")
            }
        );
    }
    for u in &r.unresolved_calls {
        println!(
            "unresolved: {}::{} at {}:{} [{}]{}",
            u.from,
            u.symbol,
            u.file,
            u.line,
            u.reason,
            if u.candidates.is_empty() {
                String::new()
            } else {
                format!(" candidates: {}", u.candidates.join(", "))
            }
        );
    }
    if !r.unassigned_files.is_empty() {
        println!("unassigned: {}", r.unassigned_files.join(", "));
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

// Emit `{ root, up_to_date, files[], changes[] }` as one JSON line. `files` is
// the repo-relative paths of the changed cache entries — ready to hand to
// another GitHub Action via `fromJSON(...)`.
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

// Join `rest` onto `root`, dropping a leading `./` so paths read cleanly
// (root "." + ".ccc/CCC.md" -> ".ccc/CCC.md").
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

// Path as a forward-slash string (stable for CI output regardless of platform).
fn path_str(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

// title for a generated HTML report: the output file's stem
// (`ccc-changes-rust.html` -> `ccc-changes-rust`)
fn html_title(p: &Path) -> String {
    p.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("ccc-changes")
        .to_string()
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

// Copy the running `ccc` binary into a directory on the user's PATH.
///
// Defaults to `~/.local/bin` — the standard user-local bin dir on Linux, so no
// root/sudo is needed. Warns (but still succeeds) if the target dir isn't on
// `$PATH` so the user knows the shell won't find `ccc` until they add it.
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

// `~/.local/bin` — the XDG user-local binary directory on Linux.
fn default_bin_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow!("HOME is not set; pass --dir to choose an install directory"))?;
    Ok(PathBuf::from(home).join(".local").join("bin"))
}

// Expand a leading `~` (or `~/`) to `$HOME`; leave other paths untouched.
fn expand_tilde(p: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    p.to_path_buf()
}

// True if both paths resolve to the same existing file.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

// True if `dir` is one of the entries in `$PATH`.
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
