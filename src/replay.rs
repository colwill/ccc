//! Replays: the timeline of what agents did on a branch, kept in the
//! repository so a team can play it back. A branch's replay lives under a ref
//! of its own, `refs/ccc/replay/<branch>`, beside the branches rather than in
//! them - so no branch's files carry it, and a merge can never bring one to
//! the default branch. A pre-push hook saves it as the branch is pushed.

use crate::edit::FileText;
use crate::prompts::{Asks, Turn};
use crate::runccc::{Fault, Keyring, Seal, Team};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// a branch's replay, beside the branches rather than in them
pub const REF_PREFIX: &str = "refs/ccc/replay/";
// what a fetch takes as well, so a teammate's `git fetch` brings the replays
const FETCH_SPEC: &str = "+refs/ccc/replay/*:refs/ccc/replay/*";
// marks a pre-push hook as the one ccc wrote
const HOOK_MARK: &str = "# ccc: save the replay of each branch pushed";
// the format a session file declares on its first line - 2 is sealed, a file per save encrypted to the team's key
const FORMAT: u64 = 2;
// what a session file kept in the clear declares - one per session, each save appending to it
const FORMAT_CLEAR: u64 = 1;
// where a branch's narration lives - beside its replay but out of the default fetch, so only a reviewer who opens the replay fetches it
pub const VOICE_PREFIX: &str = "refs/ccc/voice/";
// asks are looked for this long before a replay's first new step
const ASK_LEAD_SECS: i64 = 4 * 3600;
// the session of steps no ask accounts for, before any that one does
const UNATTRIBUTED: &str = "unattributed";

// git in `root`, its output trimmed - none when it fails
fn git(root: &Path, args: &[&str]) -> Option<String> {
    git_raw(root, args).map(|s| s.trim().to_string())
}

// git in `root`, its output as it came
fn git_raw(root: &Path, args: &[&str]) -> Option<String> {
    git_bytes(root, args).map(|b| String::from_utf8_lossy(&b).into_owned())
}

// git in `root`, its output as bytes - a sealed file is not text
fn git_bytes(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git").arg("-C").arg(root).args(args).stderr(Stdio::null()).output().ok()?;
    out.status.success().then_some(out.stdout)
}

// A text kept for a replay, by the object id `texts_of` gave it. A reviewer
// holds only what the replay carried - with any credential taken out, so
// under its own id - filed at `texts/<id>` in the replay, or sealed in a
// replay open here.
pub(crate) fn text_of(root: &Path, id: &str) -> Option<String> {
    git_raw(root, &["cat-file", "blob", id])
        .or_else(|| {
            let refs = git(root, &["for-each-ref", "--format=%(refname)", REF_PREFIX])?;
            refs.lines().find_map(|r| git_raw(root, &["cat-file", "blob", &format!("{r}:texts/{id}")]))
        })
        .or_else(|| sealed_text(root, id))
}

// git fed `input`, with `env` set - an error carries what git said
fn git_in(root: &Path, args: &[&str], input: &[u8], env: &[(&str, &str)]) -> Result<String> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git")?;
    child.stdin.take().context("git's stdin")?.write_all(input)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// is `root` inside a git work tree
pub fn is_repo(root: &Path) -> bool {
    git(root, &["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true")
}

// the branch checked out in `root` - none when HEAD is detached
pub fn branch_of(root: &Path) -> Option<String> {
    git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).filter(|b| !b.is_empty())
}

// Whether this repository records its agent sessions as replays. A replay
// carries prompts and code, so nothing is kept until someone says yes - asked
// once, by `ccc run` on a terminal or by the editor, and kept in
// `git config ccc.replay`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Consent {
    // nobody has said - recording stays off, and ccc asks
    Unasked,
    On,
    Off,
}

pub fn consent(root: &Path) -> Consent {
    match git(root, &["config", "--bool", "ccc.replay"]).as_deref() {
        Some("true") => Consent::On,
        Some("false") => Consent::Off,
        _ => Consent::Unasked,
    }
}

// Recording turned on or off for this repository. Where its replays may go is
// not this answer's to widen: a remote anyone can read gets none unless
// `.ccc/map.json` allows it.
pub fn set_consent(root: &Path, on: bool) -> Result<Hook> {
    if !is_repo(root) {
        return Ok(Hook::NotRepo);
    }
    git_in(root, &["config", "ccc.replay", if on { "true" } else { "false" }], b"", &[])?;
    install_hook(root)
}

// what `.ccc/map.json` says to allow a public remote
pub const ALLOW_PUBLIC: &str = r#""replays": { "allow_public_remote": true }"#;

// hosting services where a repository readable without signing in is readable by anyone on the internet
const PUBLIC_HOSTS: &[&str] = &[
    "github.com", "gitlab.com", "bitbucket.org", "codeberg.org", "gitee.com", "dev.azure.com", "sourceforge.net",
];

// how far a replay pushed to a remote would travel
#[derive(Debug, Clone, Serialize)]
pub struct Exposure {
    pub remote: String,
    // its address, any credentials in it left out
    pub url: String,
    pub host: String,
    // readable without signing in - none when that could not be told
    pub public: Option<bool>,
    // a hosting service, where public means anyone on the internet
    pub public_host: bool,
    // `.ccc/map.json` allows replays to a remote anyone can read
    pub allowed: bool,
    // `.ccc/map.json` encrypts replays to a runccc team's key, so only the team opens one wherever it goes
    pub sealed: bool,
}

impl Exposure {
    // may a replay - prompts and code - go to this remote: one that needs a
    // sign-in, that map.json explicitly allows, or any remote at all once
    // replays are encrypted to the team
    pub fn shareable(&self) -> bool {
        self.sealed || self.public != Some(true) || self.allowed
    }

    // why a replay stays on this machine, when it does
    pub fn withheld(&self) -> Option<String> {
        if self.shareable() {
            return None;
        }
        let why = format!(
            "{} ({}) can be read without signing in, so the replay stays on this machine - it carries your prompts and the code each step touched",
            self.remote, self.url
        );
        Some(if self.public_host {
            format!("{why}. Make the repository private to share replays with your team - allowing it in .ccc/map.json ({ALLOW_PUBLIC}) would publish them to anyone")
        } else {
            format!(
                "{why}. If {} is your company's internal instance, where every repository is readable company-wide, allow it in .ccc/map.json: {ALLOW_PUBLIC}",
                self.host
            )
        })
    }
}

// How far a replay pushed to `remote` would travel - found by asking it,
// signed out, whether it answers. Nothing is sent but that question.
pub fn exposure(root: &Path, remote: &str) -> Option<Exposure> {
    let url = git(root, &["remote", "get-url", remote]).filter(|u| !u.is_empty())?;
    let (host, probe) = readable_url(&url);
    let lower = host.to_ascii_lowercase();
    let config = crate::changes::ChangesConfig::load(root).ok();
    Some(Exposure {
        remote: remote.to_string(),
        url: without_userinfo(&url),
        public: probe.map_or(Some(false), |u| anonymous_read(&u)),
        public_host: PUBLIC_HOSTS.iter().any(|h| lower == *h || lower.ends_with(&format!(".{h}"))),
        host,
        allowed: config.as_ref().is_some_and(|c| c.replays.allow_public_remote),
        sealed: config.as_ref().is_some_and(|c| c.replays.encrypt.is_some()),
    })
}

// the remote this checkout's branch pushes to, and how far a replay would travel there
pub fn exposure_here(root: &Path) -> Option<Exposure> {
    let remote = branch_of(root).and_then(|b| remote_for(root, &b)).or_else(|| remote_for(root, ""))?;
    exposure(root, &remote)
}

// A remote's host, and the address a signed-out read is asked at: https for
// every network form - scp-like, ssh, git - and none for a path on this
// machine, which nobody else reads.
fn readable_url(url: &str) -> (String, Option<String>) {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        // scp-like `user@host:path`, never a local path such as `../remote.git` or `C:\x`
        None => match url.split_once(':') {
            Some((h, p)) if !h.contains('/') && h.len() > 1 && !p.starts_with('\\') => ("scp".to_string(), url),
            _ => return (String::new(), None),
        },
    };
    if scheme == "file" {
        return (String::new(), None);
    }
    let (authority, path) = if scheme == "scp" {
        let (h, p) = rest.split_once(':').unwrap_or((rest, ""));
        (h, p.trim_start_matches('/'))
    } else {
        rest.split_once('/').unwrap_or((rest, ""))
    };
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = host_port.split(':').next().unwrap_or(host_port).to_string();
    // an ssh port is not an https one - the web server answers on its own
    let at = if scheme == "https" || scheme == "http" { host_port.to_string() } else { host.clone() };
    let web = if scheme == "http" { "http" } else { "https" };
    (host, Some(format!("{web}://{at}/{path}")))
}

// an address with any `user:token@` taken out, for saying aloud
fn without_userinfo(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
            let host = authority.rsplit('@').next().unwrap_or(authority);
            if path.is_empty() { format!("{scheme}://{host}") } else { format!("{scheme}://{host}/{path}") }
        }
        None => url.to_string(),
    }
}

