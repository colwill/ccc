//! Credentials recognised in plain text - prompts, an agent's words, diffs and
//! files of any kind. A replay redacts what this finds before a session is
//! saved beside the repository, and the branch scan warns before one is
//! committed or pushed. `sast` reads code literals with the same shapes.
//!
//! Like `sast` this is evidence, not proof: each finding says what it looks
//! like and where, its value redacted, so a person can confirm it.

use crate::sast::Severity;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};

// credentials that announce themselves by shape - prefix, what it is, and the least length the whole token runs to
pub(crate) const TOKEN_SHAPES: &[(&str, &str, usize)] = &[
    ("AKIA", "an AWS access key id", 20),
    ("ASIA", "an AWS temporary access key id", 20),
    ("ghp_", "a GitHub personal access token", 40),
    ("gho_", "a GitHub OAuth token", 40),
    ("ghs_", "a GitHub server token", 40),
    ("ghu_", "a GitHub user-to-server token", 40),
    ("ghr_", "a GitHub refresh token", 40),
    ("github_pat_", "a GitHub fine-grained token", 40),
    ("xoxb-", "a Slack bot token", 24),
    ("xoxp-", "a Slack user token", 24),
    ("xoxa-", "a Slack app token", 24),
    ("xapp-", "a Slack app-level token", 24),
    ("sk_live_", "a Stripe live secret key", 24),
    ("rk_live_", "a Stripe restricted key", 24),
    ("AIza", "a Google API key", 39),
    ("SG.", "a SendGrid API key", 40),
    ("glpat-", "a GitLab personal access token", 20),
    ("sk-ant-", "an Anthropic API key", 40),
    ("sk-proj-", "an OpenAI API key", 40),
    ("hf_", "a Hugging Face token", 37),
    ("npm_", "an npm access token", 40),
    ("pypi-AgEI", "a PyPI upload token", 60),
    ("dop_v1_", "a DigitalOcean token", 71),
    ("shpat_", "a Shopify access token", 38),
];

// names that make a literal beside them a credential rather than a string
pub(crate) const SECRET_NAMES: &[&str] = &[
    "password", "passwd", "pwd", "secret", "api_key", "apikey", "access_key", "token",
    "credential", "private_key", "auth", "passphrase", "client_secret",
];

// values that look like secrets but are placeholders
pub(crate) const PLACEHOLDERS: &[&str] = &[
    "changeme", "change_me", "password", "secret", "token", "example", "placeholder", "your",
    "xxx", "todo", "none", "null", "test", "dummy", "sample", "redacted", "hunter2",
];

// what a name ends in, its separators gone, when the value given to it is a credential - `GITHUB_TOKEN`, `db.password`, `clientSecret`
const CREDENTIAL_ENDINGS: &[&str] = &[
    "password", "passwd", "pwd", "secret", "apikey", "accesskey", "secretkey", "privatekey",
    "signingkey", "encryptionkey", "masterkey", "token", "credential", "credentials", "passphrase",
    "secretkeybase",
];

// a line that says a credential on it is meant to be there - ccc's own, and the ones other scanners already taught people
const ALLOW_MARKS: &[&str] = &["ccc:allow-secret", "pragma: allowlist secret", "gitleaks:allow"];

// past this a file is data, not something a person typed a key into
const MAX_FILE_BYTES: u64 = 1 << 20;

// one credential found in a text, by its byte range
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub start: usize,
    pub end: usize,
    pub what: &'static str,
    pub severity: Severity,
    // how much of its start says only what kind it is - a token's prefix - and may be shown
    shown: usize,
}

impl Hit {
    // the value as a report may show it: its kind's prefix, never the secret
    pub fn evidence(&self, text: &str) -> String {
        let value = &text[self.start..self.end];
        let n = value.chars().count();
        if self.shown == 0 {
            return format!("({n} chars, redacted)");
        }
        format!("{}... ({n} chars, redacted)", &value[..self.shown])
    }
}

