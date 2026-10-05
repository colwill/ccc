// Static application security testing: syntax-level security findings over the
// same tree-sitter grammars the map is built from.
//
// This is deliberately its own pass rather than a reader of the in-memory map.
// The map keeps where code is and how it connects, not what it says: a call site
// records the callee's name but not its arguments, and a constant records its
// name but not its value. Both are exactly what a security rule needs, so this
// re-parses and looks at literals and call arguments directly.
//
// What that buys over grep: a match here is a real literal or a real call node,
// so a rule cannot fire on a comment, on prose, or on the word `password` inside
// a sentence. What it does not buy is data flow. There is no taint tracking and
// no type information behind these findings - each one cites the text it matched
// so it can be confirmed in the source, exactly as `lints` does.

use crate::languages::Language;
use crate::changes;
use crate::scan::collect_files;
use crate::secrets::{entropy, redacted, PLACEHOLDERS, SECRET_NAMES};
use serde::Serialize;
use std::path::Path;
use tree_sitter::{Node, Parser};

// a node's text is only ever used as evidence, so it never needs to be large
const MAX_TEXT: usize = 300;
const MAX_EVIDENCE: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Severity {
    High,
    Medium,
    Low,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub rule: &'static str,
    pub severity: Severity,
    pub cwe: &'static str,
    pub file: String,
    pub line: usize,
    pub function: String,
    pub language: &'static str,
    pub message: String,
    // the text the rule actually matched, redacted where it carries a secret
    pub evidence: String,
    pub hint: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct SastReport {
    pub findings: Vec<Finding>,
    pub files_scanned: usize,
    // rules that ran, so a caller can see what was looked for as well as found
    pub rules: Vec<&'static str>,
}

impl SastReport {
    pub fn by_severity(&self, s: Severity) -> usize {
        self.findings.iter().filter(|f| f.severity == s).count()
    }
}

pub const RULES: &[&str] = &[
    "hardcoded-secret",
    "secret-in-change",
    "tls-verification-disabled",
    "shell-injection",
    "sql-injection",
    "weak-hash",
    "insecure-random",
];

// one call node: what it calls, and the whole call as written
struct Call {
    line: usize,
    function: String,
    callee: String,
    // the callee as written, so a receiver can be told from a bare call
    callee_full: String,
    text: String,
    // the statement the call sits in, which is where the result gets its name
    context: String,
    in_test: bool,
}

// one string literal: its value, and the statement it sits in
struct Literal {
    line: usize,
    function: String,
    value: String,
    context: String,
    in_test: bool,
}

struct Collected {
    calls: Vec<Call>,
    literals: Vec<Literal>,
    // composite/object literals - a struct field can disable TLS without any call
    configs: Vec<Call>,
}

pub fn analyse(root: &Path, include_tests: bool) -> SastReport {
    let files = collect_files(root).unwrap_or_default();
    let mut findings = Vec::new();
    let mut scanned = 0usize;

    for path in &files {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        // a credential in a fixture is usually a fixture, not a leak
        if !include_tests && changes::is_test_path(&rel) {
            continue;
        }
        let Some(lang) = Language::from_path(path) else {
            continue;
        };
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let Some(collected) = collect(lang, &src) else {
            continue;
        };
        scanned += 1;
        findings.extend(apply_rules(&rel, lang, &collected, include_tests));
    }

    // What the branch adds that reads as a credential, in any file - a key in
    // a `.env` or a config the parse above never reads - while it can still be
    // taken out before it is committed or pushed.
    for s in crate::secrets::branch(root, None, true).findings {
        if !include_tests && changes::is_test_path(&s.file) {
            continue;
        }
        // a literal the parse already found is one finding, not two
        if findings.iter().any(|f: &Finding| f.rule == "hardcoded-secret" && f.file == s.file && f.line == s.line) {
            continue;
        }
        findings.push(Finding {
            rule: "secret-in-change",
            severity: s.severity,
            cwe: "CWE-798",
            language: Language::from_path(Path::new(&s.file)).map_or("text", |l| l.as_str()),
            message: format!("what looks like {} is added on this branch{}", s.what, if s.uncommitted { ", not committed yet" } else { "" }),
            file: s.file,
            line: s.line,
            function: String::new(),
            evidence: s.evidence,
            hint: "take it out before it is committed or pushed and read it from a secret store or the environment - once it is in git history it has to be rotated",
        });
    }

    // worst first, then by location so the order is stable
    findings.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.rule.cmp(b.rule))
    });

    SastReport {
        findings,
        files_scanned: scanned,
        rules: RULES.to_vec(),
    }
}