// Does `url` answer a signed-out read? Every way git has to sign in is taken
// away for the one question - credential helpers, askpass, a terminal prompt,
// extra headers - so a private repository says no rather than borrowing the
// user's login. None when it does not answer in time, or says something else.
fn anonymous_read(url: &str) -> Option<bool> {
    let mut child = Command::new("git")
        .args(["-c", "credential.helper=", "-c", "core.askPass=", "-c", "http.extraHeader="])
        .args(["ls-remote", url, "HEAD"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Some(true),
            Ok(Some(_)) => {
                let mut said = String::new();
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.read_to_string(&mut said);
                }
                let said = said.to_ascii_lowercase();
                let refused = ["authentication", "could not read username", "terminal prompts disabled", "401", "403", "not found", "permission denied", "access denied"];
                return refused.iter().any(|s| said.contains(s)).then_some(false);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

// where recording stands for this repository - what `ccc replay status --json` answers and the editor asks from
pub fn status(root: &Path) -> Value {
    let repo = is_repo(root);
    // the runccc project replays here are encrypted to, and who on this machine is signed in to save them - null when none is
    let encrypt = match crate::runccc::team(root) {
        Ok(Some(team)) => json!({
            "project": team.project,
            "service": team.client.base,
            "login": team.client.signed_in().map(|s| s.login),
        }),
        Ok(None) => Value::Null,
        Err(why) => json!({ "error": why.to_string() }),
    };
    json!({
        "repo": repo,
        "consent": consent(root),
        "exposure": if repo { exposure_here(root) } else { None },
        "encrypt": encrypt,
    })
}

// the same, said for a person
pub fn explain(root: &Path) -> String {
    if !is_repo(root) {
        return "ccc: not a git repository - there is nothing to keep a replay beside".into();
    }
    let on = consent(root) == Consent::On;
    let mut out = match consent(root) {
        Consent::On => "ccc: session recording is on - each branch pushed saves its replay to refs/ccc/replay/<branch>, beside it".to_string(),
        Consent::Off => "ccc: session recording is off for this repository - `ccc replay enable` turns it on".to_string(),
        Consent::Unasked => "ccc: session recording has not been turned on for this repository - `ccc replay enable` turns it on".to_string(),
    };
    if let Some(e) = exposure_here(root) {
        let line = match (e.public, e.withheld()) {
            (_, Some(why)) => why,
            (Some(true), None) if e.sealed => format!("{} answers without signing in, but replays are encrypted to the team's key, so only the team opens them", e.remote),
            (Some(true), None) => format!("{} answers without signing in, and .ccc/map.json allows replays to it ({ALLOW_PUBLIC})", e.remote),
            (Some(false), _) => format!("{} needs a sign-in to read, so a replay pushed there reaches only people who can read the code", e.remote),
            (None, _) => format!("could not tell whether {} ({}) can be read without signing in", e.remote, e.url),
        };
        out.push_str(&format!("\nccc: {line}"));
    }
    if on {
        out.push_str("\nccc: anything in a replay that looks like a secret is redacted before it is saved");
    }
    match crate::runccc::team(root) {
        Ok(Some(team)) => {
            let login = if team.client.signed_in().is_none() { " - `ccc login` signs in to save them" } else { "" };
            out.push_str(&format!("\nccc: replays are encrypted to runccc project {} before they are saved, so only its team opens them{login}", team.project));
            for r in held_in_the_clear(root) {
                out.push_str(&format!(
                    "\nccc: warning - {r} holds a replay in the clear, saved by a ccc that does not encrypt or before .ccc/map.json asked for it - the next push of its branch encrypts it"
                ));
            }
        }
        Ok(None) => {}
        Err(why) => out.push_str(&format!("\nccc: replays are not saved here - {why}")),
    }
    out
}

// the replay and narration refs here that hold anything in the clear, in their history or now
pub fn held_in_the_clear(root: &Path) -> Vec<String> {
    let refs = git(root, &["for-each-ref", "--format=%(refname)", REF_PREFIX, VOICE_PREFIX]).unwrap_or_default();
    refs.lines().filter(|r| !in_the_clear(root, r).is_empty()).map(str::to_string).collect()
}

// What each side of a step's files held, as git objects, so a saved replay
// draws any side again without the working tree. `kept` holds the texts
// already stored - the parts of one step share their sides.
pub(crate) fn texts_of(root: &Path, files: &[FileText], kept: &mut HashMap<u64, String>) -> Value {
    let mut id = |text: &Option<String>| -> Option<String> {
        let text = text.as_deref()?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(text, &mut h);
        let key = std::hash::Hasher::finish(&h);
        if let Some(id) = kept.get(&key) {
            return Some(id.clone());
        }
        let id = git_in(root, &["hash-object", "-w", "--stdin"], text.as_bytes(), &[]).ok()?;
        kept.insert(key, id.clone());
        Some(id)
    };
    json!(files
        .iter()
        .map(|f| json!({"path": f.path, "before": id(&f.before), "after": id(&f.after)}))
        .collect::<Vec<_>>())
}

// every turn by the changesets its calls named
pub(crate) fn by_changeset(turns: &[Turn]) -> BTreeMap<&str, Vec<&Turn>> {
    let mut named: BTreeMap<&str, Vec<&Turn>> = BTreeMap::new();
    for t in turns {
        for c in &t.changesets {
            named.entry(c.as_str()).or_default().push(t);
        }
    }
    named
}

// The ask a step answers: the one whose calls named its changeset - the
// latest made before it, as a changeset can be staged under one ask and
// applied under the next - or, for a look, which names none, the ask made
// last before it, in its own session where it names one. A change made by
// hand answers none.
pub(crate) fn ask_of<'t>(
    human: bool,
    changeset: Option<&str>,
    session: Option<&str>,
    at_ms: u64,
    turns: &'t [Turn],
    named: &BTreeMap<&str, Vec<&'t Turn>>,
) -> Option<&'t Turn> {
    // a transcript's clock and the server's can sit a moment apart - a look
    // read from the transcript, which names its session, keeps the transcript's own
    let lead = if session.is_some() { 0 } else { 5000 };
    let made_before = |t: &&Turn| t.epoch * 1000 <= at_ms as i64 + lead;
    match (human, changeset) {
        (true, _) => None,
        (_, Some(c)) => named.get(c).and_then(|ts| ts.iter().rev().find(|t| made_before(t)).or(ts.first()).copied()),
        (_, None) => {
            let latest = |s: Option<&str>| turns.iter().filter(made_before).filter(|t| s.is_none_or(|s| t.session == s)).max_by_key(|t| t.epoch);
            session.and_then(|s| latest(Some(s))).or_else(|| latest(None))
        }
    }
}

// the remote a branch tracks, else `origin`, else the first there is
fn remote_for(root: &Path, branch: &str) -> Option<String> {
    let tracked = git(root, &["config", "--get", &format!("branch.{branch}.remote")]).filter(|r| !r.is_empty() && r != ".");
    tracked.or_else(|| {
        let all = git(root, &["remote"])?;
        let names: Vec<&str> = all.lines().collect();
        names.iter().find(|r| **r == "origin").or(names.first()).map(|r| r.to_string())
    })
}

// the branch a remote's HEAD names, else `init.defaultBranch`, else `main` or
// `master` - whichever the repository has
fn default_branch(root: &Path, remote: Option<&str>) -> Option<String> {
    if let Some(r) = remote {
        if let Some(head) = git(root, &["symbolic-ref", "--quiet", "--short", &format!("refs/remotes/{r}/HEAD")]) {
            return Some(head.strip_prefix(&format!("{r}/")).unwrap_or(&head).to_string());
        }
    }
    let has = |b: &str| {
        git(root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{b}")]).is_some()
            || remote.is_some_and(|r| git(root, &["rev-parse", "--verify", "--quiet", &format!("refs/remotes/{r}/{b}")]).is_some())
    };
    let configured = git(root, &["config", "--get", "init.defaultBranch"]).filter(|b| has(b));
    configured.or_else(|| ["main", "master"].into_iter().find(|b| has(b)).map(str::to_string))
}

// a teammate's `git fetch` from `remote` brings the replays along
fn share(root: &Path, remote: &str) {
    let key = format!("remote.{remote}.fetch");
    let specs = git(root, &["config", "--get-all", &key]).unwrap_or_default();
    if !specs.lines().any(|s| s == FETCH_SPEC) {
        let _ = git(root, &["config", "--add", &key, FETCH_SPEC]);
    }
}

// who a replay commit is by - the repository's own identity, or ccc's where it has none
fn identity(root: &Path) -> Vec<(&'static str, &'static str)> {
    if git(root, &["config", "user.email"]).is_some_and(|e| !e.is_empty()) {
        return Vec::new();
    }
    vec![
        ("GIT_AUTHOR_NAME", "ccc"),
        ("GIT_AUTHOR_EMAIL", "ccc@localhost"),
        ("GIT_COMMITTER_NAME", "ccc"),
        ("GIT_COMMITTER_EMAIL", "ccc@localhost"),
    ]
}

// a step as a replay knows it, so one saved twice is kept once - a change by
// hand, which every server on the project finds, by what it changed
fn key_of(v: &Value) -> String {
    if v["by"] == "human" {
        return format!("hand:{}", v["texts"]);
    }
    format!("{}:{}", v["server"].as_str().unwrap_or_default(), v["seq"])
}

// a session id as a file name
fn file_safe(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

// a path a sealed replay keeps - an encrypted file, or the hashes of a sealed session's step keys
fn sealed(path: &str) -> bool {
    path.ends_with(".age") || path.ends_with(".keys")
}

// every file a commit holds, by path, to the blob it is
fn tree_of(root: &Path, commit: &str) -> BTreeMap<String, String> {
    let listed = git(root, &["ls-tree", "-r", commit]).unwrap_or_default();
    listed
        .lines()
        .filter_map(|l| {
            let (meta, path) = l.split_once('\t')?;
            Some((path.to_string(), meta.split(' ').nth(2)?.to_string()))
        })
        .collect()
}

// what a ref's history ever held in the clear - nothing a repository that encrypts its replays may push
fn in_the_clear(root: &Path, commit: &str) -> BTreeSet<String> {
    // `--root` - what the first commit added counts too, whatever `log.showRoot` says
    let touched = git(root, &["log", "--root", "--format=", "--name-only", "--no-renames", commit]).unwrap_or_default();
    touched.lines().filter(|p| !p.is_empty() && !sealed(p)).map(str::to_string).collect()
}

// the texts a step names, each with the file it was read from
fn named_texts(v: &Value, texts: &mut BTreeSet<String>, paths: &mut BTreeMap<String, String>) {
    for t in v["texts"].as_array().into_iter().flatten() {
        for side in ["before", "after"] {
            if let Some(id) = t[side].as_str() {
                texts.insert(id.to_string());
                paths.entry(id.to_string()).or_insert_with(|| t["path"].as_str().unwrap_or_default().to_string());
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct SaveOptions {
    // the branch whose replay is saved - the one checked out when none
    pub branch: Option<String>,
    // where it is pushed - the branch's upstream remote, else `origin`
    pub remote: Option<String>,
    pub push: bool,
    // save it for the default branch too, which is left out unless asked for
    pub default_branch: bool,
}

// what a save did
#[derive(Debug, Default)]
pub struct Saved {
    pub branch: String,
    // the steps added, and the sessions they went to
    pub steps: usize,
    pub sessions: Vec<String>,
    // why nothing was saved, when nothing was
    pub skipped: Option<String>,
    // the remote the replay went to, or why it did not
    pub pushed: Option<Result<String, String>>,
    // the narration that went with it
    pub voice: Option<Voiced>,
    // what looked like a secret and was taken out before anything was saved, and where
    pub redacted: Vec<String>,
    // what the replay was encrypted to, when it was
    pub sealed: Option<String>,
    // keys cached earlier stood in for a key service that could not be reached
    pub note: Option<String>,
    // the replay held something in the clear - its steps encrypted now, and its history started over without it
    pub rewritten: bool,
    pub resealed: usize,
    // a skip the person pushing must hear about - a replay that could not be encrypted
    pub warn: bool,
}

// what a save did with a replay's narration
#[derive(Debug, Default)]
pub struct Voiced {
    // turned off, for the branch or everywhere
    pub off: bool,
    // the lines the narration ref holds now, and those this save added
    pub lines: usize,
    pub added: usize,
    // steps of the replay no line reads yet
    pub unvoiced: usize,
    // lines that said what looks like a secret, kept on this machine
    pub withheld: usize,
    // the remote it went to, or why it did not
    pub pushed: Option<Result<String, String>>,
}

impl Saved {
    fn skip(branch: &str, why: &str) -> Saved {
        Saved { branch: branch.to_string(), skipped: Some(why.to_string()), ..Default::default() }
    }

    // a save a repository that encrypts its replays could not encrypt - nothing written, nothing pushed, and the person pushing told why
    fn unsealed(branch: &str, why: &Fault) -> Saved {
        let why = format!(
            "it could not be encrypted: {why}. .ccc/map.json encrypts this repository's replays and none is kept in the clear, so nothing was written or pushed - the push itself goes ahead"
        );
        Saved { warn: true, ..Saved::skip(branch, &why) }
    }

    pub fn describe(&self) -> String {
        let mut out = match &self.skipped {
            Some(why) => format!("ccc: replay of {} not saved - {why}", if self.branch.is_empty() { "this checkout" } else { &self.branch }),
            None if self.steps == 0 => format!("ccc: replay of {} rewritten in {REF_PREFIX}{}", self.branch, self.branch),
            None => format!(
                "ccc: replay of {} saved to {REF_PREFIX}{} - {} step(s) in {}",
                self.branch,
                self.branch,
                self.steps,
                self.sessions.join(", ")
            ),
        };
        if let (None, Some(sealed)) = (&self.skipped, &self.sealed) {
            out.push_str(&format!(", {sealed}"));
        }
        match &self.pushed {
            Some(Ok(remote)) => out.push_str(&format!(", pushed to {remote}")),
            Some(Err(e)) => out.push_str(&format!("\nccc: the replay was not pushed: {e}")),
            None => {}
        }
        if let Some(v) = &self.voice {
            if v.off {
                out.push_str("\nccc: its narration is not shared (`git config ccc.replay.voice` or `branch.<name>.cccVoice` is false)");
            } else if v.lines > 0 {
                let new = if v.added > 0 { format!(" ({} new)", v.added) } else { String::new() };
                let sent = match &v.pushed {
                    Some(Ok(remote)) => format!(", pushed to {remote}"),
                    _ => String::new(),
                };
                out.push_str(&format!(
                    "\nccc: its narration, {} line(s){new}, is in {VOICE_PREFIX}{}{sent} - reviewers play it without the voice",
                    v.lines, self.branch
                ));
            }
            if let Some(Err(e)) = &v.pushed {
                out.push_str(&format!("\nccc: its narration was not pushed: {e}"));
            }
            if v.withheld > 0 {
                out.push_str(&format!("\nccc: {} narration line(s) said what looks like a secret and stay on this machine", v.withheld));
            }
            if !v.off && v.unvoiced > 0 && crate::voice::ready() {
                out.push_str(&format!(
                    "\nccc: {} step(s) are not voiced yet - play the replay in the visualiser to voice them, and they go with the next push",
                    v.unvoiced
                ));
            }
        }
        if !self.redacted.is_empty() {
            let shown: Vec<&str> = self.redacted.iter().take(5).map(String::as_str).collect();
            let more = self.redacted.len().saturating_sub(shown.len());
            out.push_str(&format!(
                "\nccc: {} likely secret(s) were redacted before the replay was saved: {}{}",
                self.redacted.len(),
                shown.join("; "),
                if more > 0 { format!("; and {more} more") } else { String::new() }
            ));
        }
        if self.rewritten {
            out.push_str(&format!(
                "\nccc: warning - the replay held {} step(s) in the clear, saved by a ccc that does not encrypt or before .ccc/map.json asked for it - they are encrypted now, and its history starts over without them",
                self.resealed
            ));
        }
        if let Some(note) = &self.note {
            out.push_str(&format!("\nccc: {note}"));
        }
        out
    }

    // something worth saying even when asked to be quiet - steps saved, narration sent, a push that failed, or a replay that could not be encrypted
    pub fn news(&self) -> bool {
        self.skipped.is_none()
            || self.warn
            || !self.redacted.is_empty()
            || matches!(self.pushed, Some(Err(_)))
            || self.voice.as_ref().is_some_and(|v| v.added > 0 || matches!(v.pushed, Some(Err(_))))
    }
}

// Save a branch's replay: the steps the project's feed holds for it that the
// replay does not, filed by the agent session behind them, committed to the
// branch's replay ref and pushed beside it. The default branch keeps none
// unless asked to.
pub fn save(root: &Path, opts: &SaveOptions) -> Result<Saved> {
    if !is_repo(root) {
        return Ok(Saved::skip("", "not a git repository"));
    }
    let Some(branch) = opts.branch.clone().filter(|b| !b.is_empty()).or_else(|| branch_of(root)) else {
        return Ok(Saved::skip("", "HEAD is on no branch"));
    };
    // a replay carries prompts and code, so nothing is kept until someone said yes
    if consent(root) != Consent::On {
        return Ok(Saved::skip(&branch, "session recording is off for this repository - `ccc replay enable` turns it on"));
    }
    let remote = opts.remote.clone().filter(|r| !r.is_empty()).or_else(|| remote_for(root, &branch));
    let allowed = opts.default_branch || git(root, &["config", "--bool", "ccc.replay.defaultBranch"]).as_deref() == Some("true");
    if !allowed && default_branch(root, remote.as_deref()).as_deref() == Some(branch.as_str()) {
        return Ok(Saved::skip(
            &branch,
            "it is the default branch, which keeps no replay unless asked to \
             (--default-branch, or `git config ccc.replay.defaultBranch true`)",
        ));
    }
    // a repository whose map.json encrypts its replays never keeps one in the clear - without a key there is no save, and nothing is pushed
    let sealing = match crate::runccc::team(root) {
        Ok(None) => None,
        Ok(Some(team)) => match team.seal() {
            Ok((seal, note)) => Some((team, seal, note)),
            Err(why) => return Ok(Saved::unsealed(&branch, &why)),
        },
        Err(why) => return Ok(Saved::unsealed(&branch, &why)),
    };
    let refname = format!("{REF_PREFIX}{branch}");
    // what a teammate already pushed is built on, where it runs on from this
    if let (true, Some(r)) = (opts.push, &remote) {
        let _ = git(root, &["fetch", "--quiet", "--no-tags", r, &format!("{refname}:{refname}")]);
    }
    let base = git(root, &["rev-parse", "--verify", "--quiet", &format!("{refname}^{{commit}}")]);

    // what the replay holds so far - its session files in the clear with every step in them, the hashes kept beside each sealed one, and the number each sealed session's last file has
    let tree = base.as_deref().map(|b| tree_of(root, b)).unwrap_or_default();
    let wanted: Vec<&str> = tree.iter().filter(|(p, _)| p.starts_with("sessions/") && !p.ends_with(".age")).map(|(_, id)| id.as_str()).collect();
    let held = blobs(root, &wanted);
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    let mut have: BTreeSet<String> = BTreeSet::new();
    let mut hashed: BTreeSet<String> = BTreeSet::new();
    let mut numbered: BTreeMap<String, u64> = BTreeMap::new();
    for (path, id) in &tree {
        let Some(rest) = path.strip_prefix("sessions/") else { continue };
        let text = held.get(id).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
        match rest.split_once('/') {
            Some((dir, file)) => {
                if let Some(n) = file.strip_suffix(".jsonl.age").and_then(|n| n.parse::<u64>().ok()) {
                    let last = numbered.entry(dir.to_string()).or_default();
                    *last = (*last).max(n);
                } else if file.ends_with(".keys") {
                    hashed.extend(text.lines().map(str::to_string));
                }
            }
            None => {
                let steps = text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter(|v| v.get("status").is_some());
                have.extend(steps.map(|v| key_of(&v)));
                files.insert(path.clone(), text);
            }
        }
    }
    // a replay that held anything in the clear where map.json encrypts them - saved by a ccc that does not, or before it asked - is encrypted whole and starts a history of its own, so none of it stays reachable
    let clear = match (&sealing, &base) {
        (Some(_), Some(b)) => !in_the_clear(root, b).is_empty(),
        _ => false,
    };

    // the steps the feed holds for this branch that the replay does not
    let feed = std::fs::read_to_string(crate::serve::feed_path(root)).unwrap_or_default();
    let mut steps: Vec<Value> = feed
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["branch"].as_str() == Some(branch.as_str()))
        .filter(|v| {
            let key = key_of(v);
            !hashed.contains(&sha256(key.as_bytes())) && have.insert(key)
        })
        .collect();
    let mut saved = Saved { branch: branch.clone(), rewritten: clear, ..Default::default() };
    // every step to commit, by the session it goes with, each with its key
    let mut sessions: BTreeMap<String, Vec<(String, Value)>> = BTreeMap::new();
    let mut texts = BTreeSet::new();
    let mut paths: BTreeMap<String, String> = BTreeMap::new();
    let mut redacted = Vec::new();
    let mut added: BTreeMap<String, usize> = BTreeMap::new();
    // what the replay held in the clear goes in again encrypted, as it was saved
    if clear {
        for text in files.values() {
            let mut s = UNATTRIBUTED.to_string();
            for v in text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
                if v.get("status").is_none() {
                    s = v["session"].as_str().unwrap_or(UNATTRIBUTED).to_string();
                    continue;
                }
                named_texts(&v, &mut texts, &mut paths);
                sessions.entry(s.clone()).or_default().push((key_of(&v), v));
                saved.resealed += 1;
            }
        }
    }
    if !steps.is_empty() {
        steps.sort_by_key(|v| v["at"].as_u64().unwrap_or(0));
        let first = steps[0]["at"].as_u64().unwrap_or(0) as i64 / 1000;
        let turns = Asks::default().since(root, first - ASK_LEAD_SECS);
        let named = by_changeset(&turns);
        // a step no ask accounts for goes with the session it falls in
        let mut session: Option<String> = None;
        for mut v in steps {
            let key = key_of(&v);
            let ask = ask_of(v["by"] == "human", v["changeset"].as_str(), v["session"].as_str(), v["at"].as_u64().unwrap_or(0), &turns, &named);
            if let Some(t) = ask {
                session = Some(t.session.clone());
            }
            let s = session.clone().unwrap_or_else(|| UNATTRIBUTED.to_string());
            // the story goes with the replay - the ask, the agent's plan, its beats and its last word - so a reviewer needs no transcript
            let beat = ask.and_then(|t| crate::serve::beat_at(v["at"].as_u64().unwrap_or(0), v["changeset"].as_str(), v["call"].as_str(), t));
            v["ask"] = ask.map_or(Value::Null, crate::serve::ask_json);
            v["beat"] = json!(beat);
            // nothing that reads as a credential leaves this machine in a replay - the ask, the agent's words, the diff
            for (at, what) in crate::secrets::redact_json(&mut v) {
                redacted.push(format!("{what} in step {} ({at})", v["seq"]));
            }
            named_texts(&v, &mut texts, &mut paths);
            sessions.entry(s.clone()).or_default().push((key, v));
            *added.entry(s).or_default() += 1;
        }
    }
    if sessions.is_empty() && !clear {
        saved.skipped = Some("nothing new since it was last saved".into());
    } else {
        saved.steps = added.values().sum();
        match &sealing {
            None => {
                let mut changed = BTreeSet::new();
                for (s, steps) in &sessions {
                    let name = format!("sessions/{}.jsonl", file_safe(s));
                    let file = files
                        .entry(name.clone())
                        .or_insert_with(|| format!("{}\n", json!({"ccc_replay": FORMAT_CLEAR, "branch": branch, "session": s})));
                    for (_, v) in steps {
                        file.push_str(&v.to_string());
                        file.push('\n');
                    }
                    changed.insert(name);
                }
                redacted.extend(commit(root, &refname, base.as_deref(), &files, &changed, &texts, &paths, saved.steps)?);
            }
            Some((_, seal, note)) => {
                redacted.extend(commit_sealed(root, &refname, base.as_deref(), seal, &sessions, &texts, &paths, &numbered, clear)?);
                saved.sealed = Some(format!("encrypted to runccc project {} (key v{})", seal.project, seal.version));
                saved.note = note.clone();
            }
        }
        saved.sessions = added.into_keys().collect();
        let mut said = BTreeSet::new();
        saved.redacted = redacted.into_iter().filter(|r| said.insert(r.clone())).collect();
    }
    // a replay saved but never pushed goes along now too - never where anyone can read it, unless only the team can open it
    let local = git(root, &["rev-parse", "--verify", "--quiet", &refname]);
    let mut shareable = true;
    if let (true, Some(r), Some(l)) = (opts.push, &remote, local) {
        let withheld = match &sealing {
            // encrypted, it opens only for the team wherever it goes - but nothing left in the clear goes anywhere
            Some(_) => (!in_the_clear(root, &l).is_empty())
                .then(|| format!("{refname} still holds a replay in the clear, which a repository that encrypts its replays never pushes")),
            None => exposure(root, r).and_then(|e| e.withheld()),
        };
        if let Some(why) = withheld {
            shareable = false;
            saved.pushed = Some(Err(why));
        } else {
            share(root, r);
            // the hook's own push - `--no-verify` keeps it from running the hook again, and a history started over replaces the one that carried the clear
            let spec = format!("{}{refname}:{refname}", if clear { "+" } else { "" });
            let pushed = git_in(root, &["push", "--quiet", "--no-verify", r, &spec], b"", &[]);
            saved.pushed = Some(pushed.map(|_| r.clone()).map_err(|e| e.to_string()));
        }
    }
    let sealed = sealing.as_ref().map(|(team, seal, _)| (team, seal));
    saved.voice = share_voice(root, &branch, remote.as_deref(), opts.push && shareable, sealed);
    Ok(saved)
}

// the lines the replay's steps were read with, committed beside it under a ref of their own and pushed with it - unless narration is not shared - each sealed under a name of its own where replays are encrypted, with a sealed index saying which line is which
fn share_voice(root: &Path, branch: &str, remote: Option<&str>, push: bool, sealing: Option<(&Team, &Seal)>) -> Option<Voiced> {
    let off = |key: &str| git(root, &["config", "--bool", key]).as_deref() == Some("false");
    if off("ccc.replay.voice") || off(&format!("branch.{branch}.cccVoice")) {
        return Some(Voiced { off: true, ..Default::default() });
    }
    let refname = format!("{VOICE_PREFIX}{branch}");
    let base = git(root, &["rev-parse", "--verify", "--quiet", &format!("{refname}^{{commit}}")]);
    let replay = format!("{REF_PREFIX}{branch}");
    // the replay's steps and the lines its narration holds, by name - sealed ones opened together, in one request to the key service
    let (steps, held, clear) = match sealing {
        None => {
            let commit = git(root, &["rev-parse", "--verify", "--quiet", &format!("{replay}^{{commit}}")]);
            let steps = commit.map(|c| clear_steps(root, &c, &tree_of(root, &c))).unwrap_or_default();
            let lines = base.as_deref().map(|b| tree_of(root, b)).unwrap_or_default();
            let held: BTreeSet<String> = lines.into_keys().filter_map(|p| p.strip_prefix("lines/").map(str::to_string)).collect();
            (steps, held, false)
        }
        // no voice and no line read on this machine - nothing to share, and no reason to open the replay
        Some(_) if !crate::voice::ready() && !crate::voice::noted(root) => return None,
        Some((team, _)) => match open_sealed(root, team, &[replay, refname.clone()], false) {
            Ok(opened) => {
                let steps: Vec<Value> = opened.iter().flat_map(|o| o.steps.iter().cloned()).collect();
                let held: BTreeSet<String> = opened.iter().flat_map(|o| o.lines.keys().cloned()).collect();
                (steps, held, base.as_deref().is_some_and(|b| !in_the_clear(root, b).is_empty()))
            }
            Err(why) => {
                let why = format!("the replay could not be opened to see which of its steps are voiced - {why}");
                return Some(Voiced { pushed: Some(Err(why)), ..Default::default() });
            }
        },
    };
    if steps.is_empty() {
        return None;
    }
    let read = crate::voice::lines_for(root, &steps);
    // the steps the visualiser reads - a proposal written later is read as its write
    let written: BTreeSet<&str> = steps.iter().filter(|v| v["status"] == "applied").filter_map(|v| v["changeset"].as_str()).collect();
    let heard: BTreeSet<String> = steps
        .iter()
        .filter(|v| !(v["status"] == "staged" && v["changeset"].as_str().is_some_and(|c| written.contains(c))))
        .map(crate::voice::step_key)
        .collect();
    let mut voiced = Voiced { unvoiced: heard.iter().filter(|k| !read.contains_key(*k)).count(), ..Default::default() };
    let mut info = String::new();
    // sealed - each new line's name to the file that seals it, for the index this save adds
    let mut index = serde_json::Map::new();
    for fp in read.values().collect::<BTreeSet<_>>() {
        // a line that said a credential is not shared - its words, and the voice reading them, carry it
        let said = crate::voice::line(&format!("{fp}.json"))
            .and_then(|(meta, _)| serde_json::from_slice::<Value>(&meta).ok())
            .and_then(|m| m["text"].as_str().map(str::to_string))
            .unwrap_or_default();
        if !crate::secrets::scan(&said).is_empty() {
            voiced.withheld += 1;
            continue;
        }
        for ext in ["webm", "json"] {
            let name = format!("{fp}.{ext}");
            if held.contains(&name) {
                continue;
            }
            let Some((bytes, _)) = crate::voice::line(&name) else { continue };
            let (path, bytes) = match sealing {
                None => (format!("lines/{name}"), bytes),
                // named for a hash of what sealing made, never of what the line says
                Some((_, seal)) => {
                    let Ok(sealed) = seal.seal(&bytes) else { continue };
                    let path = format!("lines/{}.age", sha256(&sealed));
                    index.insert(name, json!(path));
                    (path, sealed)
                }
            };
            let Ok(id) = git_in(root, &["hash-object", "-w", "--stdin"], &bytes, &[]) else { continue };
            info.push_str(&format!("100644 blob {id}\t{path}\n"));
            if ext == "webm" {
                voiced.added += 1;
            }
        }
    }
    voiced.lines = held.iter().filter(|n| n.ends_with(".webm")).count() + voiced.added;
    let mut message = format!("ccc narration: {branch} - {} line(s)", voiced.lines);
    if let Some((_, seal)) = sealing {
        message = format!("ccc narration: {branch} - encrypted to runccc project {} (key v{})", seal.project, seal.version);
        let tree = base.as_deref().map(|b| tree_of(root, b)).unwrap_or_default();
        if !index.is_empty() {
            let n = tree.keys().filter_map(|p| p.strip_prefix("index/")?.strip_suffix(".json.age")?.parse::<u64>().ok()).max().unwrap_or(0) + 1;
            let file = seal.seal(Value::Object(index).to_string().as_bytes()).and_then(|f| git_in(root, &["hash-object", "-w", "--stdin"], &f, &[]));
            match file {
                Ok(id) => info.push_str(&format!("100644 blob {id}\tindex/{n}.json.age\n")),
                Err(e) => {
                    voiced.pushed = Some(Err(e.to_string()));
                    return Some(voiced);
                }
            }
        }
        // a history started over carries what was sealed, and none of what was not
        if clear {
            for (path, id) in tree.iter().filter(|(p, _)| sealed(p)) {
                info.push_str(&format!("100644 blob {id}\t{path}\n"));
            }
        }
    }
    if !info.is_empty() || clear {
        if let Err(e) = write_ref(root, &refname, base.as_deref(), &info, &message, clear) {
            voiced.pushed = Some(Err(e.to_string()));
            return Some(voiced);
        }
    }
    let local = git(root, &["rev-parse", "--verify", "--quiet", &refname]);
    if let (true, Some(r), Some(l)) = (push, remote, local) {
        if sealing.is_some() && !in_the_clear(root, &l).is_empty() {
            voiced.pushed = Some(Err(format!("{refname} still holds narration in the clear, which a repository that encrypts its replays never pushes")));
            return Some(voiced);
        }
        let spec = format!("{}{refname}:{refname}", if clear { "+" } else { "" });
        let pushed = git_in(root, &["push", "--quiet", "--no-verify", r, &spec], b"", &[]);
        voiced.pushed = Some(pushed.map(|_| r.to_string()).map_err(|e| e.to_string()));
    }
    Some(voiced)
}

// the steps a replay's session files in the clear hold, as every ccc before sealing wrote them
fn clear_steps(root: &Path, commit: &str, tree: &BTreeMap<String, String>) -> Vec<Value> {
    let mut out = Vec::new();
    for name in tree.keys().filter(|p| p.starts_with("sessions/") && !sealed(p)) {
        let text = git_raw(root, &["cat-file", "blob", &format!("{commit}:{name}")]).unwrap_or_default();
        out.extend(text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter(|v| v.get("status").is_some()));
    }
    out
}

// a sealed ref opened - the steps its session files keep and the texts they name, or the lines its narration carries, with the file keys that open them held in memory only
pub(crate) struct Opened {
    root: PathBuf,
    refname: String,
    commit: String,
    keys: Arc<Keyring>,
    steps: Vec<Value>,
    // a step's text, by the id the step names it by, to the blob that seals it
    texts: HashMap<String, String>,
    // a narration line, `<fp>.webm` or `.json`, to the blob that seals it
    lines: HashMap<String, String>,
}

// the sealed replays open in this process - opening one closes those opened before it
static OPEN: Mutex<Vec<Arc<Opened>>> = Mutex::new(Vec::new());

// sealed refs opened together - every file key in them unwrapped by the key service in one request, for the texts and lines too where `whole`, else for session files and narration indexes alone
fn open_sealed(root: &Path, team: &Team, refnames: &[String], whole: bool) -> Result<Vec<Opened>, Fault> {
    let listed = |p: &str| (p.starts_with("sessions/") && p.ends_with(".jsonl.age")) || (p.starts_with("index/") && p.ends_with(".json.age"));
    let mut found = Vec::new();
    let mut ids = Vec::new();
    for r in refnames {
        let Some(commit) = git(root, &["rev-parse", "--verify", "--quiet", &format!("{r}^{{commit}}")]) else { continue };
        let tree = tree_of(root, &commit);
        ids.extend(tree.iter().filter(|(p, _)| p.ends_with(".age") && (whole || listed(p))).map(|(_, id)| id.clone()));
        found.push((r.clone(), commit, tree));
    }
    let held = blobs(root, &ids.iter().map(String::as_str).collect::<Vec<_>>());
    let keys = Arc::new(team.keyring(&held.values().map(Vec::as_slice).collect::<Vec<_>>())?);
    let mut out = Vec::new();
    for (refname, commit, tree) in found {
        let mut o = Opened { root: root.to_path_buf(), refname, commit, keys: keys.clone(), steps: Vec::new(), texts: HashMap::new(), lines: HashMap::new() };
        for (path, id) in tree.iter().filter(|(p, _)| listed(p)) {
            let Some(file) = held.get(id) else { continue };
            let text = keys.open(file).map_err(|_| {
                Fault::Unusable(format!(
                    "{}:{path} opens with no version of runccc project {}'s key - the replay is damaged, or was encrypted for another project",
                    o.refname, team.project
                ))
            })?;
            let text = String::from_utf8_lossy(&text);
            if path.starts_with("index/") {
                let index: Value = serde_json::from_str(&text).unwrap_or_default();
                for (name, p) in index.as_object().into_iter().flatten() {
                    if let Some(blob) = p.as_str().and_then(|p| tree.get(p)) {
                        o.lines.insert(name.clone(), blob.clone());
                    }
                }
                continue;
            }
            for v in text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
                if v.get("status").is_some() {
                    o.steps.push(v);
                    continue;
                }
                // the header says which sealed file holds each text its steps name
                for (id, p) in v["texts"].as_object().into_iter().flatten() {
                    if let Some(blob) = p.as_str().and_then(|p| tree.get(p)) {
                        o.texts.insert(id.clone(), blob.clone());
                    }
                }
            }
        }
        out.push(o);
    }
    Ok(out)
}

// the team a sealed replay here opens through - map.json must name it
fn sealing_team(root: &Path) -> Result<Team, Fault> {
    crate::runccc::team(root)?.ok_or_else(|| {
        Fault::Unusable("this replay is encrypted, but .ccc/map.json here names no runccc project to open it with (replays.encrypt.project)".into())
    })
}

// a branch's sealed replay as it is open here - opened through the key service where it is not, or `fresh`, which asks the service again and closes every replay opened before it
fn opened(root: &Path, branch: &str, fresh: bool) -> Result<Arc<Opened>, Fault> {
    let refname = format!("{REF_PREFIX}{branch}");
    let commit = git(root, &["rev-parse", "--verify", "--quiet", &format!("{refname}^{{commit}}")]);
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    let current = |o: &&Arc<Opened>| o.root == root && o.refname == refname && Some(&o.commit) == commit.as_ref();
    if let Some(o) = open.iter().find(current).filter(|_| !fresh) {
        return Ok(o.clone());
    }
    let team = sealing_team(root)?;
    // its narration goes in the same request, where it is here already
    let opened = open_sealed(root, &team, &[refname.clone(), format!("{VOICE_PREFIX}{branch}")], true)?;
    open.retain(|o| o.root != root);
    open.extend(opened.into_iter().map(Arc::new));
    open.iter().find(current).cloned().ok_or_else(|| Fault::Unusable(format!("there is no replay of {branch} here")))
}

// a branch's narration as it is open here - opened anew once what its ref holds moved on, as when it arrives after its replay opened
fn opened_voice(root: &Path, branch: &str) -> Option<Arc<Opened>> {
    let refname = format!("{VOICE_PREFIX}{branch}");
    let commit = git(root, &["rev-parse", "--verify", "--quiet", &format!("{refname}^{{commit}}")])?;
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(o) = open.iter().find(|o| o.root == root && o.refname == refname && o.commit == commit) {
        return Some(o.clone());
    }
    let team = sealing_team(root).ok()?;
    let opened = Arc::new(open_sealed(root, &team, std::slice::from_ref(&refname), true).ok()?.pop()?);
    open.retain(|o| !(o.root == root && o.refname == refname));
    open.push(opened.clone());
    Some(opened)
}

// every step a branch's replay holds - those in the clear as they are, sealed ones through the key service
fn read(root: &Path, branch: &str, fresh: bool) -> Result<Vec<Value>, Fault> {
    let refname = format!("{REF_PREFIX}{branch}");
    let Some(commit) = git(root, &["rev-parse", "--verify", "--quiet", &format!("{refname}^{{commit}}")]) else {
        return Ok(Vec::new());
    };
    let tree = tree_of(root, &commit);
    let mut out = clear_steps(root, &commit, &tree);
    if tree.keys().any(|p| p.ends_with(".age")) {
        out.extend(opened(root, branch, fresh)?.steps.iter().cloned());
    } else if fresh {
        OPEN.lock().unwrap_or_else(|e| e.into_inner()).retain(|o| o.root != root);
    }
    Ok(out)
}

// a text a sealed replay open here carries, opened with the keys it was opened with
fn sealed_text(root: &Path, id: &str) -> Option<String> {
    let open = OPEN.lock().unwrap_or_else(|e| e.into_inner()).clone();
    open.iter().filter(|o| o.root == root).find_map(|o| {
        let file = git_bytes(root, &["cat-file", "blob", o.texts.get(id)?])?;
        String::from_utf8(o.keys.open(&file).ok()?).ok()
    })
}

// a line of the narration of a replay open here, sealed
fn sealed_line(root: &Path, name: &str) -> Option<Vec<u8>> {
    let branches: Vec<String> = {
        let open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
        open.iter().filter(|o| o.root == root).filter_map(|o| o.refname.strip_prefix(REF_PREFIX).map(str::to_string)).collect()
    };
    branches.iter().find_map(|b| {
        let o = opened_voice(root, b)?;
        let file = git_bytes(root, &["cat-file", "blob", o.lines.get(name)?])?;
        o.keys.open(&file).ok()
    })
}

// a branch name a request may name a ref by - nothing that could step outside `refs/ccc/` or read as an option
pub fn branch_ok(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '/', '.'])
        && !name.contains("..")
        && !name.ends_with(['/', '.'])
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
}

// the steps of a branch's replay, oldest first and numbered - what a reviewer's visualiser plays, a sealed one from the keys it was opened with while the ref still holds what they open
pub fn steps(root: &Path, branch: &str) -> Result<Vec<Value>, Fault> {
    Ok(numbered(read(root, branch, false)?))
}

// the same, opened as a reviewer opens a replay - a sealed one asks the key service afresh, which checks the team and the payment again, and closes every replay opened before it
pub fn open(root: &Path, branch: &str) -> Result<Vec<Value>, Fault> {
    Ok(numbered(read(root, branch, true)?))
}

// a replay's steps, oldest first, numbered from one, with no server's name
fn numbered(mut steps: Vec<Value>) -> Vec<Value> {
    steps.sort_by_key(|v| v["at"].as_u64().unwrap_or(0));
    for (i, v) in steps.iter_mut().enumerate() {
        v["seq"] = json!(i + 1);
        if let Some(o) = v.as_object_mut() {
            o.remove("server");
        }
    }
    steps
}

// the replays this repository holds, newest first
pub fn list(root: &Path) -> Vec<Value> {
    git(root, &["for-each-ref", "--sort=-committerdate", "--format=%(refname)%09%(committerdate:unix)%09%(contents:subject)", REF_PREFIX])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(3, '\t');
            let branch = parts.next()?.strip_prefix(REF_PREFIX)?.to_string();
            let at = parts.next()?.parse::<i64>().ok()?;
            Some(json!({"branch": branch, "at": at * 1000, "subject": parts.next().unwrap_or("")}))
        })
        .collect()
}