// every credential in `text`, in order and never overlapping - a token inside an assignment is one secret
pub fn scan(text: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    pem(text, &mut hits);
    shaped(text, &mut hits);
    jwt(text, &mut hits);
    url_password(text, &mut hits);
    assigned(text, &mut hits);
    hits.sort_by_key(|h| (h.start, std::cmp::Reverse(h.end)));
    let mut out: Vec<Hit> = Vec::new();
    for h in hits {
        if out.last().map_or(true, |l| h.start >= l.end) {
            out.push(h);
        }
    }
    out
}

// `text` with each credential replaced by what it was, and what was taken out
pub fn redact(text: &str) -> (String, Vec<&'static str>) {
    let hits = scan(text);
    if hits.is_empty() {
        return (text.to_string(), Vec::new());
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for h in &hits {
        out.push_str(&text[at..h.start]);
        out.push_str(&marker(h.what));
        at = h.end;
    }
    out.push_str(&text[at..]);
    (out, hits.iter().map(|h| h.what).collect())
}

fn marker(what: &str) -> String {
    format!("[redacted: {what}]")
}

// Every string in `v` redacted in place, and what came out where. A list of
// strings - a diff's lines - is read as one text, so a key that spans lines
// is still found.
pub fn redact_json(v: &mut Value) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    walk(v, String::new(), &mut out);
    out
}

fn walk(v: &mut Value, at: String, out: &mut Vec<(String, &'static str)>) {
    match v {
        Value::String(s) => {
            let (clean, found) = redact(s);
            if !found.is_empty() {
                *s = clean;
                out.extend(found.into_iter().map(|w| (at.clone(), w)));
            }
        }
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_string) => {
            let lines: Vec<String> = items.iter().map(|i| i.as_str().unwrap_or_default().to_string()).collect();
            let joined = lines.join("\n");
            let hits = scan(&joined);
            if hits.is_empty() {
                return;
            }
            // each line keeps what no hit covers; a hit's marker goes on the line it starts in
            let mut start = 0;
            for (item, line) in items.iter_mut().zip(&lines) {
                let end = start + line.len();
                let mut kept = String::new();
                let mut at_ = start;
                for h in hits.iter().filter(|h| h.start < end.max(start + 1) && h.end > start) {
                    if h.start > at_ {
                        kept.push_str(&joined[at_..h.start.min(end)]);
                    }
                    if h.start >= start {
                        kept.push_str(&marker(h.what));
                    }
                    at_ = h.end.min(end);
                }
                if at_ < end {
                    kept.push_str(&joined[at_..end]);
                }
                *item = Value::String(kept);
                start = end + 1;
            }
            out.extend(hits.iter().map(|h| (at.clone(), h.what)));
        }
        Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                walk(item, format!("{at}[{i}]"), out);
            }
        }
        Value::Object(map) => {
            for (k, item) in map.iter_mut() {
                walk(item, if at.is_empty() { k.clone() } else { format!("{at}.{k}") }, out);
            }
        }
        _ => {}
    }
}

// the shape a whole literal has, if it is a credential's - what `sast` asks of a string in code
pub(crate) fn shape_of(v: &str) -> Option<&'static str> {
    TOKEN_SHAPES
        .iter()
        .find(|(prefix, _, min)| v.starts_with(prefix) && shape_ok(prefix, v, *min))
        .map(|(_, what, _)| *what)
}

fn shape_ok(prefix: &str, token: &str, min: usize) -> bool {
    let body = &token[prefix.len()..];
    token.len() >= min && body_ok(prefix, body) && entropy(body) >= 3.0
}