fn collect(lang: Language, src: &str) -> Option<Collected> {
    let mut parser = Parser::new();
    parser.set_language(&lang.ts_language()).ok()?;
    let tree = parser.parse(src, None)?;
    let mut out = Collected {
        calls: Vec::new(),
        literals: Vec::new(),
        configs: Vec::new(),
    };
    walk(tree.root_node(), src, lang, "<top>", false, &mut out);
    Some(out)
}

fn walk(node: Node, src: &str, lang: Language, function: &str, in_test: bool, out: &mut Collected) {
    let kind = node.kind();

    // entering a function renames the scope every finding below it reports
    let mut scope = function.to_string();
    let mut test_scope = in_test;
    if lang.func_kinds().contains(&kind) {
        if let Some(name) = node
            .child_by_field_name("name")
            .and_then(|n| text_of(n, src))
        {
            test_scope = test_scope || changes::is_test_fn_name(&name);
            scope = name;
        }
    }
    // an inline `mod tests` is a test scope even though the file is not a test path
    if lang.module_kinds().contains(&kind) {
        if let Some(name) = node.child_by_field_name("name").and_then(|n| text_of(n, src)) {
            let n = name.to_ascii_lowercase();
            test_scope = test_scope || n == "tests" || n == "test";
        }
    }

    if lang.call_kinds().contains(&kind) {
        let callee = node
            .child_by_field_name("function")
            .and_then(|n| text_of(n, src))
            .or_else(|| node.child(0).and_then(|n| text_of(n, src)))
            .unwrap_or_default();
        out.calls.push(Call {
            line: node.start_position().row + 1,
            function: scope.clone(),
            callee: rightmost(&callee).to_string(),
            callee_full: truncate(&callee, MAX_TEXT),
            text: truncate(&text_of(node, src).unwrap_or_default(), MAX_TEXT),
            context: truncate(
                &node.parent().and_then(|p| text_of(p, src)).unwrap_or_default(),
                MAX_TEXT,
            ),
            in_test: test_scope,
        });
    }

    // `tls.Config{InsecureSkipVerify: true}` is a struct literal, not a call
    if is_config_kind(kind) {
        out.configs.push(Call {
            line: node.start_position().row + 1,
            function: scope.clone(),
            callee: String::new(),
            callee_full: String::new(),
            text: truncate(&text_of(node, src).unwrap_or_default(), MAX_TEXT),
            context: String::new(),
            in_test: test_scope,
        });
    }

    // grammars spell literals differently, but they all say "string" somewhere
    if is_string_kind(kind) {
        if let Some(raw) = text_of(node, src) {
            let value = unquote(&raw);
            if !value.is_empty() {
                let context = node
                    .parent()
                    .and_then(|p| text_of(p, src))
                    .unwrap_or_default();
                out.literals.push(Literal {
                    line: node.start_position().row + 1,
                    function: scope.clone(),
                    value,
                    context: truncate(&context, MAX_TEXT),
                    in_test: test_scope,
                });
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, src, lang, &scope, test_scope, out);
    }
}

// kinds that carry configuration written as fields rather than arguments
fn is_config_kind(kind: &str) -> bool {
    matches!(
        kind,
        "composite_literal"
            | "keyed_element"
            | "object"
            | "pair"
            | "initializer_expression"
            | "struct_expression"
            | "field_initializer"
            | "assignment_expression"
            | "assignment"
            | "dictionary"
    )
}

pub(crate) fn is_string_kind(kind: &str) -> bool {
    // `string_content` would double-report the same literal its parent already carries
    (kind.contains("string") || kind == "raw_string_literal" || kind == "interpreted_string_literal")
        && kind != "string_content"
        && !kind.contains("interpolation")
}

fn text_of(node: Node, src: &str) -> Option<String> {
    src.get(node.byte_range()).map(|s| s.to_string())
}

fn truncate(s: &str, max: usize) -> String {
    let one_line = s.replace(['\n', '\r'], " ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let cut: String = one_line.chars().take(max).collect();
    format!("{cut}...")
}

// strip the quoting a grammar hands back, whatever flavour it is
pub(crate) fn unquote(raw: &str) -> String {
    let t = raw.trim();
    // rust r#".."#, python/go prefixes, c# verbatim
    let t = t
        .trim_start_matches(|c: char| c.is_ascii_alphabetic() || c == '@')
        .trim_start_matches('#');
    let t = t.trim_start_matches("r#").trim();
    for q in ['"', '\'', '`'] {
        if let Some(inner) = t.strip_prefix(q) {
            return inner
                .strip_suffix(&format!("{q}#"))
                .or_else(|| inner.strip_suffix(q))
                .unwrap_or(inner)
                .to_string();
        }
    }
    t.to_string()
}

// `crypto.createHash` -> `createHash`, `Md5::new` -> `new`
pub(crate) fn rightmost(s: &str) -> &str {
    let s = s.trim();
    let cut = s
        .rfind("::")
        .map(|i| i + 2)
        .or_else(|| s.rfind('.').map(|i| i + 1))
        .or_else(|| s.rfind("->").map(|i| i + 2))
        .unwrap_or(0);
    &s[cut..]
}

fn norm(s: &str) -> String {
    s.to_ascii_lowercase().replace([' ', '\t'], "")
}

fn apply_rules(file: &str, lang: Language, c: &Collected, include_tests: bool) -> Vec<Finding> {
    let mut out = Vec::new();
    for lit in &c.literals {
        if lit.in_test && !include_tests {
            continue;
        }
        secret_rule(file, lang, lit, &mut out);
    }
    for call in &c.calls {
        if call.in_test && !include_tests {
            continue;
        }
        tls_rule(file, lang, call, &mut out);
        shell_rule(file, lang, call, &mut out);
        sql_rule(file, lang, call, &mut out);
        hash_rule(file, lang, call, &mut out);
        random_rule(file, lang, call, &mut out);
    }
    for cfg in &c.configs {
        if cfg.in_test && !include_tests {
            continue;
        }
        tls_rule(file, lang, cfg, &mut out);
    }
    // a nested node repeats its parent's match; one issue is one finding
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|f| seen.insert((f.rule, f.line)));
    out
}

fn push(
    out: &mut Vec<Finding>,
    rule: &'static str,
    severity: Severity,
    cwe: &'static str,
    file: &str,
    lang: Language,
    line: usize,
    function: &str,
    message: String,
    evidence: String,
    hint: &'static str,
) {
    out.push(Finding {
        rule,
        severity,
        cwe,
        file: file.to_string(),
        line,
        function: function.to_string(),
        language: lang.as_str(),
        message,
        evidence: truncate(&evidence, MAX_EVIDENCE),
        hint,
    });
}

fn secret_rule(file: &str, lang: Language, lit: &Literal, out: &mut Vec<Finding>) {
    let v = lit.value.trim();

    // a private key pasted into source is unambiguous
    if v.contains("-----BEGIN") && v.contains("PRIVATE KEY-----") && v.len() >= 64 {
        push(
            out, "hardcoded-secret", Severity::High, "CWE-798", file, lang, lit.line,
            &lit.function,
            "a PEM private key is embedded in source".to_string(),
            "-----BEGIN ... PRIVATE KEY----- (redacted)".to_string(),
            "move it to a secret store or an environment variable, and rotate it - it is in git history",
        );
        return;
    }

    // a shaped token needs no surrounding context to be recognised
    if let Some(what) = crate::secrets::shape_of(v) {
        push(
            out, "hardcoded-secret", Severity::High, "CWE-798", file, lang, lit.line,
            &lit.function,
            format!("a literal that looks like {what}"),
            redacted(v),
            "move it to a secret store or an environment variable, and rotate it - it is in git history",
        );
        return;
    }

    // a JWT carries its own header
    if v.starts_with("eyJ") && v.matches('.').count() >= 2 && v.len() >= 40 {
        push(
            out, "hardcoded-secret", Severity::Medium, "CWE-798", file, lang, lit.line,
            &lit.function,
            "a literal that looks like a JSON Web Token".to_string(),
            redacted(v),
            "move it to a secret store or an environment variable, and rotate it - it is in git history",
        );
        return;
    }

    // otherwise it takes the name beside it to tell a secret from a string
    let ctx = norm(&lit.context);
    let assigned_to_secret = SECRET_NAMES.iter().any(|n| {
        let n = n.replace('_', "");
        ctx.replace('_', "").contains(&format!("{n}=")) || ctx.replace('_', "").contains(&format!("{n}:"))
    });
    if !assigned_to_secret {
        return;
    }
    if v.len() < 8 || v.contains(' ') {
        return;
    }
    // an interpolation or an env lookup is the fix, not the bug
    if v.contains("${") || v.contains("{}") || v.contains('%') || v.starts_with("$(") {
        return;
    }
    let lower = v.to_ascii_lowercase();
    if PLACEHOLDERS.iter().any(|p| lower.contains(p)) {
        return;
    }
    // a real credential is not a word; entropy is what separates the two
    if entropy(v) < 3.0 {
        return;
    }
    push(
        out, "hardcoded-secret", Severity::High, "CWE-798", file, lang, lit.line, &lit.function,
        "a high-entropy literal is assigned to a credential-shaped name".to_string(),
        format!("{} = {}", first_secret_name(&lit.context).unwrap_or_else(|| "<name>".into()), redacted(v)),
        "move it to a secret store or an environment variable, and rotate it - it is in git history",
    );
}

fn first_secret_name(context: &str) -> Option<String> {
    let lower = context.to_ascii_lowercase();
    let hit = SECRET_NAMES
        .iter()
        .filter_map(|n| lower.find(n).map(|i| (i, *n)))
        .min_by_key(|(i, _)| *i)?;
    Some(hit.1.to_string())
}

fn tls_rule(file: &str, lang: Language, call: &Call, out: &mut Vec<Finding>) {
    let t = norm(&call.text);
    const OFF: &[(&str, &str)] = &[
        ("verify=false", "certificate verification is disabled"),
        ("rejectunauthorized:false", "certificate verification is disabled"),
        ("insecureskipverify:true", "certificate verification is disabled"),
        ("danger_accept_invalid_certs(true)", "invalid certificates are accepted"),
        ("curlopt_ssl_verifypeer,0", "peer verification is disabled"),
        ("curlopt_ssl_verifyhost,0", "hostname verification is disabled"),
        ("servercertificatevalidationcallback", "certificate validation is overridden"),
        ("checkservertrusted", "the trust manager is overridden"),
    ];
    for (needle, what) in OFF {
        if t.contains(needle) {
            push(
                out, "tls-verification-disabled", Severity::High, "CWE-295", file, lang, call.line,
                &call.function,
                format!("{what} on this call"),
                call.text.clone(),
                "leave verification on; for a private CA add it to the trust store instead",
            );
            return;
        }
    }
}

fn shell_rule(file: &str, lang: Language, call: &Call, out: &mut Vec<Finding>) {
    let t = norm(&call.text);
    let callee = call.callee.as_str();

    // python's subprocess with a shell is the classic injection surface, but the
    // keyword only means anything on a call that actually starts a process
    const SPAWNERS: &[&str] = &[
        "run", "call", "check_call", "check_output", "popen", "spawn", "exec", "execsync",
        "spawnsync", "system",
    ];
    let spawns = SPAWNERS.contains(&callee.to_ascii_lowercase().as_str())
        || norm(&call.callee_full).contains("subprocess")
        || norm(&call.callee_full).contains("child_process");
    if t.contains("shell=true") && spawns {
        push(
            out, "shell-injection", Severity::High, "CWE-78", file, lang, call.line, &call.function,
            "a subprocess is run through a shell".to_string(),
            call.text.clone(),
            "pass the command as a list and drop shell=True, so arguments cannot be reinterpreted",
        );
        return;
    }
    // `sh -c` reintroduces a shell whatever the api
    if (t.contains("\"sh\"") || t.contains("\"bash\"") || t.contains("'sh'") || t.contains("'bash'"))
        && (t.contains("\"-c\"") || t.contains("'-c'"))
    {
        push(
            out, "shell-injection", Severity::High, "CWE-78", file, lang, call.line, &call.function,
            "a command is handed to `sh -c`".to_string(),
            call.text.clone(),
            "invoke the binary directly with an argument list instead of going through a shell",
        );
        return;
    }
    // an interpreter given something built at runtime
    const EVAL: &[&str] = &["eval", "exec", "system", "popen", "execsync", "spawnsync"];
    // `RE.exec(line)` is a regex match, not a process - a bare call or a process
    // receiver is what separates the sink from the common false positive
    let receiver = call
        .callee_full
        .rsplit_once('.')
        .map(|(r, _)| norm(r))
        .unwrap_or_default();
    const PROCESS_RECEIVERS: &[&str] = &[
        "os", "subprocess", "child_process", "cp", "shell", "runtime", "sys", "process", "sh",
    ];
    let process_context = receiver.is_empty()
        || PROCESS_RECEIVERS.iter().any(|r| receiver.ends_with(r))
        || norm(&call.context).contains("subprocess")
        || norm(&call.context).contains("child_process");
    if EVAL.contains(&callee.to_ascii_lowercase().as_str())
        && process_context
        && !only_literal_args(&call.text)
    {
        push(
            out, "shell-injection", Severity::High, "CWE-78", file, lang, call.line, &call.function,
            format!("`{callee}` is called with an argument built at runtime"),
            call.text.clone(),
            "avoid interpreting data as code; use an argument list or a parser for the value instead",
        );
    }
}

fn sql_rule(file: &str, lang: Language, call: &Call, out: &mut Vec<Finding>) {
    const SINKS: &[&str] = &["execute", "executemany", "query", "exec", "raw", "prepare"];
    if !SINKS.contains(&call.callee.to_ascii_lowercase().as_str()) {
        return;
    }
    let upper = call.text.to_ascii_uppercase();
    const VERBS: &[&str] = &["SELECT ", "INSERT ", "UPDATE ", "DELETE ", "DROP ", "UNION "];
    if !VERBS.iter().any(|v| upper.contains(v)) {
        return;
    }
    // a fully literal statement is a constant query, which is the safe case
    if only_literal_args(&call.text) {
        return;
    }
    let t = &call.text;
    let concatenated = t.contains(" + ")
        || t.contains("+\"")
        || t.contains("${")
        || t.contains("' %")
        || t.contains("\" %")
        || t.contains(".format(")
        || t.contains("f\"")
        || t.contains("f'")
        || t.contains("||");
    if !concatenated {
        return;
    }
    push(
        out, "sql-injection", Severity::High, "CWE-89", file, lang, call.line, &call.function,
        "an SQL statement is assembled from a value rather than parameterised".to_string(),
        call.text.clone(),
        "pass the value as a bound parameter (`?`, `$1`, `:name`) instead of interpolating it",
    );
}

fn hash_rule(file: &str, lang: Language, call: &Call, out: &mut Vec<Finding>) {
    let t = norm(&call.text);
    let callee = norm(&call.callee);
    let weak = ["md5", "sha1", "md4", "sha-1"];
    let named = weak.iter().any(|w| callee.contains(w));
    let argued = weak
        .iter()
        .any(|w| t.contains(&format!("\"{w}\"")) || t.contains(&format!("'{w}'")));
    if !named && !argued {
        return;
    }
    // a checksum is a legitimate use; only flag where the name suggests security
    let security_context = ["password", "token", "secret", "sign", "auth", "hmac", "cert", "key"]
        .iter()
        .any(|w| t.contains(w) || norm(&call.function).contains(w));
    let severity = if security_context {
        Severity::High
    } else {
        Severity::Low
    };
    let message = if security_context {
        "a broken hash is used in what looks like a security context".to_string()
    } else {
        "a broken hash algorithm is used; harmless as a checksum, not for security".to_string()
    };
    push(
        out, "weak-hash", severity, "CWE-327", file, lang, call.line, &call.function,
        message, call.text.clone(),
        "use SHA-256 or better; for passwords use argon2, scrypt or bcrypt rather than a raw hash",
    );
}

fn random_rule(file: &str, lang: Language, call: &Call, out: &mut Vec<Finding>) {
    let callee = call.callee.as_str();
    let full = norm(&call.text);
    let weak = matches!(callee, "random" | "rand" | "rand_r" | "srand" | "nextInt" | "randint")
        || full.starts_with("math.random")
        || full.contains("math.random(");
    if !weak {
        return;
    }
    // predictable randomness only matters when the value is meant to be unguessable
    let around = format!("{} {}", norm(&call.context), norm(&call.function));
    let sensitive = ["token", "secret", "key", "nonce", "salt", "password", "session", "otp", "iv"]
        .iter()
        .any(|w| around.contains(w) || full.contains(w));
    if !sensitive {
        return;
    }
    push(
        out, "insecure-random", Severity::Medium, "CWE-338", file, lang, call.line, &call.function,
        "a predictable random source is used for a value that must be unguessable".to_string(),
        call.text.clone(),
        "use a cryptographic generator: secrets/os.urandom, crypto.randomBytes, OsRng",
    );
}

// true when every argument is a plain literal, which makes a sink constant
fn only_literal_args(text: &str) -> bool {
    let Some(open) = text.find('(') else {
        return false;
    };
    let args = &text[open + 1..text.rfind(')').unwrap_or(text.len().saturating_sub(1)).max(open + 1)];
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return true;
    }
    // an identifier, a call or an operator means something is computed here
    let mut in_string = false;
    let mut quote = '\0';
    let mut outside = String::new();
    for ch in trimmed.chars() {
        if in_string {
            if ch == quote {
                in_string = false;
            }
            continue;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            in_string = true;
            quote = ch;
            continue;
        }
        outside.push(ch);
    }
    // only separators and whitespace should remain once literals are removed
    outside
        .chars()
        .all(|c| c.is_whitespace() || c == ',' || c == '[' || c == ']' || c == 'r')
}

