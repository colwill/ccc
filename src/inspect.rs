//! What agents read with their own tools - a `Read`, a `cat` or a `sed -n` in
//! a shell - taken from their transcripts as they are written. ccc's read tools
//! put their looks on the timeline as they answer; these are the rest, so the
//! visualiser shows an agent inspecting code whichever way it looked.
//!
//! Claude Code's transcripts only, a session's subagents with it: Copilot's
//! chat log keeps no record of the files its tools read.

use crate::model::FileCache;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

// transcripts are looked for again this often - a new session, a subagent
const FOUND_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
// and this often when claude keeps no directory for the project, where
// finding them means opening every other project's
const FOUND_SLOW: std::time::Duration = std::time::Duration::from_secs(600);
// the sites one read names past the file itself
const READ_SITES: usize = 59;

// commands that read the files they name and write none
const READERS: &[&str] = &[
    "cat", "head", "tail", "sed", "awk", "nl", "less", "more", "bat", "batcat", "view", "grep", "egrep", "fgrep", "rg", "ag",
    "wc", "cut", "sort", "uniq", "diff", "strings", "xxd", "od", "jq",
];
// readers whose first word is a pattern or a script rather than a file
const PATTERNED: &[&str] = &["sed", "awk", "grep", "egrep", "fgrep", "rg", "ag", "jq"];
// words ahead of a command that only change how it runs
const WRAPPERS: &[&str] = &["sudo", "env", "time", "command", "nohup", "exec", "nice"];

// One file an agent read with its own tools.
#[derive(Debug, Clone, PartialEq)]
pub struct Read {
    pub at_ms: u64,
    pub session: String,
    // the tool call it was, so it is put down once
    pub call: String,
    // Read, NotebookRead, Grep, Bash
    pub tool: String,
    // project-relative
    pub path: String,
    // 1-based and inclusive, ending at `usize::MAX` for the rest of the file -
    // none for the whole of it
    pub lines: Option<(usize, usize)>,
    // why it was read - the description the agent gave the call, else what it last said
    pub why: Option<String>,
}

impl Read {
    // the file and the lines read, as the timeline labels it
    pub fn what(&self) -> String {
        match self.lines {
            None => self.path.clone(),
            Some((a, b)) if a == b => format!("{}:{a}", self.path),
            Some((a, usize::MAX)) => format!("{}:{a}-", self.path),
            Some((a, b)) => format!("{}:{a}-{b}", self.path),
        }
    }

    // Where on the map it was read, as (file, line - none for the file, main):
    // the one function the lines lie in, else the file and each function they
    // cross.
    pub fn sites(&self, caches: &[FileCache]) -> Vec<(String, usize, bool)> {
        let whole = vec![(self.path.clone(), 0, true)];
        let cache = caches.iter().find(|c| crate::changes::path_str(&c.rel_path) == self.path);
        let (Some((from, to)), Some(c)) = (self.lines, cache) else {
            return whole;
        };
        let holding = c
            .funcs
            .iter()
            .filter(|f| f.start_line <= from && to <= f.end_line)
            .min_by_key(|f| f.end_line - f.start_line);
        if let Some(f) = holding {
            return vec![(self.path.clone(), f.line, true)];
        }
        let crossed = c.funcs.iter().filter(|f| f.start_line <= to && from <= f.end_line);
        whole.into_iter().chain(crossed.take(READ_SITES).map(|f| (self.path.clone(), f.line, false))).collect()
    }
}

// The transcripts of this project's agents, each read as far as it was last.
#[derive(Default)]
pub struct Reads {
    // when the transcripts were last looked for
    found: Option<std::time::Instant>,
    // each, and the bytes of it read so far
    read: BTreeMap<PathBuf, u64>,
    // what each session's agent last said - the reason for the reads that follow it
    said: BTreeMap<String, String>,
}

impl Reads {
    // The reads agents made since the last call, oldest first.
    pub fn fresh(&mut self, root: &Path) -> Vec<Read> {
        let quick = crate::prompts::claude_root().is_some_and(|b| b.join(crate::prompts::claude_slug(root)).is_dir());
        let every = if quick { FOUND_EVERY } else { FOUND_SLOW };
        if self.found.is_none_or(|t| t.elapsed() >= every) {
            let found = transcripts(root);
            self.found_now(found);
        }
        self.tail(root)
    }