// the remote a reviewer's replays come from - the checked-out branch's, else `origin`
fn remote_here(root: &Path) -> Option<String> {
    branch_of(root).and_then(|b| remote_for(root, &b)).or_else(|| remote_for(root, ""))
}

// teammates' replays fetched - every branch's, its narration left until it is opened
pub fn fetch_replays(root: &Path) -> Result<String> {
    let remote = remote_here(root).context("this repository has no remote to fetch replays from")?;
    git_in(root, &["fetch", "--quiet", "--no-tags", &remote, FETCH_SPEC], b"", &[])?;
    share(root, &remote);
    Ok(remote)
}

// a branch's narration fetched as its replay is opened - it stays out of every other fetch
pub fn fetch_voice(root: &Path, branch: &str) {
    if let Some(remote) = remote_here(root) {
        let spec = format!("+{VOICE_PREFIX}{branch}:{VOICE_PREFIX}{branch}");
        let _ = git_in(root, &["fetch", "--quiet", "--no-tags", &remote, &spec], b"", &[]);
    }
}

// a kept line found in the narration a replay carries - for a reviewer whose machine never read it - in the clear, or sealed beside a replay open here
pub fn voice_line(root: &Path, name: &str) -> Option<Vec<u8>> {
    let refs = git(root, &["for-each-ref", "--format=%(refname)", VOICE_PREFIX]).unwrap_or_default();
    refs.lines().find_map(|r| git_bytes(root, &["cat-file", "blob", &format!("{r}:lines/{name}")])).or_else(|| sealed_line(root, name))
}