// what may follow a prefix: an AWS key id is upper-case letters and digits, a
// GitHub, npm or Hugging Face token letters and digits in both cases - which
// tells `hf_` from a snake_case name - and the rest the usual token characters
fn body_ok(prefix: &str, body: &str) -> bool {
    let b = body.as_bytes();
    match prefix {
        "AKIA" | "ASIA" => b.len() == 16 && b.iter().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
        "ghp_" | "gho_" | "ghs_" | "ghu_" | "ghr_" | "hf_" | "npm_" => {
            b.iter().all(u8::is_ascii_alphanumeric)
                && b.iter().any(u8::is_ascii_uppercase)
                && b.iter().any(u8::is_ascii_lowercase)
        }
        "SG." => b.iter().all(|&c| tok(c) || c == b'.'),
        _ => b.iter().all(|&c| tok(c)),
    }
}

// a byte a token is made of
fn tok(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn shaped(text: &str, out: &mut Vec<Hit>) {
    let b = text.as_bytes();
    for &(prefix, what, min) in TOKEN_SHAPES {
        for (start, _) in text.match_indices(prefix) {
            if start > 0 && tok(b[start - 1]) {
                continue;
            }
            let mut end = start + prefix.len();
            while end < b.len() && (tok(b[end]) || (prefix == "SG." && b[end] == b'.')) {
                end += 1;
            }
            if shape_ok(prefix, &text[start..end], min) {
                out.push(Hit { start, end, what, severity: Severity::High, shown: prefix.len() });
            }
        }
    }
}

// a private key, from its BEGIN line to its END line - or to the end of what was given, cut short
fn pem(text: &str, out: &mut Vec<Hit>) {
    const BEGIN: &str = "-----BEGIN ";
    let mut from = 0;
    while let Some(i) = text[from..].find(BEGIN) {
        let start = from + i;
        let label_at = start + BEGIN.len();
        let Some(close) = text[label_at..].find("-----") else { break };
        let label = &text[label_at..label_at + close];
        let body_at = label_at + close + 5;
        from = body_at;
        if !label.contains("PRIVATE KEY") || label.len() > 40 {
            continue;
        }
        if let Some(end) = pem_body(text, body_at) {
            out.push(Hit { start, end, what: "a private key", severity: Severity::High, shown: 0 });
            from = end;
        }
    }
}

// Where a key's body ends, if what follows its BEGIN line is one: lines of
// base64 - split by real newlines or the `\n` a key written into JSON carries -
// up to its END line, or to where it was cut short. A label mentioned in prose
// or code is followed by neither.
fn pem_body(text: &str, at: usize) -> Option<usize> {
    let base64 = |l: &str| l.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b));
    let mut pos = at;
    let mut encoded = 0;
    let mut last = at;
    let mut first = true;
    loop {
        let rest = &text[pos..];
        // the next break, real or escaped
        let (len, step) = match (rest.find('\n'), rest.find("\\n")) {
            (Some(a), Some(b)) if b < a => (b, 2),
            (Some(a), _) => (a, 1),
            (None, Some(b)) => (b, 2),
            (None, None) => (rest.len(), 0),
        };
        let line = rest[..len].trim_matches(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',');
        if first {
            // nothing may follow the BEGIN line on its own line
            if !line.is_empty() {
                return None;
            }
            first = false;
        } else if let Some(e) = rest[..len].find("-----END") {
            let end_at = pos + e + 8;
            return (encoded >= 64).then(|| text[end_at..].find("-----").map_or(text.len(), |k| end_at + k + 5));
        } else if base64(line) {
            encoded += line.len();
            if !line.is_empty() {
                last = pos + len;
            }
        } else if !(encoded == 0 && line.contains(": ")) {
            // prose or code where the body should go on: a key cut short, or none at all
            break;
        }
        if step == 0 {
            break;
        }
        pos += len + step;
    }
    (encoded >= 64).then_some(last)
}

// a JSON Web Token carries its own header, and a payload that reads the same way
fn jwt(text: &str, out: &mut Vec<Hit>) {
    let b = text.as_bytes();
    for (start, _) in text.match_indices("eyJ") {
        if start > 0 && (tok(b[start - 1]) || b[start - 1] == b'.') {
            continue;
        }
        let mut end = start;
        while end < b.len() && (tok(b[end]) || b[end] == b'.') {
            end += 1;
        }
        let token = &text[start..end];
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() >= 3 && parts[1].starts_with("eyJ") && parts[2].len() >= 16 && token.len() >= 40 {
            out.push(Hit { start, end, what: "a JSON Web Token", severity: Severity::Medium, shown: 3 });
        }
    }
}