pub fn rule_catalogue() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("hardcoded-secret", "CWE-798", "credentials written into source"),
        ("secret-in-change", "CWE-798", "a credential this branch adds, in any file, before it is pushed"),
        ("tls-verification-disabled", "CWE-295", "certificate checks turned off"),
        ("shell-injection", "CWE-78", "a command line built at runtime"),
        ("sql-injection", "CWE-89", "a statement assembled instead of parameterised"),
        ("weak-hash", "CWE-327", "a broken hash algorithm"),
        ("insecure-random", "CWE-338", "predictable randomness for a secret"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn run(lang: Language, src: &str) -> Vec<Finding> {
        let c = collect(lang, src).expect("the source should parse");
        apply_rules("t", lang, &c, true)
    }

    fn rules_of(f: &[Finding]) -> BTreeSet<&str> {
        f.iter().map(|x| x.rule).collect()
    }

    #[test]
    fn a_shaped_token_is_found_without_any_surrounding_context() {
        let f = run(
            Language::Python,
            "KEY = \"AKIAIOSFODNN7EXAMPLE\"\n",
        );
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule, "hardcoded-secret");
        assert_eq!(f[0].severity, Severity::High);
        // the value never appears in full in the report
        assert!(!f[0].evidence.contains("IOSFODNN7EXAMPLE"), "{}", f[0].evidence);
        assert!(f[0].evidence.contains("redacted"));
    }

    #[test]
    fn a_credential_name_plus_entropy_is_a_secret_but_a_placeholder_is_not() {
        let hit = run(
            Language::Python,
            "password = \"S7dK9vQxR2mLpZ4w\"\n",
        );
        assert_eq!(rules_of(&hit), ["hardcoded-secret"].into_iter().collect());

        // placeholders, env lookups and short values are all the fix rather than the bug
        for src in [
            "password = \"changeme\"\n",
            "password = \"${DB_PASSWORD}\"\n",
            "password = \"short\"\n",
            "greeting = \"S7dK9vQxR2mLpZ4w\"\n",
        ] {
            assert!(run(Language::Python, src).is_empty(), "should not fire: {src}");
        }
    }

    #[test]
    fn a_password_in_a_comment_or_prose_never_fires() {
        // the whole point of parsing rather than grepping
        let f = run(
            Language::Python,
            "# password = \"S7dK9vQxR2mLpZ4w\" is what we used to do\nx = 1\n",
        );
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn shell_true_and_sh_dash_c_are_both_injection_surfaces() {
        let f = run(
            Language::Python,
            "subprocess.run(cmd, shell=True)\n",
        );
        assert!(rules_of(&f).contains("shell-injection"), "{f:?}");

        let f = run(
            Language::Rust,
            "fn go(){ Command::new(\"sh\").arg(\"-c\").arg(user).spawn(); }\n",
        );
        assert!(rules_of(&f).contains("shell-injection"), "{f:?}");
    }

    #[test]
    fn a_constant_query_is_safe_and_a_built_one_is_not() {
        let safe = run(
            Language::Python,
            "cur.execute(\"SELECT 1 FROM users\")\n",
        );
        assert!(!rules_of(&safe).contains("sql-injection"), "{safe:?}");

        let unsafe_ = run(
            Language::Python,
            "cur.execute(\"SELECT * FROM users WHERE id = \" + user_id)\n",
        );
        assert!(rules_of(&unsafe_).contains("sql-injection"), "{unsafe_:?}");
    }

    #[test]
    fn tls_verification_off_is_high_whatever_the_ecosystem_calls_it() {
        for (lang, src) in [
            (Language::Python, "requests.get(url, verify=False)\n"),
            (Language::JavaScript, "https.request({rejectUnauthorized: false});\n"),
            (Language::Go, "func f(){ tls.Config{InsecureSkipVerify: true} }\n"),
        ] {
            let f = run(lang, src);
            assert!(
                f.iter().any(|x| x.rule == "tls-verification-disabled" && x.severity == Severity::High),
                "{lang:?} did not fire: {f:?}"
            );
        }
    }

    #[test]
    fn a_weak_hash_is_only_serious_in_a_security_context() {
        let checksum = run(Language::Python, "h = hashlib.md5(chunk)\n");
        let f = checksum.iter().find(|f| f.rule == "weak-hash").expect("should fire");
        // a checksum is a legitimate use, so it is reported quietly
        assert_eq!(f.severity, Severity::Low);

        let auth = run(Language::Python, "h = hashlib.md5(password)\n");
        let f = auth.iter().find(|f| f.rule == "weak-hash").expect("should fire");
        assert_eq!(f.severity, Severity::High);
    }

    #[test]
    fn predictable_randomness_only_matters_for_unguessable_values() {
        let f = run(Language::JavaScript, "const token = Math.random();\n");
        assert!(rules_of(&f).contains("insecure-random"), "{f:?}");
        // a jitter or an animation frame is not a security decision
        let f = run(Language::JavaScript, "const jitter = Math.random();\n");
        assert!(!rules_of(&f).contains("insecure-random"), "{f:?}");
    }

    #[test]
    fn findings_carry_their_enclosing_function() {
        let f = run(
            Language::Python,
            "def connect():\n    password = \"S7dK9vQxR2mLpZ4w\"\n",
        );
        assert_eq!(f[0].function, "connect");
        assert_eq!(f[0].line, 2);
    }

    #[test]
    fn a_regex_exec_is_not_a_process_call() {
        // the common false positive: RegExp.prototype.exec looks exactly like child_process.exec
        let f = run(Language::JavaScript, "const m = LISTENING.exec(line.trim());\n");
        assert!(!rules_of(&f).contains("shell-injection"), "{f:?}");

        // the real sink still fires
        let f = run(Language::JavaScript, "child_process.exec(userInput);\n");
        assert!(rules_of(&f).contains("shell-injection"), "{f:?}");
        // as does python's bare builtin, which has no receiver at all
        let f = run(Language::Python, "exec(payload)\n");
        assert!(rules_of(&f).contains("shell-injection"), "{f:?}");
    }

    #[test]
    fn shell_true_only_counts_on_a_call_that_starts_a_process() {
        // matching the text of the rule itself is not running a subprocess
        let f = run(Language::Rust, "fn r(){ t.contains(\"shell=true\"); }\n");
        assert!(!rules_of(&f).contains("shell-injection"), "{f:?}");
    }

    #[test]
    fn a_pem_label_is_not_a_pem_key() {
        // a short mention, as a rule table or a message would carry
        let f = run(Language::Rust, "fn m(){ let s = \"-----BEGIN ... PRIVATE KEY----- (redacted)\"; }\n");
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn test_scopes_are_skipped_unless_asked_for() {
        // an inline `mod tests` is a test scope even though the file is not a test path
        let src = "mod tests {\n    fn t(){ let password = \"S7dK9vQxR2mLpZ4w\"; }\n}\n";
        let c = collect(Language::Rust, src).unwrap();
        assert!(apply_rules("t", Language::Rust, &c, false).is_empty());
        assert!(!apply_rules("t", Language::Rust, &c, true).is_empty());
    }

    #[test]
    fn entropy_separates_a_key_from_a_word() {
        assert!(entropy("S7dK9vQxR2mLpZ4w") > 3.0);
        assert!(entropy("passwordpassword") < 3.0);
    }
}