// The replay's next commit: the session files that gained steps, and every
// text those steps name, over what the ref held - built in an index of its
// own, so the working tree and the user's index are never touched.
fn commit(
    root: &Path,
    refname: &str,
    base: Option<&str>,
    files: &BTreeMap<String, String>,
    changed: &BTreeSet<String>,
    texts: &BTreeSet<String>,
    // the file each text was read from, to say where a credential was taken out
    paths: &BTreeMap<String, String>,
    steps: usize,
) -> Result<Vec<String>> {
    let mut info = String::new();
    for name in changed {
        let id = git_in(root, &["hash-object", "-w", "--stdin"], files[name].as_bytes(), &[])?;
        info.push_str(&format!("100644 blob {id}\t{name}\n"));
    }
    // a text git let go of before it was saved is left out, not an error
    let ids: String = texts.iter().map(|t| format!("{t}\n")).collect();
    let found = git_in(root, &["cat-file", "--batch-check"], ids.as_bytes(), &[])?;
    let present: Vec<&str> = found
        .lines()
        .filter(|l| l.ends_with(|c: char| c.is_ascii_digit()) && l.contains(" blob "))
        .map(|l| l.split(' ').next().unwrap_or_default())
        .collect();
    // A file's text goes in with anything that reads as a credential taken out,
    // under the name its step knows it by. The text as it was stays in this
    // repository's objects, which no push of the replay reaches.
    let held = blobs(root, &present);
    let mut redacted = BTreeSet::new();
    for id in present {
        let mut blob = id.to_string();
        if let Some(text) = held.get(id).and_then(|b| std::str::from_utf8(b).ok()) {
            let (clean, found) = crate::secrets::redact(text);
            if !found.is_empty() {
                blob = git_in(root, &["hash-object", "-w", "--stdin"], clean.as_bytes(), &[])?;
                let file = paths.get(id).map(String::as_str).filter(|p| !p.is_empty()).unwrap_or("a file");
                redacted.extend(found.into_iter().map(|w| format!("{w} in {file}")));
            }
        }
        info.push_str(&format!("100644 blob {blob}\ttexts/{id}\n"));
    }
    let message = format!("ccc replay: {} - {steps} step(s)", refname.trim_start_matches(REF_PREFIX));
    write_ref(root, refname, base, &info, &message, false)?;
    Ok(redacted.into_iter().collect())
}