// `scheme://user:password@host` - the password, not the address around it
fn url_password(text: &str, out: &mut Vec<Hit>) {
    let b = text.as_bytes();
    for (i, _) in text.match_indices("://") {
        if i == 0 || !b[i - 1].is_ascii_alphanumeric() {
            continue;
        }
        let rest = &text[i + 3..];
        let authority_end = rest
            .find(|c: char| c == '/' || c == '?' || c == '#' || c.is_whitespace() || "\"'`<>()".contains(c))
            .unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        let Some(at) = authority.rfind('@') else { continue };
        let Some(colon) = authority[..at].find(':') else { continue };
        let password = &authority[colon + 1..at];
        if value_ok(password, true) {
            let start = i + 3 + colon + 1;
            out.push(Hit { start, end: start + password.len(), what: "a password in a URL", severity: Severity::High, shown: 0 });
        }
    }
}

// a value given to a credential-shaped name - `API_TOKEN=...`, `"password": "..."`, `db.password: ...`
fn assigned(text: &str, out: &mut Vec<Hit>) {
    let b = text.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut i = 0;
    while i < b.len() {
        if !ident(b[i]) || (i > 0 && ident(b[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && (ident(b[i]) || ((b[i] == b'-' || b[i] == b'.') && i + 1 < b.len() && ident(b[i + 1]))) {
            i += 1;
        }
        let name: String = text[start..i].bytes().filter(u8::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase() as char).collect();
        if !CREDENTIAL_ENDINGS.iter().any(|e| name.ends_with(e)) {
            continue;
        }
        let mut j = i;
        // the closing quote of a quoted key
        if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
            j += 1;
        }
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        // `=`, `:`, `=>` or `:=` - and never `==` or `::`, which compare and qualify
        let op = match (b.get(j), b.get(j + 1)) {
            (Some(b'='), Some(b'=')) | (Some(b':'), Some(b':')) => continue,
            (Some(b'='), Some(b'>')) | (Some(b':'), Some(b'=')) => 2,
            (Some(b'=') | Some(b':'), _) => 1,
            _ => continue,
        };
        j += op;
        while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
            j += 1;
        }
        let quote = b.get(j).copied().filter(|q| matches!(q, b'"' | b'\'' | b'`'));
        let value_at = j + usize::from(quote.is_some());
        let mut k = value_at;
        match quote {
            Some(q) => {
                while k < b.len() && b[k] != q && b[k] != b'\n' {
                    k += 1;
                }
                if k >= b.len() || b[k] != q {
                    continue;
                }
            }
            None => {
                while k < b.len() && !b[k].is_ascii_whitespace() && !b",;)}]\"'`".contains(&b[k]) {
                    k += 1;
                }
            }
        }
        // a value in backticks is markdown's code, read as code is rather than as a quoted string
        if value_ok(&text[value_at..k], quote.is_some_and(|q| q != b'`')) {
            out.push(Hit { start: value_at, end: k, what: "a credential given to a name", severity: Severity::High, shown: 0 });
        }
        i = k.max(i);
    }
}

// a value that reads as a credential rather than a reference to one, a placeholder or code
fn value_ok(v: &str, quoted: bool) -> bool {
    if v.len() < 8 || v.len() > 200 || v.chars().any(char::is_whitespace) {
        return false;
    }
    // an interpolation or an environment lookup is the fix, not the leak
    if v.contains("${") || v.contains("{{") || v.contains("{}") || v.contains('%') || v.contains('(') || v.starts_with('$') || v.starts_with('<') {
        return false;
    }
    if v.starts_with('/') || v.starts_with("./") || v.starts_with("../") || v.starts_with("~/") || v.contains("://") {
        return false;
    }
    let lower = v.to_ascii_lowercase();
    if PLACEHOLDERS.iter().any(|p| lower.contains(p)) || entropy(v) < 3.0 {
        return false;
    }
    if !quoted {
        // unquoted, it has to look like a key rather than code: letters and digits, no call, no member path
        let digit = v.bytes().any(|c| c.is_ascii_digit());
        let alpha = v.bytes().any(|c| c.is_ascii_alphabetic());
        if !(digit && alpha) || v.contains('(') || v.contains('[') || member_path(v) {
            return false;
        }
    }
    true
}

// `config.api_key`, `settings.Secret2` - a value read from somewhere, not written here
fn member_path(v: &str) -> bool {
    v.contains('.')
        && v.split('.').all(|s| {
            s.bytes().next().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
                && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        })
}

// the value as a report shows it - its first few characters, never the rest
pub(crate) fn redacted(v: &str) -> String {
    let head: String = v.chars().take(4).collect();
    format!("{head}... ({} chars, redacted)", v.chars().count())
}

// shannon entropy per char, which is what separates a key from a word
pub(crate) fn entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    let mut total = 0usize;
    for b in s.bytes() {
        counts[b as usize] += 1;
        total += 1;
    }
    let total = total as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / total;
            -p * p.log2()
        })
        .sum()
}

