//! Where an agent broke ccc's rules, read from its own transcripts.
//!
//! ccc asks agents to explore the project through its tools, to write it only
//! through its edit tools, and to reach the web only through `search`. Every
//! tool call in a transcript is checked against those rules, and each one that
//! broke one is reported with the call, the files or urls it touched, and the
//! ccc tool that does that job.
//!
//! Claude Code's transcripts only, a session's subagents with it: Copilot's
//! chat log keeps no record of its tool calls.

use crate::inspect::{assignment, inside, parts, PATTERNED, READERS, WRAPPERS};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

pub const SCHEMA: &str = "ccc-violations/1";
// the most of a command or input one violation quotes
const QUOTE_CAP: usize = 240;
// the files or urls one violation names
const TARGET_CAP: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Rule {
    WritesOutsideCcc,
    ShellExploration,
    WebOutsideSearch,
    BlockedUrl,
}

impl Rule {
    pub const ALL: [Rule; 4] = [Rule::WritesOutsideCcc, Rule::ShellExploration, Rule::WebOutsideSearch, Rule::BlockedUrl];

    pub fn label(self) -> &'static str {
        match self {
            Rule::WritesOutsideCcc => "writes-outside-ccc",
            Rule::ShellExploration => "shell-exploration",
            Rule::WebOutsideSearch => "web-outside-search",
            Rule::BlockedUrl => "blocked-url",
        }
    }

    pub fn parse(s: &str) -> Option<Rule> {
        Rule::ALL.into_iter().find(|r| r.label() == s.trim())
    }

    // what the rule asks of an agent
    pub fn says(self) -> &'static str {
        match self {
            Rule::WritesOutsideCcc => "project files are written only through ccc's edit tools",
            Rule::ShellExploration => "the project is read and searched only through ccc's tools",
            Rule::WebOutsideSearch => "the web is reached only through ccc's `search`",
            Rule::BlockedUrl => "a url the search rules refuse is not to be reached",
        }
    }

    // the ccc tool that does the job instead
    pub fn instead(self) -> &'static str {
        match self {
            Rule::WritesOutsideCcc => "edit_text, or edit_rename / edit_replace / edit_delete / edit_insert on a find or references handle, then edit_apply",
            Rule::ShellExploration => "index, find, references, file, dependencies or notes",
            Rule::WebOutsideSearch => "search",
            Rule::BlockedUrl => "nothing - the user's search rules refuse it",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Violation {
    pub rule: Rule,
    // when the call was made, rfc 3339
    pub at: String,
    pub session: String,
    // `claude`, or `claude subagent` for a call a subagent made
    pub agent: String,
    pub tool: String,
    // the command line or the input, cut short
    pub call: String,
    // project-relative paths, or urls
    pub targets: Vec<String>,
    pub transcript: String,
    // the transcript line the call is on, 1-based
    pub line: usize,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub schema: &'static str,
    pub root: String,
    pub transcripts: usize,
    pub sessions: usize,
    pub calls: usize,
    pub counts: BTreeMap<&'static str, usize>,
    pub violations: Vec<Violation>,
    pub note: &'static str,
}

#[derive(Debug, Default, Clone)]
pub struct Options {
    // only calls from the last N days
    pub since_days: Option<u64>,
    pub session: Option<String>,
    // only these rules; none means every rule
    pub rules: Vec<Rule>,
}

const NOTE: &str = "Read from Claude Code's transcripts, subagents included - Copilot's chat log keeps no tool calls. \
    Shell lines are read as written: a path or url held in a variable cannot be checked, and loopback \
    requests (ccc's own server) are not web access.";

// every transcript of this project, each tool call in it checked against the rules
pub fn check(root: &Path, opts: &Options) -> Report {
    let files = crate::inspect::transcripts(root);
    let cutoff_ms = opts.since_days.map(|d| chrono::Utc::now().timestamp_millis() - (d as i64) * 86_400_000);
    let mut scan = Scan { root, opts, cutoff_ms, calls: 0, sessions: BTreeSet::new(), out: Vec::new() };
    for f in &files {
        scan.file(f);
    }
    scan.out.sort_by(|a, b| (&a.at, &a.transcript, a.line).cmp(&(&b.at, &b.transcript, b.line)));
    let mut counts: BTreeMap<&'static str, usize> = Rule::ALL.iter().map(|r| (r.label(), 0)).collect();
    for v in &scan.out {
        *counts.entry(v.rule.label()).or_default() += 1;
    }
    Report {
        schema: SCHEMA,
        root: crate::changes::path_str(root),
        transcripts: files.len(),
        sessions: scan.sessions.len(),
        calls: scan.calls,
        counts,
        violations: scan.out,
        note: NOTE,
    }
}

struct Scan<'a> {
    root: &'a Path,
    opts: &'a Options,
    cutoff_ms: Option<i64>,
    calls: usize,
    sessions: BTreeSet<String>,
    out: Vec<Violation>,
}