// A sealed replay's next commit: a file per session that gained steps, and
// every text they name - each encrypted to the team's key once anything that
// reads as a credential is out - with the hashes of its steps' keys beside
// each session file, so the next save knows what the replay holds without
// opening it. Nothing in it reads in the clear: a text is named for a hash
// of what sealing made, and only the sealed session says which is which.
// `fresh` starts a history of its own, carrying only what `base` held sealed.
#[allow(clippy::too_many_arguments)]
fn commit_sealed(
    root: &Path,
    refname: &str,
    base: Option<&str>,
    seal: &Seal,
    sessions: &BTreeMap<String, Vec<(String, Value)>>,
    texts: &BTreeSet<String>,
    // the file each text was read from, to say where a credential was taken out
    paths: &BTreeMap<String, String>,
    // the number each sealed session's last file has
    numbered: &BTreeMap<String, u64>,
    fresh: bool,
) -> Result<Vec<String>> {
    let branch = refname.trim_start_matches(REF_PREFIX);
    let mut info = String::new();
    if let (true, Some(b)) = (fresh, base) {
        for (path, id) in tree_of(root, b).iter().filter(|(p, _)| sealed(p)) {
            info.push_str(&format!("100644 blob {id}\t{path}\n"));
        }
    }
    // a text git let go of before it was saved is left out, not an error - one a replay kept in the clear is taken from there
    let ids: Vec<&str> = texts.iter().map(String::as_str).collect();
    let mut held = blobs(root, &ids);
    let missing: Vec<&str> = ids.iter().copied().filter(|id| !held.contains_key(*id)).collect();
    for (id, b) in missing.into_iter().filter_map(|id| base.map(|b| (id, b))) {
        if let Some(bytes) = git_bytes(root, &["cat-file", "blob", &format!("{b}:texts/{id}")]) {
            held.insert(id.to_string(), bytes);
        }
    }
    let mut redacted = BTreeSet::new();
    let mut sealed_as: BTreeMap<&str, String> = BTreeMap::new();
    for (id, bytes) in &held {
        let mut text = bytes.clone();
        if let Ok(t) = std::str::from_utf8(bytes) {
            let (clean, found) = crate::secrets::redact(t);
            if !found.is_empty() {
                let file = paths.get(id).map(String::as_str).filter(|p| !p.is_empty()).unwrap_or("a file");
                redacted.extend(found.into_iter().map(|w| format!("{w} in {file}")));
                text = clean.into_bytes();
            }
        }
        let file = seal.seal(&text)?;
        let path = format!("texts/{}.age", sha256(&file));
        let blob = git_in(root, &["hash-object", "-w", "--stdin"], &file, &[])?;
        info.push_str(&format!("100644 blob {blob}\t{path}\n"));
        sealed_as.insert(id, path);
    }
    let mut next = numbered.clone();
    for (s, steps) in sessions {
        let dir = file_safe(s);
        let n = next.entry(dir.clone()).or_default();
        *n += 1;
        let mut named = BTreeMap::new();
        for (_, v) in steps {
            named_texts(v, &mut BTreeSet::new(), &mut named);
        }
        let named: BTreeMap<&str, &String> = named.keys().filter_map(|id| sealed_as.get(id.as_str()).map(|p| (id.as_str(), p))).collect();
        let mut jsonl = format!("{}\n", json!({"ccc_replay": FORMAT, "branch": branch, "session": s, "texts": named}));
        let mut keys = String::new();
        for (key, v) in steps {
            jsonl.push_str(&format!("{v}\n"));
            keys.push_str(&format!("{}\n", sha256(key.as_bytes())));
        }
        let blob = git_in(root, &["hash-object", "-w", "--stdin"], &seal.seal(jsonl.as_bytes())?, &[])?;
        info.push_str(&format!("100644 blob {blob}\tsessions/{dir}/{n}.jsonl.age\n"));
        let blob = git_in(root, &["hash-object", "-w", "--stdin"], keys.as_bytes(), &[])?;
        info.push_str(&format!("100644 blob {blob}\tsessions/{dir}/{n}.keys\n"));
    }
    let message = format!("ccc replay: {branch} - encrypted to runccc project {} (key v{})", seal.project, seal.version);
    write_ref(root, refname, base, &info, &message, fresh)?;
    Ok(redacted.into_iter().collect())
}

// the blobs `ids` name, as git holds them - one read for all of them
fn blobs(root: &Path, ids: &[&str]) -> HashMap<String, Vec<u8>> {
    let mut out = HashMap::new();
    let Ok(mut child) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return out;
    };
    // written on a thread of its own: git answers as it reads, and two full pipes would stall
    let input: String = ids.iter().map(|i| format!("{i}\n")).collect();
    let stdin = child.stdin.take();
    let writer = std::thread::spawn(move || {
        if let Some(mut s) = stdin {
            let _ = s.write_all(input.as_bytes());
        }
    });
    let mut raw = Vec::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_end(&mut raw);
    }
    let _ = writer.join();
    let _ = child.wait();
    // `<id> <type> <size>\n<bytes>\n`, one after another - a missing object is its line alone
    let mut at = 0;
    while let Some(nl) = raw[at..].iter().position(|&b| b == b'\n') {
        let head = String::from_utf8_lossy(&raw[at..at + nl]).into_owned();
        at += nl + 1;
        let parts: Vec<&str> = head.split(' ').collect();
        let (Some(id), Some(kind), Some(size)) = (parts.first(), parts.get(1), parts.get(2).and_then(|s| s.parse::<usize>().ok())) else {
            continue;
        };
        if at + size > raw.len() {
            break;
        }
        if *kind == "blob" {
            out.insert(id.to_string(), raw[at..at + size].to_vec());
        }
        at += size + 1;
    }
    out
}