// ------------------------------------------------------------ what a branch adds

// a credential a change adds, where it is
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub file: String,
    pub line: usize,
    pub what: &'static str,
    pub severity: Severity,
    // what it looks like, its value redacted
    pub evidence: String,
    // still only in the working tree - nothing is in git history yet
    pub uncommitted: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    // what the branch was read against
    pub base: String,
    pub findings: Vec<Finding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// What a branch adds over `base` that looks like a credential - committed, and
// with `worktree` what is not committed yet, untracked files included. With
// no base to read against, only what is not committed yet.
pub fn branch(root: &Path, base: Option<&str>, worktree: bool) -> Report {
    match crate::changes::resolve_base(root, base) {
        Ok((label, sha)) => against(root, &label, &sha, worktree),
        Err(_) if worktree && base.is_none() => against(root, "HEAD", "HEAD", true),
        Err(e) => Report { error: Some(format!("{e:#}")), ..Default::default() },
    }
}

// the same, against a base already resolved
pub fn against(root: &Path, label: &str, base_sha: &str, worktree: bool) -> Report {
    let mut report = Report { base: label.to_string(), ..Default::default() };
    let mut args = vec!["diff", "--no-color", "--no-ext-diff", "--relative", "-U0", "--src-prefix=a/", "--dst-prefix=b/", base_sha];
    if !worktree {
        args.push("HEAD");
    }
    let Some(diff) = git(root, &args) else {
        report.error = Some(format!("could not diff against {label}"));
        return report;
    };
    let mut uncommitted = BTreeSet::new();
    if worktree {
        uncommitted.extend(git(root, &["diff", "--name-only", "--relative", "HEAD"]).unwrap_or_default().lines().map(str::to_string));
    }
    let mut runs = added_runs(&diff);
    if worktree {
        for path in git(root, &["ls-files", "--others", "--exclude-standard", "-z"]).unwrap_or_default().split('\0').filter(|p| !p.is_empty()) {
            let file = root.join(path);
            if std::fs::metadata(&file).map_or(true, |m| m.len() > MAX_FILE_BYTES) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&file) {
                uncommitted.insert(path.to_string());
                runs.push((path.to_string(), 1, text));
            }
        }
    }
    for (file, first, text) in runs {
        let fresh = uncommitted.contains(&file);
        findings_in(&file, first, &text, fresh, &mut report.findings);
    }
    sort(&mut report.findings);
    report
}