// where a call was made, kept until its answer is read
struct Pending {
    at: String,
    session: String,
    tool: String,
    call: String,
    line: usize,
}

impl Scan<'_> {
    fn file(&mut self, path: &Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let transcript = path.to_string_lossy().into_owned();
        let agent = if transcript.contains("/subagents/") { "claude subagent" } else { "claude" };
        // `search` calls waiting for their answers, by call id
        let mut searches: BTreeMap<String, Pending> = BTreeMap::new();
        for (n, line) in text.lines().enumerate() {
            // a tool's answer can run to megabytes - only calls, and the answers to searches, are parsed
            let call = line.contains("\"tool_use\"") && line.contains("\"type\":\"assistant\"");
            let answer = !searches.is_empty() && line.contains("\"tool_result\"") && searches.keys().any(|id| line.contains(id.as_str()));
            if !call && !answer {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let at = rec["timestamp"].as_str().unwrap_or_default().to_string();
            let session = rec["sessionId"].as_str().unwrap_or_default().to_string();
            if !self.wanted(&at, &session) {
                continue;
            }
            let cwd = rec["cwd"].as_str().map_or_else(|| self.root.to_path_buf(), PathBuf::from);
            for b in rec["message"]["content"].as_array().into_iter().flatten() {
                match b["type"].as_str() {
                    Some("tool_use") => {
                        let tool = b["name"].as_str().unwrap_or_default();
                        let input = &b["input"];
                        self.calls += 1;
                        self.sessions.insert(session.clone());
                        let quoted = quote(tool, input);
                        if tool.starts_with("mcp__") && tool.ends_with("__search") {
                            if let Some(id) = b["id"].as_str() {
                                let pending = Pending { at: at.clone(), session: session.clone(), tool: tool.to_string(), call: quoted.clone(), line: n + 1 };
                                searches.insert(id.to_string(), pending);
                            }
                        }
                        for (rule, targets) in classify(tool, input, &cwd, self.root) {
                            self.push(rule, &at, &session, agent, tool, &quoted, targets, &transcript, n + 1);
                        }
                    }
                    Some("tool_result") => {
                        let Some(p) = b["tool_use_id"].as_str().and_then(|id| searches.remove(id)) else {
                            continue;
                        };
                        let refused = refused_urls(&result_text(&b["content"]));
                        if !refused.is_empty() {
                            self.push(Rule::BlockedUrl, &p.at, &p.session, agent, &p.tool, &p.call, refused, &transcript, p.line);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn wanted(&self, at: &str, session: &str) -> bool {
        if self.opts.session.as_deref().is_some_and(|s| s != session) {
            return false;
        }
        let Some(cutoff) = self.cutoff_ms else {
            return true;
        };
        chrono::DateTime::parse_from_rfc3339(at).is_ok_and(|t| t.timestamp_millis() >= cutoff)
    }

    #[allow(clippy::too_many_arguments)]
    fn push(&mut self, rule: Rule, at: &str, session: &str, agent: &str, tool: &str, call: &str, mut targets: Vec<String>, transcript: &str, line: usize) {
        if !self.opts.rules.is_empty() && !self.opts.rules.contains(&rule) {
            return;
        }
        targets.dedup();
        targets.truncate(TARGET_CAP);
        self.out.push(Violation {
            rule,
            at: at.to_string(),
            session: session.to_string(),
            agent: agent.to_string(),
            tool: tool.to_string(),
            call: call.to_string(),
            targets,
            transcript: transcript.to_string(),
            line,
        });
    }
}

// the part of a call worth quoting: a shell line, a path, a url, or the input itself
fn quote(tool: &str, input: &Value) -> String {
    let text = ["command", "file_path", "notebook_path", "url", "query", "pattern", "path"]
        .iter()
        .find_map(|k| input[k].as_str().map(str::to_string))
        .unwrap_or_else(|| input.to_string());
    let text = text.replace('\n', " ⏎ ");
    match text.char_indices().nth(QUOTE_CAP) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None if text.is_empty() => tool.to_string(),
        None => text,
    }
}

// a tool answer's text, whether it came as a string or as content blocks
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

// the urls a `search` answer says it refused
fn refused_urls(text: &str) -> Vec<String> {
    text.match_indices(crate::search::REFUSED)
        .filter_map(|(at, m)| text[at + m.len()..].split_whitespace().next())
        .map(str::to_string)
        .collect()
}

// the rules one tool call broke, each with what it touched
fn classify(tool: &str, input: &Value, cwd: &Path, root: &Path) -> Vec<(Rule, Vec<String>)> {
    let path = |k: &str| input[k].as_str().and_then(|p| project_path(root, &cwd.join(p))).into_iter().collect::<Vec<_>>();
    let one = |rule: Rule, targets: Vec<String>| if targets.is_empty() { Vec::new() } else { vec![(rule, targets)] };
    match tool {
        "Edit" | "Write" | "MultiEdit" => one(Rule::WritesOutsideCcc, path("file_path")),
        "NotebookEdit" => one(Rule::WritesOutsideCcc, path("notebook_path")),
        "Read" => one(Rule::ShellExploration, path("file_path")),
        "NotebookRead" => one(Rule::ShellExploration, path("notebook_path")),
        // a search with no path searches where the agent stands
        "Grep" | "Glob" | "LS" => {
            let at = input["path"].as_str().map_or_else(|| cwd.to_path_buf(), |p| cwd.join(p));
            one(Rule::ShellExploration, project_path(root, &at).into_iter().collect())
        }
        "WebFetch" => vec![(Rule::WebOutsideSearch, input["url"].as_str().map(str::to_string).into_iter().collect())],
        "WebSearch" => vec![(Rule::WebOutsideSearch, input["query"].as_str().map(|q| format!("query: {q}")).into_iter().collect())],
        "Bash" => input["command"].as_str().map(|c| shell(c, cwd, root)).unwrap_or_default(),
        t if t.starts_with("mcp__") && !t.starts_with("mcp__ccc__") && web_tool(t) => {
            vec![(Rule::WebOutsideSearch, input["url"].as_str().map(str::to_string).into_iter().collect())]
        }
        _ => Vec::new(),
    }
}

// another mcp server's tool that reaches the web
fn web_tool(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["fetch", "browse", "browser", "scrape", "crawl", "web", "http"].iter().any(|w| n.contains(w))
}

// a path inside the project, relative to it - `.` for the project itself, never one under `.git`, which is git's own
fn project_path(root: &Path, path: &Path) -> Option<String> {
    let mut abs = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                abs.pop();
            }
            Component::CurDir => {}
            c => abs.push(c),
        }
    }
    if abs == root {
        return Some(".".to_string());
    }
    inside(root, &abs).filter(|p| p != ".git" && !p.starts_with(".git/"))
}

// commands that write the files they name
const WRITERS: &[&str] = &["tee", "rm", "rmdir", "unlink", "touch", "truncate", "shred"];
// commands that list or search a tree, the current directory when they name none
const WALKERS: &[&str] = &["find", "fd", "fdfind", "ls", "tree", "rg", "ag", "du"];
// commands that reach the network
const FETCHERS: &[&str] = &["curl", "wget", "http", "https", "xh", "httpie", "aria2c", "lynx", "w3m", "links"];
// git subcommands that write the working tree
const GIT_WRITES: &[&str] = &["apply", "am", "restore", "mv", "rm"];

// the rules one shell line broke - each simple command in it read on its own
fn shell(line: &str, cwd: &Path, root: &Path) -> Vec<(Rule, Vec<String>)> {
    let mut cwd = cwd.to_path_buf();
    let mut writes = Vec::new();
    let mut reads = Vec::new();
    let mut web = Vec::new();
    for part in parts(line) {
        // a path held in a variable or a substitution names nothing that can be checked
        let at = |p: &str, cwd: &Path| {
            let full = cwd.join(p);
            (!full.to_string_lossy().contains(['$', '`'])).then(|| project_path(root, &full)).flatten()
        };
        writes.extend(part.written.iter().filter_map(|w| at(w, &cwd)));
        let mut words = part.words.iter().map(String::as_str).skip_while(|w| assignment(w) || WRAPPERS.contains(w));
        let Some(cmd) = words.next() else {
            continue;
        };
        let args: Vec<&str> = words.collect();
        let name = cmd.rsplit('/').next().unwrap_or(cmd);
        let plain: Vec<&str> = args.iter().copied().filter(|a| !a.starts_with('-')).collect();
        match name {
            "cd" => {
                if let Some(dir) = args.first() {
                    cwd = cwd.join(dir);
                }
            }
            "sed" if args.iter().any(|a| a.starts_with("-i") || a.starts_with("--in-place")) => {
                let given = args.iter().any(|a| matches!(*a, "-e" | "-f" | "--expression" | "--file"));
                writes.extend(plain.iter().skip(usize::from(!given)).filter_map(|f| at(f, &cwd)));
            }
            "perl" if args.iter().any(|a| perl_in_place(a)) => {
                let script = args.iter().position(|a| *a == "-e" || *a == "-E").map(|i| i + 1);
                let files = args.iter().enumerate().filter(|(i, a)| !a.starts_with('-') && Some(*i) != script);
                writes.extend(files.filter_map(|(_, f)| at(f, &cwd)));
            }
            "cp" | "install" | "ln" => writes.extend(plain.last().and_then(|f| at(f, &cwd))),
            "mv" => writes.extend(plain.iter().filter_map(|f| at(f, &cwd))),
            "dd" => writes.extend(args.iter().filter_map(|a| a.strip_prefix("of=")).filter_map(|f| at(f, &cwd))),
            "patch" => writes.extend(at(".", &cwd)),
            "git" => {
                let sub = plain.first().copied().unwrap_or_default();
                let reverts = sub == "checkout" && args.contains(&"--");
                if GIT_WRITES.contains(&sub) || reverts {
                    let named: Vec<String> = plain.iter().skip(1).filter_map(|f| at(f, &cwd)).collect();
                    writes.extend(if named.is_empty() { at(".", &cwd).into_iter().collect() } else { named });
                }
            }
            n if WRITERS.contains(&n) => writes.extend(plain.iter().filter_map(|f| at(f, &cwd))),
            n if FETCHERS.contains(&n) => web.extend(args.iter().filter(|a| !a.starts_with('-')).filter(|a| remote_url(a)).map(|a| a.to_string())),
            n if READERS.contains(&n) || WALKERS.contains(&n) => {
                // a search's first word is its pattern, unless a flag gave it
                let given = args.iter().any(|a| matches!(*a, "-e" | "-f" | "--regexp" | "--expression" | "--file"));
                let pattern = usize::from(PATTERNED.contains(&n) && !given);
                let named: Vec<&str> = match n {
                    // find's paths come before its expression
                    "find" => args.iter().copied().take_while(|a| !a.starts_with('-') && !matches!(*a, "(" | "!")).collect(),
                    _ => plain.iter().copied().filter(|a| !a.bytes().all(|b| b.is_ascii_digit())).skip(pattern).collect(),
                };
                let files: Vec<String> = named.iter().copied().chain(part.fed.iter().map(String::as_str)).filter_map(|f| at(f, &cwd)).collect();
                if !files.is_empty() {
                    reads.extend(files);
                } else if named.is_empty() && part.fed.is_empty() && !part.piped && walks(n, &args) {
                    // nothing named and nothing piped in: it walks where it stands
                    reads.extend(at(".", &cwd));
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for (rule, targets) in [(Rule::WritesOutsideCcc, writes), (Rule::ShellExploration, reads), (Rule::WebOutsideSearch, web)] {
        if !targets.is_empty() {
            out.push((rule, targets));
        }
    }
    out
}

// a command that searches the tree it stands in when it names no path
fn walks(name: &str, args: &[&str]) -> bool {
    WALKERS.contains(&name)
        || (matches!(name, "grep" | "egrep" | "fgrep") && args.iter().any(|a| *a == "--recursive" || (a.starts_with('-') && !a.starts_with("--") && (a.contains('r') || a.contains('R')))))
}

// `-i`, `-pi`, `-i.bak` - perl editing in place; letters after one that takes an argument are that argument
fn perl_in_place(arg: &str) -> bool {
    let Some(flags) = arg.strip_prefix('-').filter(|f| !f.starts_with('-')) else {
        return false;
    };
    for c in flags.chars() {
        match c {
            'i' => return true,
            'e' | 'E' | 'M' | 'm' | 'I' | 'x' | 'C' | 'd' | 'D' => return false,
            _ => {}
        }
    }
    false
}

// a literal url, or bare host, that is not this machine
fn remote_url(word: &str) -> bool {
    let rest = match word.split_once("://") {
        Some((_, rest)) => rest,
        None if word.starts_with("www.") => word,
        None => {
            // `example.com/path`, which curl reads as a url too - a bare `payer.jar` is a file
            let Some((host, _)) = word.split_once('/') else {
                return false;
            };
            let host = host.split(':').next().unwrap_or_default();
            let labels: Vec<&str> = host.split('.').collect();
            let named = labels.len() > 1 && labels.iter().all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
            if !named || !labels.last().is_some_and(|tld| tld.len() > 1 && tld.chars().all(|c| c.is_ascii_alphabetic())) {
                return false;
            }
            word
        }
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = if host.starts_with('[') { host.split(']').next().unwrap_or_default().trim_start_matches('[') } else { host.split(':').next().unwrap_or_default() };
    let host = host.to_ascii_lowercase();
    !(host.is_empty() || host == "localhost" || host.ends_with(".localhost") || host.starts_with("127.") || host == "::1" || host == "0.0.0.0" || host.contains('$'))
}

// the report as a person reads it on a terminal
pub fn text(r: &Report) -> String {
    let mut out = summary(r);
    for v in &r.violations {
        out.push_str(&format!("{} {} [{}] {} `{}`", short_at(&v.at), short_session(&v.session), v.rule.label(), v.tool, v.call));
        if !v.targets.is_empty() {
            out.push_str(&format!(" -> {}", v.targets.join(", ")));
        }
        out.push_str(&format!("\n    {}:{}\n", v.transcript, v.line));
    }
    out
}

// the report for an agent: grouped by rule, each with what to use instead, paged
pub fn markdown(r: &Report, offset: usize, limit: usize) -> String {
    let mut out = format!("# violations\n{}", summary(r));
    let total = r.violations.len();
    let start = offset.min(total);
    let end = (start + limit.max(1)).min(total);
    if start > 0 || end < total {
        let more = if end < total { format!(", pass offset={end} for the next page") } else { String::new() };
        out.push_str(&format!("(showing {}-{end} of {total}{more})\n", start + 1));
    }
    let mut rule = None;
    for v in &r.violations[start..end] {
        if rule != Some(v.rule) {
            rule = Some(v.rule);
            out.push_str(&format!("\n## {} - {}; use {} instead\n", v.rule.label(), v.rule.says(), v.rule.instead()));
        }
        out.push_str(&format!("- {} {} {} `{}`", short_at(&v.at), short_session(&v.session), v.tool, v.call));
        if !v.targets.is_empty() {
            out.push_str(&format!(" -> {}", v.targets.join(", ")));
        }
        out.push('\n');
    }
    out.push_str(&format!("\n{}\n", r.note));
    out
}

fn summary(r: &Report) -> String {
    let counts: Vec<String> = r.counts.iter().map(|(k, n)| format!("{k} {n}")).collect();
    format!(
        "{} violation(s) in {} tool call(s) across {} session(s), {} transcript(s)\n{}\n",
        r.violations.len(),
        r.calls,
        r.sessions,
        r.transcripts,
        counts.join(" | ")
    )
}

fn short_at(at: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(at).map_or_else(|_| at.to_string(), |t| t.format("%Y-%m-%d %H:%M").to_string())
}

fn short_session(s: &str) -> &str {
    s.get(..8).unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules(line: &str) -> Vec<(Rule, Vec<String>)> {
        shell(line, Path::new("/p"), Path::new("/p"))
    }

    fn rule_of(line: &str) -> Vec<Rule> {
        rules(line).into_iter().map(|(r, _)| r).collect()
    }

    #[test]
    fn a_write_around_the_edit_tools_is_caught_however_it_is_spelled() {
        for line in [
            "sed -i 's/a/b/' src/x.rs",
            "perl -pi -e 's/a/b/' src/x.rs",
            "echo hi > src/x.rs",
            "cat /tmp/a >> docs/notes.md",
            "printf x | tee src/x.rs",
            "cp /tmp/x src/x.rs",
            "mv src/a.rs src/b.rs",
            "rm -f src/x.rs",
            "git apply /tmp/fix.patch",
            "git checkout -- src/x.rs",
        ] {
            assert_eq!(rule_of(line), vec![Rule::WritesOutsideCcc], "{line}");
        }
        assert_eq!(rules("sed -i 's/a/b/' src/x.rs")[0].1, vec!["src/x.rs"]);
    }

    #[test]
    fn writes_outside_the_project_or_to_a_stream_are_not_violations() {
        for line in ["echo hi > /tmp/x", "cargo build 2>&1 | tail", "make 2>/dev/null", "cp src/x.rs /tmp/x", "git commit -m 'x'", "echo x > $S/sid", "cd $S && rm out.log"] {
            assert!(!rule_of(line).contains(&Rule::WritesOutsideCcc), "{line}");
        }
    }

    #[test]
    fn exploring_with_the_shell_is_caught_but_filtering_a_pipe_is_not() {
        for line in ["cat src/main.rs", "sed -n '1,40p' src/main.rs", "grep -rn insights src", "grep -rn insights", "rg foo", "find . -name '*.rs'", "ls docs", "head -20 README.md"] {
            assert_eq!(rule_of(line), vec![Rule::ShellExploration], "{line}");
        }
        for line in ["cargo test | grep FAILED", "git log | head", "ls /tmp", "cat /etc/hosts", "echo x | wc -l"] {
            assert!(rule_of(line).is_empty(), "{line}");
        }
    }

    #[test]
    fn the_web_is_flagged_but_loopback_is_ccc_itself() {
        assert_eq!(rule_of("curl -s https://example.com/x"), vec![Rule::WebOutsideSearch]);
        assert_eq!(rule_of("wget docs.example.org/page"), vec![Rule::WebOutsideSearch]);
        for line in ["curl -s http://127.0.0.1:6767/mcp", "curl localhost:3000", "curl -s $BASE/mcp", "curl -s -X POST \"$URL\"", "curl -sL -o payer.jar $JAR", "curl -D hdr.txt $U"] {
            assert!(rule_of(line).is_empty(), "{line}");
        }
    }

    #[test]
    fn a_cd_moves_where_relative_paths_land() {
        assert!(rule_of("cd /tmp && cat notes.txt").is_empty());
        assert_eq!(rules("cd src && cat main.rs")[0].1, vec!["src/main.rs"]);
    }

    #[test]
    fn agent_tools_are_classified_by_what_they_touch() {
        let (cwd, root) = (Path::new("/p"), Path::new("/p"));
        let c = |tool: &str, input: Value| classify(tool, &input, cwd, root).into_iter().map(|(r, _)| r).collect::<Vec<_>>();
        assert_eq!(c("Edit", json!({"file_path": "/p/src/x.rs"})), vec![Rule::WritesOutsideCcc]);
        assert!(c("Write", json!({"file_path": "/tmp/scratch.md"})).is_empty());
        assert_eq!(c("Read", json!({"file_path": "/p/README.md"})), vec![Rule::ShellExploration]);
        assert_eq!(c("Grep", json!({"pattern": "x"})), vec![Rule::ShellExploration]);
        assert_eq!(c("WebFetch", json!({"url": "https://example.com"})), vec![Rule::WebOutsideSearch]);
        assert_eq!(c("mcp__fetch__fetch", json!({"url": "https://example.com"})), vec![Rule::WebOutsideSearch]);
        assert!(c("mcp__ccc__find", json!({"query": "x"})).is_empty());
        assert!(c("mcp__ccc__search", json!({"url": "https://example.com"})).is_empty());
    }

    #[test]
    fn a_transcript_yields_its_violations_and_its_refused_searches() {
        let dir = std::env::temp_dir().join(format!("ccc-violations-{}", std::process::id()));
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let file = dir.join("t.jsonl");
        let call = |id: &str, name: &str, input: Value| {
            json!({"type": "assistant", "timestamp": "2026-10-10T12:00:00Z", "sessionId": "s1", "cwd": root,
                   "message": {"content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}})
            .to_string()
        };
        let answer = |id: &str, text: &str| {
            json!({"type": "user", "sessionId": "s1", "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": text}]}}).to_string()
        };
        let lines = [
            call("a", "Bash", json!({"command": "sed -i 's/x/y/' src/lib.rs"})),
            call("b", "mcp__ccc__search", json!({"url": "https://blocked.example/"})),
            answer("b", &format!("error: {} https://blocked.example/ - not on the allow list", crate::search::REFUSED)),
            call("c", "mcp__ccc__find", json!({"query": "x"})),
        ];
        std::fs::write(&file, lines.join("\n") + "\n").unwrap();
        let mut scan = Scan { root: &root, opts: &Options::default(), cutoff_ms: None, calls: 0, sessions: BTreeSet::new(), out: Vec::new() };
        scan.file(&file);
        let found: Vec<(Rule, Vec<String>)> = scan.out.iter().map(|v| (v.rule, v.targets.clone())).collect();
        assert_eq!(
            found,
            vec![
                (Rule::WritesOutsideCcc, vec!["src/lib.rs".to_string()]),
                (Rule::BlockedUrl, vec!["https://blocked.example/".to_string()]),
            ]
        );
        assert_eq!(scan.calls, 3);
        assert_eq!(scan.out[1].line, 2, "a refusal points at the call that asked");

        // a rule filter and a time window narrow it
        let only = Options { rules: vec![Rule::BlockedUrl], ..Default::default() };
        let mut scan = Scan { root: &root, opts: &only, cutoff_ms: Some(i64::MAX), calls: 0, sessions: BTreeSet::new(), out: Vec::new() };
        scan.file(&file);
        assert!(scan.out.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