// a ref's next commit - `info` entries over what it held, built in an index of its own so the working tree and the user's index are never touched - or, `fresh`, `info` alone in a history of its own, so nothing `base` reached stays reachable from the ref
fn write_ref(root: &Path, refname: &str, base: Option<&str>, info: &str, message: &str, fresh: bool) -> Result<()> {
    let index = git(root, &["rev-parse", "--git-path", "ccc-replay.index"]).context("finding the git directory")?;
    let index = root.join(index);
    let _ = std::fs::remove_file(&index);
    let index_env = index.to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", index_env.as_str())];
    let parent = base.filter(|_| !fresh);
    let built = (|| -> Result<String> {
        match parent {
            Some(b) => git_in(root, &["read-tree", b], b"", &env)?,
            None => git_in(root, &["read-tree", "--empty"], b"", &env)?,
        };
        git_in(root, &["update-index", "--add", "--index-info"], info.as_bytes(), &env)?;
        let tree = git_in(root, &["write-tree"], b"", &env)?;
        let mut args = vec!["commit-tree", tree.as_str(), "-m", message];
        if let Some(b) = parent {
            args.extend(["-p", b]);
        }
        git_in(root, &args, b"", &identity(root))
    })();
    let _ = std::fs::remove_file(&index);
    let commit = built?;
    let mut args = vec!["update-ref", "-m", message, refname, commit.as_str()];
    if let Some(b) = base {
        args.push(b);
    }
    git_in(root, &args, b"", &[])?;
    Ok(())
}

// where ccc's pre-push hook stands
#[derive(Debug)]
pub enum Hook {
    // written now
    Installed(PathBuf),
    // already there, brought up to date
    Present(PathBuf),
    // a hook ccc did not write is there, and is left alone
    Foreign(PathBuf),
    // turned off with `git config ccc.replay false`
    Off,
    NotRepo,
}

impl Hook {
    pub fn describe(&self) -> String {
        match self {
            Hook::Installed(p) => format!(
                "ccc: each push is now checked for anything that looks like a secret, and saves its branch's replay where recording is on (hook at {})",
                p.display()
            ),
            Hook::Present(p) => format!(
                "ccc: each push is checked for anything that looks like a secret, and saves its branch's replay where recording is on (hook at {})",
                p.display()
            ),
            Hook::Foreign(p) => format!(
                "ccc: {} is not ccc's, so it was left alone - add `ccc hook pre-push \"$1\" \"$2\"` to it to check pushes for secrets and keep replays",
                p.display()
            ),
            Hook::Off => "ccc: the pre-push hook is off here (`git config ccc.replay false` and `ccc.secrets false`)".into(),
            Hook::NotRepo => "ccc: not a git repository - there is no push to save a replay with".into(),
        }
    }
}

// The pre-push hook: it warns of anything that looks like a secret in what
// each branch sends, saves the branch's replay beside it where recording is
// on, and never stops the push. It calls the ccc that wrote it, else the one
// on the PATH, which reads the refs git sends on stdin.
fn hook_script() -> String {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().replace('\'', r"'\''")).unwrap_or_default();
    format!(
        r#"#!/bin/sh
{HOOK_MARK}, to refs/ccc/replay/<branch> beside it, where
# recording is on (`ccc replay enable`), and warns of anything that looks like a
# secret in what is pushed (`git config ccc.secrets false` turns that off).
# Written by ccc, and it never stops a push.
ccc='{exe}'
[ -x "$ccc" ] || ccc="$(command -v ccc)" || exit 0
"$ccc" hook pre-push "$1" "$2" || true
exit 0
"#
    )
}

// What ccc's pre-push hook does with one push: warn of anything that looks
// like a secret in what each branch sends, and save the branch's replay where
// recording is on. It never stops the push; what it says goes to the person
// pushing.
pub fn pre_push(root: &Path, remote: &str, updates: &str) -> String {
    let off = |k: &str| git(root, &["config", "--bool", k]).as_deref() == Some("false");
    let mut out = String::new();
    for line in updates.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let [local_ref, local_sha, _, remote_sha] = parts[..] else { continue };
        let Some(branch) = local_ref.strip_prefix("refs/heads/") else { continue };
        // a branch being deleted sends nothing
        if !local_sha.bytes().any(|b| b != b'0') {
            continue;
        }
        if !off("ccc.secrets") {
            out.push_str(&warn_secrets(branch, remote, &crate::secrets::pushed(root, remote, local_sha, remote_sha)));
        }
        if consent(root) == Consent::On {
            let opts = SaveOptions { branch: Some(branch.to_string()), remote: Some(remote.to_string()), push: true, default_branch: false };
            match save(root, &opts) {
                Ok(saved) if saved.news() => out.push_str(&format!("{}\n", saved.describe())),
                Ok(_) => {}
                Err(e) => out.push_str(&format!("ccc: the replay of {branch} was not saved: {e:#}\n")),
            }
        }
    }
    out
}

// what a push sends that looks like a credential, said before it lands
fn warn_secrets(branch: &str, remote: &str, found: &[crate::secrets::Finding]) -> String {
    if found.is_empty() {
        return String::new();
    }
    let mut out = format!("ccc: warning - pushing {branch} to {remote} sends {} line(s) that look like a secret:\n", found.len());
    for f in found.iter().take(10) {
        out.push_str(&format!("ccc:   {}:{} {} {}\n", f.file, f.line, f.what, f.evidence));
    }
    if found.len() > 10 {
        out.push_str(&format!("ccc:   and {} more\n", found.len() - 10));
    }
    out.push_str(
        "ccc: once pushed it stays in the remote's history - if one is real, take it out of these commits and rotate it.\n\
         ccc: a line marked `ccc:allow-secret` is meant to be there and is not reported.\n",
    );
    out
}