// What a push sends that looks like a credential: the lines `to` adds over
// what `remote` holds already - its old tip, else where the branch left the
// remote's default branch, else everything.
pub fn pushed(root: &Path, remote: &str, to: &str, remote_sha: &str) -> Vec<Finding> {
    let known = |r: &str| git(root, &["cat-file", "-e", &format!("{r}^{{commit}}")]).is_some();
    let from = if remote_sha.bytes().any(|b| b != b'0') && known(remote_sha) {
        remote_sha.to_string()
    } else {
        [format!("refs/remotes/{remote}/HEAD"), format!("refs/remotes/{remote}/main"), format!("refs/remotes/{remote}/master")]
            .iter()
            .filter(|r| known(r.as_str()))
            .find_map(|r| git(root, &["merge-base", r.as_str(), to]).map(|s| s.trim().to_string()))
            .or_else(|| git_in(root, &["hash-object", "-t", "tree", "--stdin"], b"").map(|s| s.trim().to_string()))
            .unwrap_or_default()
    };
    let Some(diff) = git(root, &["diff", "--no-color", "--no-ext-diff", "-U0", "--src-prefix=a/", "--dst-prefix=b/", &from, to]) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (file, first, text) in added_runs(&diff) {
        findings_in(&file, first, &text, false, &mut out);
    }
    sort(&mut out);
    out
}

fn findings_in(file: &str, first: usize, text: &str, uncommitted: bool, out: &mut Vec<Finding>) {
    let lines: Vec<&str> = text.split('\n').collect();
    for h in scan(text) {
        let at = text[..h.start].matches('\n').count();
        let last = text[..h.end].matches('\n').count();
        // a line that says the credential is meant to be there keeps it
        if lines[at..=last.min(lines.len() - 1)].iter().any(|l| ALLOW_MARKS.iter().any(|m| l.contains(m))) {
            continue;
        }
        out.push(Finding {
            file: file.to_string(),
            line: first + at,
            what: h.what,
            severity: h.severity,
            evidence: h.evidence(text),
            uncommitted,
        });
    }
}

fn sort(findings: &mut [Finding]) {
    findings.sort_by(|a, b| (a.severity, &a.file, a.line).cmp(&(b.severity, &b.file, b.line)));
}