    // What a transcript held when it was first found is history; one begun
    // since is news from its first line.
    fn found_now(&mut self, found: Vec<PathBuf>) {
        let first = self.found.is_none();
        self.read.retain(|f, _| found.contains(f));
        for f in found {
            let from = if first { std::fs::metadata(&f).map_or(0, |m| m.len()) } else { 0 };
            self.read.entry(f).or_insert(from);
        }
        self.found = Some(std::time::Instant::now());
    }

    fn tail(&mut self, root: &Path) -> Vec<Read> {
        let mut out = Vec::new();
        for (f, from) in self.read.iter_mut() {
            let (records, end) = tail(f, *from);
            *from = end;
            for r in &records {
                let session = r["sessionId"].as_str().unwrap_or_default();
                if let Some(text) = narration(r) {
                    self.said.insert(session.to_string(), text);
                }
                out.extend(reads_of(r, root, self.said.get(session).map(String::as_str)));
            }
        }
        out.sort_by_key(|r| r.at_ms);
        out
    }
}

// claude's transcripts for this project, each session's subagents beside it
fn transcripts(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for main in crate::prompts::claude_transcripts(root).0 {
        out.extend(crate::prompts::jsonl_files(&main.with_extension("").join("subagents")));
        out.push(main);
    }
    out
}

// The records a transcript gained past byte `from` that hold a tool call, and
// the byte they end at. A line still being written waits for the next read; a
// transcript shorter than `from` began again, so it is read from the top.
fn tail(path: &Path, from: u64) -> (Vec<Value>, u64) {
    use std::io::{Read as _, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return (Vec::new(), from);
    };
    let from = if f.metadata().map_or(0, |m| m.len()) < from { 0 } else { from };
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(from)).is_err() || f.read_to_end(&mut buf).is_err() {
        return (Vec::new(), from);
    }
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let has = |l: &[u8], pat: &[u8]| l.windows(pat.len()).any(|w| w == pat);
    let records = buf[..end]
        .split(|&b| b == b'\n')
        // a tool's answer can run to megabytes - only the calls, and what the agent said, are parsed
        .filter(|l| has(l, b"\"tool_use\"") || (has(l, b"\"type\":\"assistant\"") && has(l, b"\"type\":\"text\"")))
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect();
    (records, from + end as u64)
}

// what an agent said in one transcript record, if it said anything
fn narration(rec: &Value) -> Option<String> {
    if rec["type"] != "assistant" {
        return None;
    }
    let text: Vec<&str> = rec["message"]["content"].as_array()?.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
    let said = crate::prompts::said(&text.join("\n"));
    (!said.is_empty()).then_some(said)
}

// the files inside the project one transcript record's tool calls read - `said` what its agent last said
fn reads_of(rec: &Value, root: &Path, said: Option<&str>) -> Vec<Read> {
    if rec["type"] != "assistant" {
        return Vec::new();
    }
    let at_ms = rec["timestamp"]
        .as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map_or(0, |d| d.timestamp_millis().max(0) as u64);
    let session = rec["sessionId"].as_str().unwrap_or_default();
    let cwd = rec["cwd"].as_str().map_or_else(|| root.to_path_buf(), PathBuf::from);
    let mut out = Vec::new();
    for b in rec["message"]["content"].as_array().into_iter().flatten() {
        let (Some("tool_use"), Some(tool), Some(call)) = (b["type"].as_str(), b["name"].as_str(), b["id"].as_str()) else {
            continue;
        };
        let input = &b["input"];
        // a shell call carries the agent's own description of it; any other takes what it last said
        let why = input["description"].as_str().map(crate::prompts::said).filter(|d| !d.is_empty()).or_else(|| said.map(str::to_string));
        let named: Vec<(PathBuf, Option<(usize, usize)>)> = match tool {
            "Read" => input["file_path"].as_str().map(|p| (cwd.join(p), read_range(input))).into_iter().collect(),
            "NotebookRead" => input["notebook_path"].as_str().map(|p| (cwd.join(p), None)).into_iter().collect(),
            // a search of one file looks at it; one of a directory names no file until it answers
            "Grep" => input["path"].as_str().map(|p| (cwd.join(p), None)).into_iter().collect(),
            "Bash" => input["command"].as_str().map(|c| shell_reads(c, &cwd)).unwrap_or_default(),
            _ => Vec::new(),
        };
        out.extend(named.into_iter().filter_map(|(p, lines)| {
            let path = inside(root, &p)?;
            Some(Read { at_ms, session: session.to_string(), call: call.to_string(), tool: tool.to_string(), path, lines, why: why.clone() })
        }));
    }
    out
}