// Install the pre-push hook - unless a hook ccc did not write is there
// already, or both of its jobs are turned off.
pub fn install_hook(root: &Path) -> Result<Hook> {
    if !is_repo(root) {
        return Ok(Hook::NotRepo);
    }
    let off = |k: &str| git(root, &["config", "--bool", k]).as_deref() == Some("false");
    if off("ccc.replay") && off("ccc.secrets") {
        return Ok(Hook::Off);
    }
    let hooks = root.join(git(root, &["rev-parse", "--git-path", "hooks"]).context("finding git's hooks")?);
    let file = hooks.join("pre-push");
    let state = match std::fs::read_to_string(&file) {
        Ok(text) if !text.contains(HOOK_MARK) => return Ok(Hook::Foreign(file)),
        // rewritten when ccc moves, so it calls the ccc that runs now
        Ok(text) if text == hook_script() => return Ok(Hook::Present(file)),
        Ok(_) => Hook::Present(file.clone()),
        Err(_) => Hook::Installed(file.clone()),
    };
    std::fs::create_dir_all(&hooks)?;
    std::fs::write(&file, hook_script())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))?;
    }
    if consent(root) == Consent::On {
        if let Some(r) = branch_of(root).and_then(|b| remote_for(root, &b)) {
            share(root, &r);
        }
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // A look read from a transcript keeps the transcript's clock and serves
    // the ask its own session made last before it - not one made a moment
    // after, nor another session's.
    #[test]
    fn a_look_from_a_transcript_serves_its_own_sessions_ask() {
        let turn = |id: &str, session: &str, epoch| Turn {
            id: id.into(),
            agent: "claude".into(),
            session: session.into(),
            model: None,
            ts: String::new(),
            branch: None,
            prompt: String::new(),
            edits: Vec::new(),
            changesets: Vec::new(),
            epoch,
            beats: Vec::new(),
        };
        let turns = [turn("a", "s1", 100), turn("b", "s1", 102), turn("c", "s2", 150)];
        let named = by_changeset(&turns);
        let ask = |session, at_ms| ask_of(false, None, session, at_ms, &turns, &named).map(|t| t.id.as_str());
        // the server's own look allows for the two clocks
        assert_eq!(ask(None, 101_000), Some("b"));
        assert_eq!(ask(Some("s1"), 101_000), Some("a"));
        assert_eq!(ask(Some("s1"), 160_000), Some("b"));
        assert_eq!(ask(None, 160_000), Some("c"));
    }

    // a repository with a branch off `main`, and a bare remote both were pushed to
    struct Repo {
        dir: PathBuf,
        work: PathBuf,
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn sh(dir: &Path, args: &[&str]) -> String {
        git(dir, args).unwrap_or_else(|| panic!("git {args:?} failed in {}", dir.display()))
    }

    fn repo(tag: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!("ccc-replay-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let work = dir.join("work");
        fs::create_dir_all(&work).unwrap();
        sh(&dir, &["init", "--quiet", "--bare", "remote.git"]);
        sh(&work, &["init", "--quiet", "-b", "main"]);
        sh(&work, &["config", "user.name", "t"]);
        sh(&work, &["config", "user.email", "t@example.com"]);
        sh(&work, &["config", "ccc.replay", "true"]);
        fs::write(work.join("a.rs"), "fn a() {}\n").unwrap();
        sh(&work, &["add", "."]);
        sh(&work, &["commit", "--quiet", "-m", "start"]);
        sh(&work, &["remote", "add", "origin", dir.join("remote.git").to_str().unwrap()]);
        sh(&work, &["push", "--quiet", "-u", "origin", "main"]);
        sh(&work, &["remote", "set-head", "origin", "main"]);
        sh(&work, &["checkout", "--quiet", "-b", "feature"]);
        Repo { dir, work }
    }

    // steps onto the project's feed, as servers write them
    fn feed(work: &Path, steps: &[Value]) {
        let path = crate::serve::feed_path(work);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = fs::read_to_string(&path).unwrap_or_default();
        for s in steps {
            text.push_str(&format!("{s}\n"));
        }
        fs::write(path, text).unwrap();
    }

    fn step(seq: u64, branch: &str, status: &str, texts: Value) -> Value {
        json!({"server": "s1", "seq": seq, "at": seq * 1000, "branch": branch, "status": status,
               "by": "agent", "tool": "edit_text", "changeset": "c1-x", "files": [], "texts": texts})
    }

    fn lines(work: &Path, spec: &str) -> usize {
        sh(work, &["cat-file", "blob", spec]).lines().count()
    }

    // A pushed branch carries its replay beside it - under a ref of its own,
    // on the remote too, and in no branch's files - and a replay saved again
    // keeps each step once.
    #[test]
    fn a_pushed_branch_carries_its_replay_beside_it_never_in_it() {
        let r = repo("save");
        let text = texts_of(&r.work, &[FileText { path: "a.rs".into(), before: Some("fn a() {}\n".into()), after: Some("fn a() { b(); }\n".into()) }], &mut HashMap::new());
        let id = text[0]["after"].as_str().unwrap().to_string();
        feed(&r.work, &[step(1, "feature", "staged", text.clone()), step(2, "feature", "applied", text), step(3, "main", "applied", json!([]))]);
        let push = SaveOptions { push: true, ..Default::default() };

        let saved = save(&r.work, &push).unwrap();
        assert_eq!((saved.steps, saved.skipped.as_deref()), (2, None), "{saved:?}");
        assert!(matches!(&saved.pushed, Some(Ok(remote)) if remote == "origin"), "{saved:?}");
        let theirs = sh(&r.dir, &["--git-dir", "remote.git", "for-each-ref", "--format=%(refname)"]);
        assert!(theirs.lines().any(|l| l == "refs/ccc/replay/feature"), "{theirs}");
        let files = sh(&r.work, &["ls-tree", "-r", "--name-only", "refs/ccc/replay/feature"]);
        assert!(files.contains("sessions/unattributed.jsonl") && files.contains(&format!("texts/{id}")), "{files}");
        assert!(!sh(&r.work, &["ls-tree", "-r", "--name-only", "feature"]).contains("sessions"), "never in the branch");
        assert_eq!(lines(&r.work, "refs/ccc/replay/feature:sessions/unattributed.jsonl"), 3, "a header and two steps");
        // a teammate's fetch brings replays along
        assert!(sh(&r.work, &["config", "--get-all", "remote.origin.fetch"]).contains(FETCH_SPEC));

        assert_eq!(save(&r.work, &push).unwrap().steps, 0, "nothing new");
        feed(&r.work, &[step(4, "feature", "inspect", json!([]))]);
        assert_eq!(save(&r.work, &push).unwrap().steps, 1);
        assert_eq!(lines(&r.work, "refs/ccc/replay/feature:sessions/unattributed.jsonl"), 4);
        assert_eq!(sh(&r.work, &["rev-list", "--count", "refs/ccc/replay/feature"]), "2");

        // the default branch keeps none unless asked to
        sh(&r.work, &["checkout", "--quiet", "main"]);
        let main = save(&r.work, &SaveOptions::default()).unwrap();
        assert!(main.skipped.as_deref().is_some_and(|s| s.contains("default branch")), "{main:?}");
        let asked = save(&r.work, &SaveOptions { default_branch: true, ..Default::default() }).unwrap();
        assert_eq!(asked.steps, 1);
    }

    // a replay's narration goes beside it under a ref of its own, out of the default fetch; a reviewer plays its steps and lines from it; turned off, it stays home
    #[test]
    fn a_replays_narration_goes_beside_it_and_a_reviewer_plays_it() {
        crate::voice::tests::in_cache("replay", |_| {
            let r = repo("voice");
            let text = texts_of(&r.work, &[FileText { path: "a.rs".into(), before: Some("fn a() {}\n".into()), after: Some("fn a() { b(); }\n".into()) }], &mut HashMap::new());
            feed(&r.work, &[step(1, "feature", "staged", text.clone()), step(2, "feature", "applied", text), step(3, "feature", "applied", json!([]))]);
            // the visualiser read the written step aloud - its line kept, and noted against the step
            let fp = "b".repeat(64);
            let meta = json!({"duration": 1.0, "words": [], "step": {"at": 2000, "changeset": "c1-x", "call": null}}).to_string();
            crate::voice::keep_line(&format!("{fp}.webm"), b"opus").unwrap();
            crate::voice::keep_line(&format!("{fp}.json"), meta.as_bytes()).unwrap();
            crate::voice::note_line(&r.work, &format!("{fp}.json"), meta.as_bytes()).unwrap();

            let push = SaveOptions { push: true, ..Default::default() };
            let saved = save(&r.work, &push).unwrap();
            let v = saved.voice.as_ref().unwrap();
            // the proposal is heard as its write, so two steps are heard and one of them is voiced
            assert_eq!((v.added, v.lines, v.unvoiced), (1, 1, 1), "{v:?}");
            assert!(matches!(&v.pushed, Some(Ok(remote)) if remote == "origin"), "{v:?}");
            assert!(saved.describe().contains("refs/ccc/voice/feature"), "{}", saved.describe());
            let theirs = sh(&r.dir, &["--git-dir", "remote.git", "for-each-ref", "--format=%(refname)"]);
            assert!(theirs.lines().any(|l| l == "refs/ccc/voice/feature"), "{theirs}");
            assert!(!sh(&r.work, &["config", "--get-all", "remote.origin.fetch"]).contains("voice"), "out of the default fetch");
            assert_eq!(save(&r.work, &push).unwrap().voice.map(|v| v.added), Some(0), "kept once");

            // a reviewer: the replay's steps numbered, and the line read from the narration the replay carries
            crate::voice::remove().unwrap();
            assert!(crate::voice::line(&format!("{fp}.webm")).is_none());
            assert_eq!(voice_line(&r.work, &format!("{fp}.webm")).as_deref(), Some(&b"opus"[..]));
            let played = steps(&r.work, "feature").unwrap();
            assert_eq!(played.iter().filter_map(|s| s["seq"].as_u64()).collect::<Vec<_>>(), [1, 2, 3]);
            assert!(played.iter().all(|s| s.get("server").is_none()));
            assert_eq!(list(&r.work)[0]["branch"], "feature");

            // turned off for the branch, its narration stays home
            sh(&r.work, &["config", "branch.feature.cccVoice", "false"]);
            assert!(save(&r.work, &SaveOptions::default()).unwrap().voice.is_some_and(|v| v.off));
        });
        assert!(branch_ok("feature/x-1") && !branch_ok("../x") && !branch_ok("-x") && !branch_ok("a b") && !branch_ok("x:y"));
    }

    // a repository nobody said yes for keeps nothing, and a fetch there brings no replays
    #[test]
    fn nothing_is_recorded_until_someone_says_yes() {
        let r = repo("consent");
        sh(&r.work, &["config", "--unset", "ccc.replay"]);
        feed(&r.work, &[step(1, "feature", "applied", json!([]))]);
        assert_eq!(consent(&r.work), Consent::Unasked);
        let saved = save(&r.work, &SaveOptions { push: true, ..Default::default() }).unwrap();
        assert!(saved.skipped.as_deref().is_some_and(|s| s.contains("recording is off")), "{saved:?}");
        assert!(!saved.news(), "the hook stays quiet about it");
        assert!(git(&r.work, &["rev-parse", "--verify", "--quiet", "refs/ccc/replay/feature"]).is_none());
        install_hook(&r.work).unwrap();
        assert!(!sh(&r.work, &["config", "--get-all", "remote.origin.fetch"]).contains(FETCH_SPEC));

        set_consent(&r.work, true).unwrap();
        assert_eq!(save(&r.work, &SaveOptions::default()).unwrap().steps, 1);
        set_consent(&r.work, false).unwrap();
        assert_eq!(consent(&r.work), Consent::Off);
    }

    // what reads as a credential is taken out of a step and the texts it carries before the replay is committed
    #[test]
    fn a_replay_carries_no_secret_its_session_saw() {
        let r = repo("redact");
        let token = format!("ghp_{}", "aB3dE5fG7hJ9kL1mN3pQ5rS7tU9vW1xY3zA5");
        let text = texts_of(&r.work, &[FileText { path: "a.env".into(), before: None, after: Some(format!("GH={token}\n")) }], &mut HashMap::new());
        let id = text[0]["after"].as_str().unwrap().to_string();
        let mut s = step(1, "feature", "applied", text);
        s["intent"] = json!([format!("set the token to {token}")]);
        feed(&r.work, &[s]);

        let saved = save(&r.work, &SaveOptions::default()).unwrap();
        assert_eq!(saved.steps, 1, "{saved:?}");
        assert!(saved.redacted.iter().any(|w| w.contains("intent")) && saved.redacted.iter().any(|w| w.contains("a.env")), "{:?}", saved.redacted);
        assert!(saved.news() && saved.describe().contains("redacted"));
        let session = sh(&r.work, &["cat-file", "blob", "refs/ccc/replay/feature:sessions/unattributed.jsonl"]);
        let kept = sh(&r.work, &["cat-file", "blob", &format!("refs/ccc/replay/feature:texts/{id}")]);
        assert!(!session.contains(&token) && !kept.contains(&token), "{session}\n{kept}");
        assert!(kept.contains("[redacted: a GitHub personal access token]"), "{kept}");
        // the text is still found under the name its step knows it by
        assert!(text_of(&r.work, &id).is_some());
    }

    // every network address is asked at https, and a path on this machine is never asked at all
    #[test]
    fn a_remote_is_asked_about_at_its_web_address() {
        let probe = |u: &str| readable_url(u);
        assert_eq!(probe("git@github.com:acme/app.git"), ("github.com".into(), Some("https://github.com/acme/app.git".into())));
        assert_eq!(probe("ssh://git@git.corp.io:2222/acme/app.git"), ("git.corp.io".into(), Some("https://git.corp.io/acme/app.git".into())));
        assert_eq!(probe("https://bot:tok@gitlab.corp.io:8443/a/b"), ("gitlab.corp.io".into(), Some("https://gitlab.corp.io:8443/a/b".into())));
        assert_eq!(probe("/srv/git/app.git").1, None);
        assert_eq!(probe("../remote.git").1, None);
        assert_eq!(probe("file:///srv/git/app.git").1, None);
        assert_eq!(without_userinfo("https://bot:tok@host/a/b"), "https://host/a/b");

        let e = |public, public_host, allowed| Exposure { remote: "origin".into(), url: "u".into(), host: "git.corp.io".into(), public, public_host, allowed, sealed: false };
        assert!(e(Some(false), true, false).shareable() && e(None, false, false).shareable());
        let sealed = Exposure { sealed: true, ..e(Some(true), true, false) };
        assert!(sealed.shareable() && sealed.withheld().is_none(), "an encrypted replay may go to a public remote");
        assert!(!e(Some(true), true, false).shareable(), "public on a hosting service");
        assert!(!e(Some(true), false, false).shareable(), "public on a company instance nobody allowed");
        assert!(e(Some(true), false, true).shareable(), "allowed in map.json");
        assert!(e(Some(true), false, false).withheld().is_some_and(|w| w.contains(".ccc/map.json") && w.contains("allow_public_remote")));
    }

    // the permission is the repository's, in map.json where the team sees it - never a setting on one machine
    #[test]
    fn only_map_json_allows_a_public_remote() {
        let r = repo("allow");
        assert!(!exposure(&r.work, "origin").unwrap().allowed);
        fs::create_dir_all(r.work.join(".ccc")).unwrap();
        fs::write(r.work.join(".ccc/map.json"), format!("{{ {ALLOW_PUBLIC} }}")).unwrap();
        assert!(exposure(&r.work, "origin").unwrap().allowed);
        fs::write(r.work.join(".ccc/map.json"), r#"{ "replays": { "allow_public_remote": false } }"#).unwrap();
        assert!(!exposure(&r.work, "origin").unwrap().allowed);
    }

    // a push that sends a token is warned of, whether or not it records a replay
    #[test]
    fn a_push_that_sends_a_secret_is_warned_of() {
        let r = repo("push-secret");
        sh(&r.work, &["config", "ccc.replay", "false"]);
        fs::write(r.work.join("b.env"), format!("API_TOKEN=ghp_{}\n", "aB3dE5fG7hJ9kL1mN3pQ5rS7tU9vW1xY3zA5")).unwrap();
        sh(&r.work, &["add", "."]);
        sh(&r.work, &["commit", "--quiet", "-m", "oops"]);
        let head = sh(&r.work, &["rev-parse", "HEAD"]);
        let zeros = "0".repeat(40);
        let said = pre_push(&r.work, "origin", &format!("refs/heads/feature {head} refs/heads/feature {zeros}\n"));
        assert!(said.contains("b.env:1") && said.contains("GitHub personal access token"), "{said}");
        assert!(!said.contains("replay"), "recording is off: {said}");
        sh(&r.work, &["config", "ccc.secrets", "false"]);
        assert!(pre_push(&r.work, "origin", &format!("refs/heads/feature {head} refs/heads/feature {zeros}\n")).is_empty());
    }

    // the hook goes in once, follows ccc as it moves, never over a hook ccc
    // did not write, and not at all once both its jobs are turned off
    #[test]
    fn the_hook_is_ccc_s_own_or_left_alone() {
        let r = repo("hook");
        let Hook::Installed(file) = install_hook(&r.work).unwrap() else { panic!("not installed") };
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.contains(HOOK_MARK) && text.contains("hook pre-push"), "{text}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(fs::metadata(&file).unwrap().permissions().mode() & 0o111, 0, "executable");
        }
        assert!(matches!(install_hook(&r.work).unwrap(), Hook::Present(_)));
        fs::write(&file, "#!/bin/sh\necho mine\n").unwrap();
        assert!(matches!(install_hook(&r.work).unwrap(), Hook::Foreign(_)));
        assert_eq!(fs::read_to_string(&file).unwrap(), "#!/bin/sh\necho mine\n");
        fs::remove_file(&file).unwrap();
        sh(&r.work, &["config", "ccc.replay", "false"]);
        assert!(matches!(install_hook(&r.work).unwrap(), Hook::Installed(_)), "secrets are still checked");
        sh(&r.work, &["config", "ccc.secrets", "false"]);
        assert!(matches!(install_hook(&r.work).unwrap(), Hook::Off));
    }

    use crate::runccc::tests::{client, in_config, service, Project, PROJECT};

    // map.json encrypting replays to the test project at a stand-in key service
    fn sealed_to(work: &Path, base: &str) {
        fs::create_dir_all(work.join(".ccc")).unwrap();
        fs::write(work.join(".ccc/map.json"), json!({"replays": {"encrypt": {"project": PROJECT, "service": base}}}).to_string()).unwrap();
    }

    // every object the refs under refs/ccc carry, shown as git shows it - trees by their names, commits with their messages
    fn carried(dir: &Path) -> String {
        let refs = sh(dir, &["for-each-ref", "--format=%(refname)", "refs/ccc/"]);
        let mut out = String::new();
        for r in refs.lines() {
            for object in sh(dir, &["rev-list", "--objects", r]).lines() {
                let id = object.split(' ').next().unwrap();
                out.push_str(&git_raw(dir, &["cat-file", "-p", id]).unwrap_or_default());
            }
        }
        out
    }

    // a teammate's clone of the remote, set up for the same project, with every replay fetched
    fn teammate(r: &Repo, base: &str) -> PathBuf {
        let mate = r.dir.join("mate");
        sh(&r.dir, &["clone", "--quiet", "remote.git", "mate"]);
        sealed_to(&mate, base);
        fetch_replays(&mate).unwrap();
        mate
    }

    // An encrypted replay carries nothing readable - not the prompt, not a
    // line of the code, not the blob id a text had, not a commit message that
    // says either - and a teammate opens it whole through the key service, in
    // one request, across the key versions it was sealed under.
    #[test]
    fn an_encrypted_replay_carries_nothing_readable_and_the_team_opens_it() {
        in_config("sealed", |config| {
            let project = Project::new();
            let s = service(project.clone());
            client(&s.base, config, true);
            let r = repo("sealed");
            sealed_to(&r.work, &s.base);
            let line = "let launch_code = 4242;";
            let text = texts_of(&r.work, &[FileText { path: "a.rs".into(), before: Some("fn a() {}\n".into()), after: Some(format!("fn a() {{ {line} }}\n")) }], &mut HashMap::new());
            let id = text[0]["after"].as_str().unwrap().to_string();
            let mut first = step(1, "feature", "applied", text);
            first["intent"] = json!(["wire the launch sequence to the red button"]);
            feed(&r.work, &[first]);

            let push = SaveOptions { push: true, ..Default::default() };
            let saved = save(&r.work, &push).unwrap();
            assert_eq!((saved.steps, saved.skipped.as_deref()), (1, None), "{saved:?}");
            assert!(matches!(&saved.pushed, Some(Ok(remote)) if remote == "origin"), "{saved:?}");
            assert!(saved.describe().contains("encrypted to runccc project prj_test (key v1)"), "{}", saved.describe());
            let files = sh(&r.work, &["ls-tree", "-r", "--name-only", "refs/ccc/replay/feature"]);
            assert!(files.lines().all(sealed), "{files}");
            assert!(files.contains("sessions/unattributed/1.jsonl.age") && files.contains("sessions/unattributed/1.keys"), "{files}");
            let remote = r.dir.join("remote.git");
            let theirs = carried(&remote);
            for plain in ["launch sequence", "launch_code", id.as_str()] {
                assert!(!theirs.contains(plain), "{plain} is readable under refs/ccc");
            }
            assert_eq!(save(&r.work, &push).unwrap().steps, 0, "kept once, known without opening it");
            assert_eq!(s.count("/unwrap"), 0, "a save never opens what it saved");

            // a new key version after someone left - new saves go to it, and both open
            project.lock().unwrap().keys.push(age::x25519::Identity::generate());
            feed(&r.work, &[step(2, "feature", "applied", json!([]))]);
            let saved = save(&r.work, &push).unwrap();
            assert!(saved.describe().contains("(key v2)"), "{}", saved.describe());
            assert!(sh(&r.work, &["ls-tree", "-r", "--name-only", "refs/ccc/replay/feature"]).contains("sessions/unattributed/2.jsonl.age"));

            let mate = teammate(&r, &s.base);
            let played = open(&mate, "feature").unwrap();
            assert_eq!(s.count("/unwrap"), 1, "the whole replay opens in one request");
            assert_eq!(played.len(), 2);
            assert_eq!(played[0]["intent"][0], "wire the launch sequence to the red button");
            assert!(git(&mate, &["cat-file", "-e", &id]).is_none(), "the teammate never had the text in the clear");
            assert!(text_of(&mate, &id).is_some_and(|t| t.contains(line)), "the text opens from the replay");
            assert_eq!(steps(&mate, "feature").unwrap().len(), 2);
            assert_eq!(s.count("/unwrap"), 1, "an open replay keeps its keys while it is open");
        });
    }

    // A repository that encrypts its replays never saves one it cannot
    // encrypt - signed out, off the team, unpaid, the service out of reach with
    // no keys from the last day - and says why, to the hook too; a reviewer
    // who cannot open one is told the service's reason, not a decryption error.
    #[test]
    fn a_replay_that_cannot_be_encrypted_is_not_saved_and_says_why() {
        in_config("unsealed", |config| {
            let project = Project::new();
            let s = service(project.clone());
            let r = repo("unsealed");
            sealed_to(&r.work, &s.base);
            feed(&r.work, &[step(1, "feature", "applied", json!([]))]);
            let push = SaveOptions { push: true, ..Default::default() };
            let nothing = |r: &Repo| {
                git(&r.work, &["rev-parse", "--verify", "--quiet", "refs/ccc/replay/feature"]).is_none()
                    && sh(&r.dir, &["--git-dir", "remote.git", "for-each-ref", "refs/ccc/"]).is_empty()
            };

            assert_eq!(status(&r.work)["encrypt"], json!({ "project": PROJECT, "service": s.base, "login": null }));
            let saved = save(&r.work, &push).unwrap();
            let why = saved.skipped.clone().unwrap_or_default();
            assert!(why.contains("could not be encrypted") && why.contains("ccc login"), "{why}");
            assert!(saved.warn && saved.news() && nothing(&r), "{saved:?}");
            let head = sh(&r.work, &["rev-parse", "HEAD"]);
            let said = pre_push(&r.work, "origin", &format!("refs/heads/feature {head} refs/heads/feature {}\n", "0".repeat(40)));
            assert!(said.contains("could not be encrypted"), "the push hears it: {said}");

            client(&s.base, config, true);
            assert_eq!(status(&r.work)["encrypt"]["login"], "dev");
            project.lock().unwrap().down = true;
            let why = save(&r.work, &push).unwrap().skipped.unwrap_or_default();
            assert!(why.contains("could not help just now") && why.contains("last day"), "{why}");
            project.lock().unwrap().down = false;
            project.lock().unwrap().member = false;
            assert!(save(&r.work, &push).unwrap().skipped.unwrap_or_default().contains("isn't on the team"));
            project.lock().unwrap().member = true;
            project.lock().unwrap().paid = false;
            assert!(save(&r.work, &push).unwrap().skipped.unwrap_or_default().contains("not paid for"));
            assert!(nothing(&r), "nothing written or pushed");

            // keys the service gave within the day stand in while it is down - the unpaid answer kept last would not
            project.lock().unwrap().paid = true;
            client(&s.base, config, false).keys(PROJECT).unwrap();
            project.lock().unwrap().down = true;
            let saved = save(&r.work, &push).unwrap();
            assert_eq!(saved.steps, 1, "{saved:?}");
            assert!(saved.describe().contains("could not be reached, so the replay was sealed to key v1"), "{}", saved.describe());
            project.lock().unwrap().down = false;

            project.lock().unwrap().member = false;
            let off = open(&r.work, "feature").unwrap_err();
            assert_eq!(off.status(), 403);
            assert!(off.to_string().contains("isn't on the team"), "{off}");
            project.lock().unwrap().member = true;
            project.lock().unwrap().paid = false;
            let unpaid = open(&r.work, "feature").unwrap_err();
            assert_eq!(unpaid.status(), 402);
            assert!(unpaid.to_string().contains("isn't paid for"), "{unpaid}");
        });
    }

    // A replay saved in the clear - before map.json asked for encryption, or
    // by a teammate's older ccc - is warned of, and the next save encrypts it
    // whole and starts its history over, so none of it stays reachable.
    #[test]
    fn a_replay_left_in_the_clear_is_encrypted_whole() {
        in_config("reseal", |config| {
            let project = Project::new();
            let s = service(project.clone());
            client(&s.base, config, true);
            let r = repo("reseal");
            let line = "let launch_code = 4242;";
            let text = texts_of(&r.work, &[FileText { path: "a.rs".into(), before: None, after: Some(format!("{line}\n")) }], &mut HashMap::new());
            let id = text[0]["after"].as_str().unwrap().to_string();
            feed(&r.work, &[step(1, "feature", "staged", text.clone()), step(2, "feature", "applied", text)]);
            let push = SaveOptions { push: true, ..Default::default() };
            assert_eq!(save(&r.work, &push).unwrap().steps, 2, "format 1, in the clear");
            let remote = r.dir.join("remote.git");
            assert!(carried(&remote).contains("launch_code"));
            assert_eq!(steps(&r.work, "feature").unwrap().len(), 2, "format 1 still opens");

            sealed_to(&r.work, &s.base);
            assert!(explain(&r.work).contains("refs/ccc/replay/feature holds a replay in the clear"), "{}", explain(&r.work));
            feed(&r.work, &[step(3, "feature", "applied", json!([]))]);
            let saved = save(&r.work, &push).unwrap();
            assert_eq!((saved.steps, saved.resealed, saved.rewritten), (1, 2, true), "{saved:?}");
            assert!(saved.describe().contains("history starts over"), "{}", saved.describe());
            assert!(matches!(&saved.pushed, Some(Ok(_))), "{saved:?}");
            assert!(in_the_clear(&remote, "refs/ccc/replay/feature").is_empty(), "no history in the clear on the remote");
            assert!(!carried(&remote).contains("launch_code"));
            assert!(held_in_the_clear(&r.work).is_empty());

            let mate = teammate(&r, &s.base);
            assert_eq!(open(&mate, "feature").unwrap().len(), 3, "every step, sealed now");
            assert!(text_of(&mate, &id).is_some_and(|t| t.contains(line)));
        });
    }

    // a replay's narration is encrypted beside it - each line under a name of its own, an encrypted index saying which is which - and a reviewer plays it
    #[test]
    fn encrypted_narration_goes_beside_it_and_a_reviewer_plays_it() {
        crate::voice::tests::in_cache("sealed-voice", |_| {
            in_config("sealed-voice", |config| {
                let project = Project::new();
                let s = service(project.clone());
                client(&s.base, config, true);
                let r = repo("sealed-voice");
                sealed_to(&r.work, &s.base);
                feed(&r.work, &[step(1, "feature", "applied", json!([])), step(2, "feature", "applied", json!([]))]);
                let fp = "c".repeat(64);
                let meta = json!({"duration": 1.0, "words": [], "text": "the agent wires the launch sequence", "step": {"at": 2000, "changeset": "c1-x", "call": null}}).to_string();
                crate::voice::keep_line(&format!("{fp}.webm"), b"opus").unwrap();
                crate::voice::keep_line(&format!("{fp}.json"), meta.as_bytes()).unwrap();
                crate::voice::note_line(&r.work, &format!("{fp}.json"), meta.as_bytes()).unwrap();

                let push = SaveOptions { push: true, ..Default::default() };
                let saved = save(&r.work, &push).unwrap();
                let v = saved.voice.as_ref().unwrap();
                assert_eq!((v.added, v.lines), (1, 1), "{v:?}");
                assert!(matches!(&v.pushed, Some(Ok(_))), "{v:?}");
                let lines = sh(&r.work, &["ls-tree", "-r", "--name-only", "refs/ccc/voice/feature"]);
                assert!(lines.lines().all(sealed) && lines.contains("index/1.json.age"), "{lines}");
                let theirs = carried(&r.dir.join("remote.git"));
                assert!(!theirs.contains(&fp) && !theirs.contains("launch sequence"), "no line named or said in the clear");
                assert_eq!(save(&r.work, &push).unwrap().voice.map(|v| v.added), Some(0), "kept once");

                let mate = teammate(&r, &s.base);
                fetch_voice(&mate, "feature");
                crate::voice::remove().unwrap();
                let before = s.count("/unwrap");
                open(&mate, "feature").unwrap();
                assert_eq!(s.count("/unwrap"), before + 1, "the replay and its narration open in one request");
                assert_eq!(voice_line(&mate, &format!("{fp}.webm")).as_deref(), Some(&b"opus"[..]));
            });
        });
    }
}