// The lines a `-U0` diff adds, file by file, in runs of consecutive lines -
// a key that spans lines is found in its run. Each is (file, first line, text).
fn added_runs(diff: &str) -> Vec<(String, usize, String)> {
    let mut out: Vec<(String, usize, String)> = Vec::new();
    let mut file: Option<String> = None;
    let mut header = false;
    let mut next = 0usize;
    // the run being built, and the line after its last
    let mut run: Option<(usize, String)> = None;
    let mut run_end = 0usize;
    let flush = |file: &Option<String>, run: &mut Option<(usize, String)>, out: &mut Vec<(String, usize, String)>| {
        if let (Some(f), Some((first, text))) = (file, run.take()) {
            out.push((f.clone(), first, text));
        }
    };
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            flush(&file, &mut run, &mut out);
            file = None;
            header = true;
        } else if header && line.starts_with("+++ ") {
            let p = line[4..].trim_matches('"');
            file = p.strip_prefix("b/").map(str::to_string);
        } else if line.starts_with("@@") {
            header = false;
            // `@@ -a,b +c,d @@` - the new side starts at c
            next = line
                .split(' ')
                .find_map(|p| p.strip_prefix('+'))
                .and_then(|p| p.split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
        } else if !header {
            if let Some(added) = line.strip_prefix('+') {
                match &mut run {
                    Some((_, text)) if run_end == next => {
                        text.push('\n');
                        text.push_str(added);
                    }
                    _ => {
                        flush(&file, &mut run, &mut out);
                        run = Some((next, added.to_string()));
                    }
                }
                next += 1;
                run_end = next;
            }
        }
    }
    flush(&file, &mut run, &mut out);
    out
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(root).args(["-c", "core.quotepath=off"]).args(args).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_in(root: &Path, args: &[&str], input: &[u8]) -> Option<String> {
    use std::io::Write;
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(input).ok()?;
    let out = child.wait_with_output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // tokens are put together here rather than written out, so this file
    // never reads as carrying one - to its own scan, or anyone else's
    fn gh() -> String {
        format!("ghp_{}", "aB3dE5fG7hJ9kL1mN3pQ5rS7tU9vW1xY3zA5")
    }

    fn whats(text: &str) -> Vec<&'static str> {
        scan(text).iter().map(|h| h.what).collect()
    }

    #[test]
    fn a_shaped_token_is_found_in_prose_and_its_prefix_alone_is_shown() {
        let text = format!("I set the header to {} and it worked.", gh());
        let hits = scan(&text);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].what, "a GitHub personal access token");
        assert_eq!(&text[hits[0].start..hits[0].end], gh());
        assert_eq!(hits[0].evidence(&text), "ghp_... (40 chars, redacted)");
    }

    #[test]
    fn a_name_that_only_starts_like_a_token_is_not_one() {
        // snake_case and a run too short or too plain to be random
        for text in ["hf_hub_download_the_model_weights_now_please", "ghp_short", "AKIA_ACCOUNT_LOOKUP_TABLE_NAME", "npm_package_name_from_the_manifest_file"] {
            assert!(scan(text).is_empty(), "should not fire: {text}");
        }
    }

    #[test]
    fn a_value_given_to_a_credential_name_is_found_in_any_file() {
        let pw = ["S7dK9vQx", "R2mLpZ4w"].concat();
        for text in [
            format!("DB_PASSWORD={pw}"),
            format!("\"clientSecret\": \"{pw}\""),
            format!("spring.datasource.password: {pw}"),
            format!("api_token => '{pw}'"),
        ] {
            assert_eq!(whats(&text), ["a credential given to a name"], "{text}");
        }
    }

    #[test]
    fn references_placeholders_and_code_are_left_alone() {
        for text in [
            "GITHUB_TOKEN=${{ secrets.GITHUB_TOKEN }}",
            "export API_TOKEN=$(gh auth token)",
            "password = os.environ[\"DB_PASSWORD\"]",
            "let password: String = read_password();",
            "token: &str",
            "max_tokens = 40960000",
            "if password == expected_password_value {",
            "password: changeme123",
            "secret = config.signing_secret2",
        ] {
            assert!(scan(text).is_empty(), "should not fire: {text}");
        }
    }

    #[test]
    fn a_private_key_is_found_across_its_lines_but_a_mention_is_not() {
        let body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7".repeat(3);
        let key = format!("-----BEGIN {} KEY-----\n{body}\n-----END {} KEY-----", "PRIVATE", "PRIVATE");
        let text = format!("before\n{key}\nafter");
        let hits = scan(&text);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(&text[hits[0].start..hits[0].end], key);
        assert!(scan("the parser looks for -----BEGIN RSA PRIVATE KEY----- lines").is_empty());
        // a mention followed by code on the next lines is still a mention
        let code = format!("let label = \"-----BEGIN {} KEY-----\";\nfn parse_the_body_of_it(input: &str) -> Option<Vec<u8>> {{ None }}\n{body}", "RSA PRIVATE");
        assert!(scan(&code).is_empty(), "{:?}", scan(&code));
        // a service account file carries its key on one line, broken by `\n`
        let json = format!("\"private_key\": \"-----BEGIN {} KEY-----\\n{body}\\n-----END {} KEY-----\\n\",", "PRIVATE", "PRIVATE");
        assert_eq!(whats(&json), ["a private key"]);
    }

    #[test]
    fn prose_about_a_token_is_not_one() {
        assert!(scan("The literal has to be the *next* token: `.with_unit(MILLIS)` reports no unit").is_empty());
    }

    #[test]
    fn a_password_in_a_url_is_redacted_but_the_address_stays() {
        let pw = ["Xq7", "Lm2", "Rv9", "Tz"].concat();
        let (clean, found) = redact(&format!("DATABASE_URL=postgres://app:{pw}@db.internal:5432/app"));
        assert_eq!(found, ["a password in a URL"]);
        assert_eq!(clean, "DATABASE_URL=postgres://app:[redacted: a password in a URL]@db.internal:5432/app");
        assert!(scan("postgres://app:password@localhost/app").is_empty(), "a placeholder");
    }

    #[test]
    fn a_json_web_token_is_found() {
        let jwt = ["eyJhbGciOiJIUzI1NiJ9", "eyJzdWIiOiIxMjM0NTY3ODkwIn0", "dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U"].join(".");
        assert_eq!(whats(&format!("Authorization: Bearer {jwt}")), ["a JSON Web Token"]);
    }

    #[test]
    fn a_step_is_redacted_wherever_its_text_is_and_a_diff_keeps_its_lines() {
        let body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7".repeat(2);
        let mut step = serde_json::json!({
            "ask": {"prompt": format!("use {} for the api", gh())},
            "files": [{"diff": ["@@ -1 +1,3 @@", format!("+-----BEGIN {} KEY-----", "PRIVATE"), format!("+{body}"), format!("+-----END {} KEY-----", "PRIVATE"), " fn a() {}"]}],
            "seq": 4,
        });
        let found = redact_json(&mut step);
        let wheres: Vec<&str> = found.iter().map(|(at, _)| at.as_str()).collect();
        assert_eq!(wheres, ["ask.prompt", "files[0].diff"], "{found:?}");
        assert_eq!(step["ask"]["prompt"], "use [redacted: a GitHub personal access token] for the api");
        let diff: Vec<&str> = step["files"][0]["diff"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect();
        assert_eq!(diff, ["@@ -1 +1,3 @@", "+[redacted: a private key]", "", "", " fn a() {}"]);
        assert!(!step.to_string().contains(&gh()) && !step.to_string().contains(&body));
    }

    #[test]
    fn added_lines_are_read_in_runs_with_their_numbers() {
        let diff = "diff --git a/x.env b/x.env\nindex 1..2 100644\n--- a/x.env\n+++ b/x.env\n@@ -3,0 +4,2 @@\n+A=1\n+B=2\n@@ -9 +11 @@\n-old\n+C=3\n";
        assert_eq!(
            added_runs(diff),
            [("x.env".to_string(), 4, "A=1\nB=2".to_string()), ("x.env".to_string(), 11, "C=3".to_string())]
        );
    }

    #[test]
    fn a_branch_that_adds_a_token_is_warned_and_a_marked_line_is_not() {
        let dir = std::env::temp_dir().join(format!("ccc-secrets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sh = |args: &[&str]| assert!(git(&dir, args).is_some(), "git {args:?}");
        sh(&["init", "--quiet", "-b", "main"]);
        sh(&["config", "user.name", "t"]);
        sh(&["config", "user.email", "t@example.com"]);
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        sh(&["add", "."]);
        sh(&["commit", "--quiet", "-m", "start"]);
        sh(&["checkout", "--quiet", "-b", "feature"]);
        std::fs::write(dir.join("a.txt"), format!("hello\nGH={}\n", gh())).unwrap();
        sh(&["commit", "--quiet", "-am", "oops"]);
        std::fs::write(dir.join("new.yaml"), format!("token: {} # ccc:allow-secret\nother: {}\n", gh(), gh())).unwrap();

        let r = branch(&dir, Some("main"), true);
        let got: Vec<(&str, usize, bool)> = r.findings.iter().map(|f| (f.file.as_str(), f.line, f.uncommitted)).collect();
        assert_eq!(got, [("a.txt", 2, false), ("new.yaml", 2, true)], "{r:?}");
        assert!(r.findings.iter().all(|f| !f.evidence.contains(&gh()[4..])));

        // a push from a branch the remote has never seen sends the whole branch
        let pushed = pushed(&dir, "origin", "feature", "0000000000000000000000000000000000000000");
        assert_eq!(pushed.iter().map(|f| (f.file.as_str(), f.line)).collect::<Vec<_>>(), [("a.txt", 2)]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
