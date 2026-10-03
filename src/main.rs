use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use codecache::{CheckReport, Encoding, ChangesOptions, ChangesReport};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "ccc",
    about = "Map a project for agents and CI: serve it, diff it, or write it out",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    // read every `ccc:skip` as an ordinary comment, so the code it marks is
    // mapped, searched, measured and edited like the rest - any command
    #[arg(long, global = true)]
    ignore_skip: bool,
}

const CACHE_DIR: &str = ".ccc";

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    // Human-readable summary (default).
    Text,
    // Machine-readable JSON: { root, up_to_date, files[], changes[] }.
    Json,
}

#[derive(Subcommand)]
enum Command {
    // parse a project and report what the map holds. Writes nothing without
    // `--dir`: the map is what every other command reads, and it is built in
    // memory whether or not a copy is left on disk
    Scan {
        #[arg(default_value = ".")]
        path: PathBuf,
        // Also write the markdown cache. Bare `--dir` writes `<PATH>/.ccc`;
        // `--dir=DIR` writes there instead, relative to `PATH`. The value needs
        // `=` so a bare `--dir` cannot swallow the path argument after it.
        #[arg(long, value_name = "DIR", num_args = 0..=1, require_equals = true,
              default_missing_value = CACHE_DIR)]
        dir: Option<PathBuf>,
        // pre-encode the written markdown into a token stream. Needs `--dir`:
        // the stream is an encoding of those files, so they have to exist
        #[arg(long, requires = "dir")]
        tokens: bool,
        #[arg(long, default_value = "o200k_base")]
        encoding: String,
    },
    // Deprecated, and hidden from `--help` because of it: still callable so the
    // pipelines built on it keep passing, but no longer offered. It verifies a
    // written cache, and a written cache is now the exception - `ccc scan`
    // builds the map in memory and only `--dir` puts a copy on disk for
    // something outside `ccc` to read.
    #[command(hide = true)]
    Check {
        #[arg(default_value = ".")]
        path: PathBuf,
        // the cache to verify, relative to `PATH` (default `.ccc`)
        #[arg(long, value_name = "DIR", default_value = CACHE_DIR)]
        dir: PathBuf,
        // output format: `text` (default) or `json` (changed files as an array)
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    Tokenize {
        #[arg(default_value = ".")]
        path: PathBuf,
        // the cache to encode, relative to `PATH` (default `.ccc`)
        #[arg(long, value_name = "DIR", default_value = CACHE_DIR)]
        dir: PathBuf,
        #[arg(long, default_value = "o200k_base")]
        encoding: String,
    },
    // Set `.ccc/` up: a starter service map, and the surface other repos
    // consume. Both come out of one parse of the tree, and both are things a
    // person edits afterwards rather than output to be regenerated blindly.
    Init {
        #[arg(default_value = ".")]
        path: PathBuf,
        // name other repos will know this one by; defaults to the directory
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        // "owner/repo", recorded for display on the consuming side
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
        // write the surface here instead of `<path>/.ccc/surface.json`.
        // `-` writes it to stdout
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
        // Moved to `ccc init`. Kept hidden only so the old spelling fails with
        // somewhere to go: unlike the other retired flags this one *did*
        // something, and quietly accepting it would hand back a change report
        // where a scaffolded config was asked for
        #[arg(long, hide = true)]
        init: bool,
        // include uncommitted edits and untracked files in the diff. CI wants
        // the committed view (the default); a local run usually wants this
        #[arg(long)]
        worktree: bool,
        // name the request behind each change
        #[arg(long)]
        prompts: bool,
        #[arg(long)]
        deps: bool,
        // narrow the output to what this branch did to the OpenTelemetry
        #[arg(long)]
        telemetry: bool,
        // render one section as markdown for an agent
        #[arg(long)]
        markdown: bool,
        // exit non-zero when the change introduces an advisory
        #[arg(long)]
        fail_introduced: bool,
        // also write a single-file HTML view of the report (Tailwind + HTMX
        // live-query panel against `ccc run`), e.g. ccc-changes-rust.html
        #[arg(long, value_name = "FILE")]
        html: Option<PathBuf>,
        // render --html from an existing changes JSON report instead of running
        // the analysis (no git needed)
        #[arg(long, value_name = "REPORT.json", requires = "html")]
        from: Option<PathBuf>,
    },
    // what this branch did to the dependency tree
    Deps {
        #[arg(default_value = ".")]
        path: PathBuf,
        // base ref to diff against
        #[arg(long)]
        base: Option<String>,
        // include uncommitted edits and untracked files in the diff
        #[arg(long)]
        worktree: bool,
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
        // render the dependency delta as markdown for an agent
        #[arg(long)]
        markdown: bool,
        // exit non-zero when the change introduces an advisory
        #[arg(long)]
        fail_introduced: bool,
    },
    Prompts {
        #[arg(default_value = ".")]
        path: PathBuf,
        // base ref to diff against
        #[arg(long)]
        base: Option<String>,
        #[arg(long, value_name = "NAME")]
        agent: Option<String>,
        // restrict to exactly one session id: the caller's own, self-reported
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        // self-reported model name; only takes effect paired with --session
        #[arg(long, value_name = "NAME")]
        model: Option<String>,
        #[arg(long, value_name = "DAYS")]
        since: Option<u64>,
        #[arg(long)]
        worktree: bool,
        #[arg(long)]
        record: bool,
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
    },
    // markdown a PR description can be built from 
    // the same base/agent/session/model filters apply
    PrSummary {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        base: Option<String>,
        #[arg(long, value_name = "NAME")]
        agent: Option<String>,
        #[arg(long, value_name = "ID")]
        session: Option<String>,
        #[arg(long, value_name = "NAME")]
        model: Option<String>,
        #[arg(long, value_name = "DAYS")]
        since: Option<u64>,
        #[arg(long)]
        worktree: bool,
    },
    // run the code map over HTTP for AI agents and people: REST endpoints
    // (/find /references /dependencies ...), an MCP endpoint at /mcp, and the
    // insights UI at /insights. `serve` was the original name; kept so existing
    // scripts and editor integrations keep working
    #[command(alias = "serve")]
    Run {
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
        // The insights UI is served either way now. Accepted so the scripts and
        // editor integrations that pass it keep working, but it turns nothing on
        #[arg(long)]
        html: bool,
        // leave the human-facing UI off and serve the agent endpoints alone
        #[arg(long, conflicts_with = "html")]
        no_html: bool,
        #[arg(long)]
        deps: bool,
        // serve the architecture visualiser at /vis - C4-style levels from the
        // system down to one function's logic - and open it in the browser
        #[arg(long, conflicts_with = "no_html")]
        vis: bool,
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
    // scan this project's own source for security findings
    Sast {
        #[arg(default_value = ".")]
        path: PathBuf,
        // include test files, which normally carry fixtures rather than leaks
        #[arg(long)]
        include_tests: bool,
        // only report findings at or above this severity
        #[arg(long, default_value = "low")]
        min_severity: String,
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    // resolve dependencies from lockfiles and check them against the OSV advisory database
    Audit {
        #[arg(default_value = ".")]
        path: PathBuf,
        // resolve only; never reach for the advisory database
        #[arg(long)]
        offline: bool,
        // let a dev/build-only advisory fail the run too
        #[arg(long)]
        dev: bool,
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    // install this `ccc` binary onto your PATH (Linux; defaults to ~/.local/bin)
    Install {
        // Directory to install into, positionally: `ccc install ~/bin`. Every
        // other command takes its path this way, and naming a destination is
        // the only argument this one has.
        #[arg(value_name = "DIR")]
        path: Option<PathBuf>,
        // the same directory as a flag, for anyone already spelling it out
        #[arg(long, value_name = "DIR", conflicts_with = "path")]
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

// ccc:skip
fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    // before anything is parsed, so every map this process builds agrees
    codecache::extract::ignore_skips(cli.ignore_skip);
    match cli.command {
        Command::Scan {
            path,
            dir,
            tokens,
            encoding,
        } => {
            let root = canonical(&path);
            // a relative `--dir` is relative to what was scanned, so the same
            // flag means the same place whatever directory it was run from
            let out = dir.map(|d| root.join(d));
            let report = codecache::scan(&root, out.as_deref())?;
            let t = report.totals;
            let mapped = format!(
                "{} files: {} funcs, {} consts, {} refs, {} notes",
                report.files, t.funcs, t.consts, t.refs, t.notes
            );
            match &report.out_dir {
                Some(d) => println!("Mapped {mapped}\nWrote {}", d.display()),
                // nothing was written, and saying so is the difference between
                // "it worked" and "where did my files go"
                None => println!("Mapped {mapped} (in memory; --dir writes the markdown)"),
            }
            if tokens {
                let d = out.as_deref().expect("clap enforces --tokens requires --dir");
                // exactly what was written above, not a re-read of it
                run_tokenize(&report.rendered, d, &encoding)?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Check { path, dir, format } => {
            // stderr, so a `--format json` pipeline reading stdout is unaffected
            eprintln!(
                "warning: `ccc check` is deprecated and will be removed. It verifies a written \
                 cache, and `ccc scan` no longer writes one unless `--dir` asks. If you still \
                 commit a cache, regenerate it with `ccc scan --dir` and let your VCS report the \
                 diff; everything else reads the map `ccc` builds in memory."
            );
            // Canonicalize for the check itself (so results match `scan`, which
            // does the same), but keep the original `path` for building the
            // repo-relative cache paths reported in JSON.
            let root = canonical(&path);
            #[allow(deprecated)]
            let report = codecache::check(&root, &root.join(&dir))?;
            match format {
                OutputFormat::Text => print_check_text(&report, &dir),
                OutputFormat::Json => print_check_json(&path, &dir, &report)?,
            }
            if report.up_to_date {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::FAILURE)
            }
        }
        Command::Init {
            path,
            name,
            repo,
            out,
        } => {
            let root = canonical(&path);
            // The service map first: it is the file a person is meant to edit,
            // and an existing one is their work, not something to overwrite.
            match codecache::changes::ChangesConfig::path(&root) {
                Some(existing) => println!("kept {}", existing.display()),
                None => {
                    let cfg = codecache::init_config(&root)?;
                    println!(
                        "Wrote {} - edit the service globs, then re-run `ccc changes`",
                        cfg.display()
                    );
                }
            }

            let files = codecache::scan::collect_files(&root)?;
            let caches = codecache::scan::build_caches(&root, &files);
            let label = name.unwrap_or_else(|| path_str(&root));
            // what the schemas tie this repo to is published with the rest
            let contracts = codecache::contracts::ContractIndex::for_root(&root, &caches);
            let mut surface = codecache::Surface::from_caches(
                &label,
                &codecache::render::now_ts(),
                &caches,
                &contracts,
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
                    println!(
                        "Wrote {} - {} provided, {} consumed",
                        target.display(),
                        surface.provides.len(),
                        surface.consumes.len(),
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Tokenize { path, dir, encoding } => {
            let root = canonical(&path);
            // the map, built here and now - not read back off a `.ccc` that may
            // not exist and may not match the source if it does
            let files = codecache::scan::collect_files(&root)?;
            let caches = codecache::scan::build_caches(&root, &files);
            let corpus =
                codecache::scan::render_all(&root, &caches, &codecache::render::now_ts());
            run_tokenize(&corpus, &root.join(dir), &encoding)?;
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
            prompts,
            deps,
            telemetry,
            markdown,
            fail_introduced,
            html,
            from,
        } => {
            let _ = deps;
            let root = canonical(&path);
            if init {
                return Err(anyhow!(
                    "`ccc changes --init` has moved to `ccc init`, which writes the same \
                     `.ccc/map.json` and the surface beside it"
                ));
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
                prompts,
                deps: true,
            };
            let report = codecache::changes(&root, &path_str(&path), &opts)?;
            if let Some(html_path) = &html {
                let value = serde_json::to_value(&report)?;
                codecache::html::write_changes_html(html_path, &value, &html_title(html_path))?;
                // stderr so stdout stays pure JSON for pipelines
                eprintln!("wrote {}", html_path.display());
            }

            match (telemetry, markdown, format) {
                (true, true, _) => {
                    print!(
                        "{}",
                        codecache::telemetry::markdown(&report.telemetry, &report.base)
                    )
                }
                (true, false, OutputFormat::Json) => {
                    println!("{}", serde_json::to_string(&report.telemetry)?)
                }
                (true, false, OutputFormat::Text) => print!(
                    "{}",
                    codecache::telemetry::text(&report.telemetry, &report.base)
                ),
                (false, true, _) => {
                    let d = report.deps.as_ref().expect("`changes` always computes the delta");
                    print!("{}", codecache::deps::markdown(d, &report.base));
                }
                (false, false, OutputFormat::Json) => {
                    println!("{}", serde_json::to_string(&report)?)
                }
                (false, false, OutputFormat::Text) => print_changes_text(&report),
            }
            if fail_untested && !report.untested.is_empty() {
                eprintln!(
                    "changes: {} changed function(s) with no detected test reference",
                    report.untested.len()
                );
                return Ok(ExitCode::FAILURE);
            }
            if fail_introduced {
                let d = report.deps.as_ref().expect("`changes` always computes the delta");
                if gate_introduced("changes", d) {
                    return Ok(ExitCode::FAILURE);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Deps {
            path,
            base,
            worktree,
            format,
            markdown,
            fail_introduced,
        } => {
            let root = canonical(&path);
            let (base_label, report) =
                codecache::changes::deps_report(&root, base.as_deref(), worktree)?;
            match (markdown, format) {
                (true, _) => print!("{}", codecache::deps::markdown(&report, &base_label)),
                (false, OutputFormat::Json) => println!("{}", serde_json::to_string(&report)?),
                (false, OutputFormat::Text) => {
                    print!("{}", codecache::deps::text(&report, &base_label))
                }
            }
            if fail_introduced && gate_introduced("deps", &report) {
                return Ok(ExitCode::FAILURE);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Prompts {
            path,
            base,
            agent,
            session,
            model,
            since,
            worktree,
            record,
            format,
        } => {
            validate_agent(agent.as_deref())?;
            validate_model_pairing(&model, &session)?;
            let root = canonical(&path);
            let opts = codecache::PromptsOptions {
                base,
                worktree,
                agent,
                since_days: since,
                record,
                session,
                model,
            };
            let report = codecache::prompts(&root, &path_str(&path), &opts)?;
            match format {
                OutputFormat::Json => println!("{}", serde_json::to_string(&report)?),
                OutputFormat::Text => print_prompts_text(&report),
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::PrSummary {
            path,
            base,
            agent,
            session,
            model,
            since,
            worktree,
        } => {
            validate_agent(agent.as_deref())?;
            validate_model_pairing(&model, &session)?;
            let root = canonical(&path);
            let opts = codecache::PromptsOptions {
                base,
                worktree,
                agent,
                since_days: since,
                record: false,
                session,
                model,
            };
            let report = codecache::prompts(&root, &path_str(&path), &opts)?;
            print!("{}", codecache::pr_summary::markdown(&report, None, 0));
            Ok(ExitCode::SUCCESS)
        }
        Command::Run {
            path,
            addr,
            port,
            watch_interval,
            no_watch,
            html,
            no_html,
            deps,
            vis,
        } => {
            let watch = if no_watch || watch_interval == 0 {
                None
            } else {
                Some(std::time::Duration::from_secs(watch_interval))
            };
            // neither flag turns anything on; only `--no-html` turns one off
            let _ = (deps, html);
            let opts = codecache::ServeOptions {
                addr,
                port,
                watch,
                html: !no_html,
                vis,
            };
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
        Command::Sast { path, include_tests, min_severity, format } => {
            let root = canonical(&path);
            let report = codecache::sast::analyse(&root, include_tests);
            let floor = match min_severity.to_ascii_lowercase().as_str() {
                "high" => codecache::sast::Severity::High,
                "medium" | "moderate" => codecache::sast::Severity::Medium,
                _ => codecache::sast::Severity::Low,
            };
            let shown: Vec<&codecache::sast::Finding> =
                report.findings.iter().filter(|f| f.severity <= floor).collect();
            match format {
                OutputFormat::Text => print_sast_text(&report, &shown),
                OutputFormat::Json => println!("{}", serde_json::to_string(&report)?),
            }
            // only a high finding fails the run, so CI does not break on a checksum
            let high = shown
                .iter()
                .filter(|f| f.severity == codecache::sast::Severity::High)
                .count();
            Ok(if high > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS })
        }
        Command::Audit { path, offline, dev, format } => {
            let root = canonical(&path);
            let mut report = codecache::audit::resolve(&root);
            if !offline {
                codecache::audit::assess(&mut report);
            }
            codecache::audit::locate(&root, &mut report);
            match format {
                OutputFormat::Text => print_audit_text(&report, offline),
                OutputFormat::Json => println!("{}", serde_json::to_string(&report)?),
            }
            // a runtime advisory fails the run so CI can gate on it; dev ones only with --dev
            let gating = if dev {
                report.findings.len()
            } else {
                report.runtime_findings().len()
            };
            Ok(if gating > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS })
        }
        Command::Install { path, dir, force } => run_install(path.or(dir), force),
    }
}

// shared by `prompts` and `pr-summary`
fn validate_agent(agent: Option<&str>) -> Result<()> {
    match agent {
        Some(a) if !matches!(a, "claude" | "copilot") => {
            Err(anyhow!("--agent wants `claude` or `copilot`, got '{a}'"))
        }
        _ => Ok(()),
    }
}

fn validate_model_pairing(model: &Option<String>, session: &Option<String>) -> Result<()> {
    if model.is_some() && session.is_none() {
        return Err(anyhow!("--model only takes effect paired with --session"));
    }
    Ok(())
}

// `--fail-introduced` shared by `changes` and `deps` when the
// run should fail, with the reason already on stderr
fn gate_introduced(cmd: &str, d: &codecache::deps::DepsReport) -> bool {
    if !d.gates() {
        return false;
    }
    match d.error.as_deref() {
        // "we could not check" is not "it is fine"
        Some(err) => eprintln!("{cmd}: the dependency delta was not assessed - {err}"),
        None => {
            eprintln!("{cmd}: this branch introduces advisories the base did not carry:");
            for f in &d.introduced {
                eprintln!(
                    "  {} {} - {} {}",
                    f.advisory.id, f.advisory.severity, f.package.name, f.package.version
                );
            }
        }
    }
    true
}

fn print_sast_text(r: &codecache::sast::SastReport, shown: &[&codecache::sast::Finding]) {
    use codecache::sast::Severity;
    println!(
        "security: {} finding(s) across {} file(s) - {} rule(s) applied",
        shown.len(),
        r.files_scanned,
        r.rules.len()
    );
    println!(
        "  high {}, medium {}, low {}",
        r.by_severity(Severity::High),
        r.by_severity(Severity::Medium),
        r.by_severity(Severity::Low)
    );
    if shown.is_empty() {
        println!("\nnothing matched - these are syntax-level rules, not a proof of safety");
        return;
    }
    for f in shown {
        println!(
            "\n  [{}] {} {}:{} in {}",
            f.severity.as_str(),
            f.rule,
            f.file,
            f.line,
            f.function
        );
        println!("        {}", f.message);
        println!("        evidence: {}", f.evidence);
        println!("        {} - {}", f.cwe, f.hint);
    }
    println!("\nevery finding is a syntax match with no data flow behind it - confirm in the source");
}

fn print_audit_text(r: &codecache::audit::AuditReport, offline: bool) {
    println!(
        "dependencies: {} resolved from {} lockfile(s), {} direct",
        r.packages.len(),
        r.lockfiles.len(),
        r.direct_count()
    );
    for lock in &r.lockfiles {
        let n = r.packages.iter().filter(|p| &p.lockfile == lock).count();
        println!("  {lock} - {n} package(s)");
    }
    if !r.unresolved.is_empty() {
        // a coverage gap is not a clean result, so it is never left implicit
        println!("\nnot resolved ({}):", r.unresolved.len());
        for u in &r.unresolved {
            println!("  {} - {}", u.manifest, u.reason);
        }
    }
    if r.packages.is_empty() {
        println!("\nno lockfile found - run your package manager so versions can be resolved");
        return;
    }
    if offline {
        println!("\nadvisory database not consulted (--offline)");
        return;
    }
    if let Some(err) = &r.error {
        // the resolution above still stands; only the assessment is missing
        println!("\nvulnerabilities: not assessed - {err}");
        return;
    }
    let runtime = r.runtime_findings().len();
    let dev = r.findings.len() - runtime;
    if r.findings.is_empty() {
        println!("\nvulnerabilities: none known against {} package(s)", r.packages.len());
        return;
    }
    println!("\nvulnerabilities: {} ({runtime} runtime, {dev} dev-only)", r.findings.len());
    for f in &r.findings {
        let p = &f.package;
        let scope = if p.dev { ", dev" } else { "" };
        let reach = if p.direct { "direct" } else { "transitive" };
        println!(
            "\n  [{}] {} {} ({}{scope}, {reach})",
            f.advisory.severity.to_uppercase(),
            p.name,
            p.version,
            p.ecosystem.label()
        );
        println!("        {}", f.advisory.summary);
        match &f.advisory.fixed {
            Some(v) => println!("        fixed in {v}"),
            None => println!("        no fixed version published"),
        }
        println!("        {} {}", f.advisory.id, f.advisory.url);
    }
}

fn print_prompts_text(r: &codecache::PromptsReport) {
    let c = &r.counts;
    println!(
        "prompts: {} request(s) against {} (claude {}, copilot {}), base {}",
        c.turns, r.root, c.claude_turns, c.copilot_turns, r.base
    );
    for s in &r.sources {
        println!("source: {} - {} session(s) at {}", s.agent, s.sessions, s.location);
    }
    println!(
        "attributed: {} changed file(s), {} unexplained",
        c.attributed_files, c.unattributed_files
    );
    // the requests once, numbered
    let slot: std::collections::BTreeMap<&str, usize> = r
        .turns
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id.as_str(), i + 1))
        .collect();
    for (i, t) in r.turns.iter().enumerate() {
        let edits = match t.edits.len() {
            0 => " (changed nothing)".to_string(),
            n => format!(" ({n} edit(s))"),
        };
        let agent = match &t.model {
            Some(m) => format!("{} [{m}]", t.agent),
            None => t.agent.clone(),
        };
        println!("#{} {} {}{edits}: {}", i + 1, agent, t.ts, t.prompt);
    }
    for (path, refs) in &r.attributed {
        // the evidence is part of the answer, not a footnote: a temporal match
        // is a guess and should read as one
        let cited: Vec<String> = refs
            .iter()
            .map(|p| {
                let span = match p.lines {
                    Some([s, e]) => format!("L{s}-{e} "),
                    None => String::new(),
                };
                let n = slot.get(p.turn.as_str()).copied().unwrap_or(0);
                format!("{span}[{}] #{n}", p.evidence)
            })
            .collect();
        println!("{path}: {}", cited.join(", "));
    }
    for path in &r.unattributed {
        println!("unexplained: {path}");
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
    if let Some(d) = &r.deps {
        print!("{}", codecache::deps::text(d, &r.base));
    }
    if r.telemetry.instrumented || r.telemetry.error.is_some() {
        print!("{}", codecache::telemetry::text(&r.telemetry, &r.base));
    }
}

fn print_check_text(report: &CheckReport, dir: &Path) {
    let name = path_str(dir);
    if report.up_to_date {
        println!("{name} is up to date");
    } else {
        // name the flag that writes, because a bare `ccc scan` no longer does
        let flag = if dir == Path::new(CACHE_DIR) {
            "--dir".to_string()
        } else {
            format!("--dir={name}")
        };
        eprintln!("{name} is out of date; run `ccc scan {flag}`:");
        for c in &report.changes {
            eprintln!("  {:9} {}", format!("{}:", c.kind.as_str()), c.file);
        }
    }
}

// Emit `{ root, up_to_date, files[], changes[] }` as one JSON line. `files` is
// the repo-relative paths of the changed cache entries — ready to hand to
// another GitHub Action via `fromJSON(...)`.
fn print_check_json(root: &Path, dir: &Path, report: &CheckReport) -> Result<()> {
    let ccc_rel = rel_join(root, dir);
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

// `corpus` is the rendered map to encode and `ccc` where the stream lands -
// beside the markdown when there is any, on its own when there is not
fn run_tokenize(
    corpus: &std::collections::BTreeMap<String, String>,
    ccc: &Path,
    encoding: &str,
) -> Result<()> {
    let enc = Encoding::parse(encoding)
        .ok_or_else(|| anyhow!("unknown encoding '{encoding}' (use o200k_base or cl100k_base)"))?;
    let report = codecache::tokenize(corpus, ccc, enc)?;
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