// the lines a `Read` asked for - none when it asked for the whole file
fn read_range(input: &Value) -> Option<(usize, usize)> {
    let num = |v: &Value| v.as_u64().or_else(|| v.as_str()?.trim().parse().ok()).map(|n| n as usize);
    let (offset, limit) = (num(&input["offset"]), num(&input["limit"]));
    if offset.is_none() && limit.is_none() {
        return None;
    }
    let from = offset.unwrap_or(1).max(1);
    Some((from, limit.map_or(usize::MAX, |l| from + l.max(1) - 1)))
}

// an absolute path as a path inside the project, relative to it
fn inside(root: &Path, path: &Path) -> Option<String> {
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
    let rel = abs.strip_prefix(root).ok()?;
    rel.components().next().is_some().then(|| crate::changes::path_str(rel))
}

// The files a shell command line reads, each with the lines a `sed -n 'a,bp'`
// or a `head -n` took where it says. A word is taken for a file only where the
// command it follows reads files and writes none.
fn shell_reads(line: &str, cwd: &Path) -> Vec<(PathBuf, Option<(usize, usize)>)> {
    let mut cwd = cwd.to_path_buf();
    let mut out = Vec::new();
    for (words, fed) in commands(line) {
        let mut words = words.iter().map(String::as_str).skip_while(|w| assignment(w) || WRAPPERS.contains(w));
        let Some(cmd) = words.next() else {
            continue;
        };
        let args: Vec<&str> = words.collect();
        let name = cmd.rsplit('/').next().unwrap_or(cmd);
        if name == "cd" {
            if let Some(dir) = args.first() {
                cwd = cwd.join(dir);
            }
            continue;
        }
        let in_place = name == "sed" && args.iter().any(|a| a.starts_with("-i") || a.starts_with("--in-place"));
        if !READERS.contains(&name) || in_place {
            continue;
        }
        let lines = match name {
            "sed" => args.iter().find_map(|a| sed_range(a)),
            "head" => head_count(&args).map(|n| (1, n)),
            _ => None,
        };
        // a search's or an edit script's first word is its pattern, unless a flag gave it
        let given = args.iter().any(|a| matches!(*a, "-e" | "-f" | "--regexp" | "--expression" | "--file"));
        let pattern = usize::from(PATTERNED.contains(&name) && !given);
        let words = args.iter().copied().filter(|a| !a.starts_with('-') && !a.bytes().all(|b| b.is_ascii_digit()));
        let files = words.skip(pattern).chain(fed.iter().map(String::as_str));
        out.extend(files.map(|f| (cwd.join(f), lines)));
    }
    out
}

// `NAME=value` ahead of a command
fn assignment(word: &str) -> bool {
    word.split_once('=')
        .is_some_and(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

// the lines `a,bp` or `ap` prints under `sed -n`
fn sed_range(script: &str) -> Option<(usize, usize)> {
    let body = script.strip_suffix('p')?;
    let (a, b) = body.split_once(',').unwrap_or((body, body));
    let a: usize = a.parse().ok()?;
    let b = if b == "$" { usize::MAX } else { b.parse().ok()? };
    (a >= 1 && b >= a).then_some((a, b))
}

// how many lines a `head` took, where it says
fn head_count(args: &[&str]) -> Option<usize> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let n: Option<usize> = match *a {
            "-n" | "--lines" => it.next().and_then(|n| n.parse().ok()),
            a => a
                .strip_prefix("--lines=")
                .or_else(|| a.strip_prefix("-n"))
                .or_else(|| a.strip_prefix('-'))
                .and_then(|n| n.parse().ok()),
        };
        if n.is_some() {
            return n.filter(|&n| n > 0);
        }
    }
    None
}

// where the word being read goes
#[derive(Clone, Copy)]
enum Into {
    Arg,
    // fed to the command with `<`
    Fed,
    // written with `>` - no read
    Written,
}

// a command line on its way to its simple commands
struct Split {
    commands: Vec<(Vec<String>, Vec<String>)>,
    word: Option<String>,
    into: Into,
}

impl Split {
    fn push(&mut self, c: char) {
        self.word.get_or_insert_with(String::new).push(c);
    }

    fn word(&mut self) {
        let Some(w) = self.word.take() else {
            return;
        };
        let Some((args, fed)) = self.commands.last_mut() else {
            return;
        };
        match std::mem::replace(&mut self.into, Into::Arg) {
            Into::Arg => args.push(w),
            Into::Fed => fed.push(w),
            Into::Written => {}
        }
    }

    fn command(&mut self) {
        self.word();
        self.commands.push(Default::default());
    }
}

// A command line as its simple commands, each its words with the quotes taken
// off and the words a `<` feeds it apart. A heredoc's body is text, not
// commands, so the line is read no further than one.
fn commands(line: &str) -> Vec<(Vec<String>, Vec<String>)> {
    let mut s = Split { commands: vec![Default::default()], word: None, into: Into::Arg };
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                s.word.get_or_insert_with(String::new);
                for c in chars.by_ref().take_while(|&c| c != '\'') {
                    s.push(c);
                }
            }
            '"' => {
                s.word.get_or_insert_with(String::new);
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => chars.next().into_iter().for_each(|n| s.push(n)),
                        c => s.push(c),
                    }
                }
            }
            '\\' => chars.next().into_iter().filter(|&n| n != '\n').for_each(|n| s.push(n)),
            ' ' | '\t' => s.word(),
            '\n' | ';' | '|' | '&' | '(' | ')' => s.command(),
            '#' if s.word.is_none() => {
                chars.by_ref().take_while(|&c| c != '\n').for_each(drop);
                s.command();
            }
            '<' if chars.peek() == Some(&'<') => break,
            '<' => {
                s.word();
                s.into = Into::Fed;
            }
            '>' => {
                // the `2` of `2>` is the stream, not a word
                if s.word.as_deref().is_some_and(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_digit())) {
                    s.word = None;
                }
                s.word();
                while chars.next_if(|&c| c == '>' || c == '&').is_some() {}
                s.into = Into::Written;
            }
            c => s.push(c),
        }
    }
    s.command();
    s.commands.retain(|(args, fed)| !args.is_empty() || !fed.is_empty());
    s.commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn files(line: &str) -> Vec<(String, Option<(usize, usize)>)> {
        shell_reads(line, Path::new("/p/sub"))
            .into_iter()
            .map(|(p, lines)| (p.to_string_lossy().into_owned(), lines))
            .collect()
    }

    // A shell line reads the files its reading commands name - with the lines
    // a `sed -n` or `head` took - and nothing a command writes, runs or edits
    // in place, a heredoc's text included.
    #[test]
    fn a_shell_line_reads_what_its_readers_name() {
        assert_eq!(files("sed -n '120,180p' src/a.rs"), [("/p/sub/src/a.rs".into(), Some((120, 180)))]);
        assert_eq!(files("cd .. && head -n 40 lib.rs | grep fn"), [("/p/sub/../lib.rs".into(), Some((1, 40)))]);
        assert_eq!(
            files("grep -n \"fn main\" a.rs b.rs 2>/dev/null; cat < c.rs"),
            [("/p/sub/a.rs".into(), None), ("/p/sub/b.rs".into(), None), ("/p/sub/c.rs".into(), None)]
        );
        assert_eq!(files("LANG=C sudo cat /etc/x"), [("/etc/x".into(), None)]);
        assert!(files("sed -i 's/a/b/' a.rs").is_empty());
        assert!(files("cargo test --lib a.rs > out.txt").is_empty());
        assert!(files("cat > a.rs <<'EOF'\nfn x() {}\nEOF").is_empty());
        assert!(files("echo cat a.rs # cat b.rs").is_empty());
    }

    // A read is seen where it was taken: the one function its lines lie in,
    // else the file and the functions they cross - the whole file when it
    // asked for no lines.
    #[test]
    fn a_read_is_seen_at_the_function_its_lines_lie_in() {
        let src = "fn a() {\n    1;\n}\n\nfn b() {\n    2;\n}\n";
        let caches = vec![crate::scan::read_one(Path::new("src/x.rs"), src).unwrap()];
        let read = |lines| Read { at_ms: 0, session: "s".into(), call: "c".into(), tool: "Read".into(), path: "src/x.rs".into(), lines, why: None };
        assert_eq!(read(Some((2, 2))).sites(&caches), [("src/x.rs".to_string(), 1, true)]);
        assert_eq!(
            read(Some((2, 6))).sites(&caches),
            [("src/x.rs".to_string(), 0, true), ("src/x.rs".to_string(), 1, false), ("src/x.rs".to_string(), 5, false)]
        );
        assert_eq!(read(None).sites(&caches), [("src/x.rs".to_string(), 0, true)]);
        assert_eq!(read(Some((5, usize::MAX))).what(), "src/x.rs:5-");
    }

    // A transcript is read on from where it was last: what it held when it
    // was found is history, each call after is a read of what it named inside
    // the project, and a line still being written waits for the next look.
    #[test]
    fn a_transcripts_reads_are_found_as_they_are_written() {
        let dir = std::env::temp_dir().join(format!("ccc-inspect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let log = dir.join("s1.jsonl");
        let call = |id: &str, name: &str, input: Value| {
            json!({"type": "assistant", "timestamp": "2026-10-03T12:00:00.000Z", "sessionId": "s1", "cwd": root,
                   "message": {"content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}})
            .to_string()
                + "\n"
        };
        let mut text = call("old", "Read", json!({"file_path": root.join("a.rs")}));
        std::fs::write(&log, &text).unwrap();
        let mut reads = Reads::default();
        reads.found_now(vec![log.clone()]);
        assert!(reads.tail(&root).is_empty(), "history");

        // what the agent says before it reads is why it reads, unless the call describes itself
        text += &json!({"type": "assistant", "sessionId": "s1", "message": {"content": [{"type": "text", "text": "Reading **the parser** to see\nwhere it splits:"}]}}).to_string();
        text += "\n";
        text += &call("t1", "Read", json!({"file_path": root.join("src/a.rs"), "offset": 10, "limit": 5}));
        text += &call("t2", "Bash", json!({"command": "sed -n '1,3p' src/b.rs && cat /etc/hosts", "description": "Read b's head"}));
        text += &call("t3", "mcp__ccc__file", json!({"path": "src/c.rs"}));
        text += &json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "content": "x"}]}}).to_string();
        text += "\n";
        let half = call("t4", "Read", json!({"file_path": root.join("src/d.rs")}));
        std::fs::write(&log, format!("{text}{}", &half[..20])).unwrap();
        let found = reads.tail(&root);
        let got: Vec<(String, String, String)> = found.iter().map(|r| (r.call.clone(), r.tool.clone(), r.what())).collect();
        assert_eq!(got, [("t1".into(), "Read".into(), "src/a.rs:10-14".into()), ("t2".into(), "Bash".into(), "src/b.rs:1-3".into())]);
        let why: Vec<Option<&str>> = found.iter().map(|r| r.why.as_deref()).collect();
        assert_eq!(why, [Some("Reading the parser to see where it splits:"), Some("Read b's head")]);

        std::fs::write(&log, format!("{text}{half}")).unwrap();
        let got: Vec<String> = reads.tail(&root).into_iter().map(|r| r.what()).collect();
        assert_eq!(got, ["src/d.rs"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
