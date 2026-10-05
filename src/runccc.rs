//! runccc - the key service a team's replays are encrypted through, and ccc's open-source client of it (API.md in the service is the contract)

use age::secrecy::{zeroize::Zeroize, ExposeSecret, SecretBox};
use age_core::format::{FileKey, Stanza, FILE_KEY_BYTES};
use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// where a project's key is held when map.json names no service
pub const SERVICE: &str = "https://teams.runccc.dev";
// how long keys the service gave may stand in for it while it cannot be reached
const CACHE_FOR: Duration = Duration::from_secs(24 * 3600);
// the most stanzas the service unwraps in one request
const BATCH: usize = 2000;
// the stanza a project key wraps a file key in - the only kind the service opens
const X25519: &str = "X25519";

// the service to ask - `RUNCCC_URL`, else the one map.json names, else runccc's own
pub fn service_url(named: Option<&str>) -> String {
    let env = std::env::var("RUNCCC_URL").ok();
    let url = [env.as_deref(), named].into_iter().flatten().map(str::trim).find(|u| !u.is_empty()).unwrap_or(SERVICE);
    url.trim_end_matches('/').to_string()
}

// a project id as the service hands them out - nothing that could step out of the path it goes in
pub fn project_ok(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

// the user's own config, where the token and cached keys live - `~/.config/ccc/runccc` on Linux, moved by `CCC_CONFIG_DIR`
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CCC_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir).join("runccc"));
    }
    let home = || std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).filter(|h| !h.is_empty()).map(PathBuf::from);
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").filter(|d| !d.is_empty()).map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        home().map(|h| h.join("Library").join("Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()).map(PathBuf::from).or_else(|| home().map(|h| h.join(".config")))
    };
    base.map(|d| d.join("ccc").join("runccc"))
}

// a file only its owner reads - mode 0600 in a folder only its owner enters, written whole and then put in place
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp)?;
    // a temp file left by an earlier run keeps the mode it had - this one never reads wider
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    drop(file);
    std::fs::rename(&tmp, path)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// why the key service could not do what was asked, said for a person
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fault {
    // nobody is signed in to the service on this machine
    SignedOut(String),
    // it answered no - its status and error code, and its message for a person
    Refused { status: u16, code: String, message: String },
    // it could not be reached, or asked to be tried again later
    Unreachable(String),
    // what it gave, or what the repository says, cannot be used
    Unusable(String),
}

impl Fault {
    // the status a page asking for a sealed replay is answered with
    pub fn status(&self) -> u16 {
        match self {
            Fault::SignedOut(_) => 401,
            Fault::Refused { status, .. } => *status,
            Fault::Unreachable(_) => 502,
            Fault::Unusable(_) => 422,
        }
    }
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fault::SignedOut(m) | Fault::Unreachable(m) | Fault::Unusable(m) => f.write_str(m),
            Fault::Refused { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for Fault {}

// who is signed in, and the token that says so
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedIn {
    pub token: String,
    pub login: String,
}

// a project's status and every version of its public key, as the service gives them
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectKeys {
    pub project: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub team: String,
    // `active`, or `unpaid` - nothing is saved for an unpaid project
    pub status: String,
    // the version new files are sealed to
    pub current: u32,
    // newest first
    pub keys: Vec<KeyVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyVersion {
    pub version: u32,
    // an age X25519 recipient, `age1…`
    pub recipient: String,
    #[serde(default)]
    pub created_at: String,
}

// what a sign-in starts with - the code the person approves in their browser
#[derive(Debug, Deserialize)]
struct Device {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}

// the code a sign-in shows the person, and the page they approve it on
#[derive(Debug, Clone, Serialize)]
pub struct Code {
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    // seconds before it expires
    pub expires_in: u64,
    // ccc opened the page in a browser itself
    pub opened: bool,
}

// what the service answered - its status, and its json
struct Answer {
    status: u16,
    body: Value,
}

// the key service at one address, as whoever signed in to it on this machine
#[derive(Debug, Clone)]
pub struct Client {
    pub base: String,
    // where its token and the keys it gave are kept - none without a home
    dir: Option<PathBuf>,
}

impl Client {
    pub fn new(base: &str) -> Client {
        Client { base: base.trim_end_matches('/').to_string(), dir: config_dir() }
    }

    // the service named for a person - runccc's own goes without saying
    fn at(&self) -> String {
        if self.base == SERVICE {
            String::new()
        } else {
            format!(" at {}", self.base)
        }
    }

    fn signed_out(&self) -> Fault {
        Fault::SignedOut(format!("not signed in to runccc{} - `ccc login` signs in", self.at()))
    }

    // every service's sign-in on this machine, by its address - a token is only ever sent where it came from
    fn credentials(&self) -> BTreeMap<String, SignedIn> {
        let file = self.dir.as_ref().map(|d| d.join("credentials.json"));
        file.and_then(|f| std::fs::read_to_string(f).ok()).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn signed_in(&self) -> Option<SignedIn> {
        self.credentials().remove(&self.base)
    }

    // this service's sign-in kept, or forgotten
    fn keep(&self, signed: Option<&SignedIn>) -> Result<()> {
        let dir = self.dir.as_ref().context("there is no home directory to keep the sign-in in")?;
        let mut all = self.credentials();
        match signed {
            Some(s) => all.insert(self.base.clone(), s.clone()),
            None => all.remove(&self.base),
        };
        let file = dir.join("credentials.json");
        write_private(&file, &serde_json::to_vec_pretty(&all)?).with_context(|| format!("writing {}", file.display()))
    }

    // where the keys this service gave are cached - each service's apart, so one never stands in for another's
    fn keys_dir(&self) -> Option<PathBuf> {
        let tag = hex(&Sha256::digest(self.base.as_bytes())[..8]);
        self.dir.as_ref().map(|d| d.join("keys").join(tag))
    }

    // one request, made with curl as ccc's other downloads are - its settings fed on stdin, so the token never shows in a process list
    fn request(&self, method: &str, path: &str, token: Option<&str>, body: Option<&Value>) -> Result<Answer, Fault> {
        let quoted = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        let mut config = vec![
            "silent".to_string(),
            "show-error".to_string(),
            "proto = \"=http,https\"".to_string(),
            "connect-timeout = 10".to_string(),
            "max-time = 60".to_string(),
            format!("request = {}", quoted(method)),
            format!("url = {}", quoted(&format!("{}{path}", self.base))),
            format!("header = {}", quoted(&format!("User-Agent: ccc/{} (runccc)", env!("CARGO_PKG_VERSION")))),
            "header = \"Accept: application/json\"".to_string(),
            r#"write-out = "\n%{http_code}""#.to_string(),
        ];
        if let Some(t) = token {
            config.push(format!("header = {}", quoted(&format!("Authorization: Bearer {t}"))));
        }
        if let Some(b) = body {
            config.push("header = \"Content-Type: application/json\"".to_string());
            config.push(format!("data-binary = {}", quoted(&b.to_string())));
        }
        let unreachable = |why: &str| Fault::Unreachable(format!("the key service at {} could not be reached - {why}", self.base));
        let mut child = Command::new("curl")
            .args(["--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| unreachable(&format!("running curl, which ccc talks to it with: {e}")))?;
        // curl reads all of its settings before it answers, so they go in whole first
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(config.join("\n").as_bytes());
        }
        let out = child.wait_with_output().map_err(|e| unreachable(&e.to_string()))?;
        let text = String::from_utf8_lossy(&out.stdout);
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
        let status: u16 = code.trim().parse().unwrap_or(0);
        if status == 0 {
            let said = String::from_utf8_lossy(&out.stderr);
            return Err(unreachable(said.trim().trim_start_matches("curl: ")));
        }
        Ok(Answer { status, body: serde_json::from_str(body).unwrap_or(Value::Null) })
    }

    // what an answer that is not a yes means - a refusal to show, or a service to try again later
    fn fault(&self, a: &Answer) -> Fault {
        let code = a.body["error"].as_str().unwrap_or_default().to_string();
        let message = a.body["message"]
            .as_str()
            .filter(|m| !m.is_empty())
            .map_or_else(|| format!("the key service at {} answered {}", self.base, a.status), str::to_string);
        if a.status >= 500 || a.status == 429 {
            Fault::Unreachable(format!("the key service at {} could not help just now - {message}", self.base))
        } else {
            Fault::Refused { status: a.status, code, message }
        }
    }

    // `ccc login` - a code the person approves in their browser, then the token it grants, kept for this service alone - without `browser` the caller opens the page
    pub fn login(&self, client: &str, browser: bool, say: &mut dyn FnMut(&Code)) -> Result<SignedIn> {
        let a = self.request("POST", "/api/v1/cli/device", None, Some(&json!({ "client": client })))?;
        if a.status != 200 {
            bail!(self.fault(&a));
        }
        let device: Device = serde_json::from_value(a.body).context("reading the key service's sign-in code")?;
        let opened = browser && crate::serve::open_in_browser(&device.verification_uri_complete).is_ok();
        say(&Code {
            user_code: device.user_code,
            verification_uri: device.verification_uri,
            verification_uri_complete: device.verification_uri_complete,
            expires_in: device.expires_in,
            opened,
        });
        let deadline = Instant::now() + Duration::from_secs(device.expires_in);
        // never faster than once a second, however the service asks - a test's stand-in asks for no wait at all
        let floor = if cfg!(test) { Duration::ZERO } else { Duration::from_secs(1) };
        let mut every = Duration::from_secs(device.interval).max(floor);
        loop {
            std::thread::sleep(every);
            if Instant::now() >= deadline {
                bail!("the code expired before it was approved - run `ccc login` again");
            }
            let a = match self.request("POST", "/api/v1/cli/token", None, Some(&json!({ "device_code": device.device_code }))) {
                Ok(a) => a,
                // a moment the service is out of reach is waited out while the code lasts
                Err(Fault::Unreachable(_)) => continue,
                Err(e) => return Err(e.into()),
            };
            match (a.status, a.body["error"].as_str().unwrap_or_default()) {
                (200, _) => {
                    let signed: SignedIn = serde_json::from_value(a.body).context("reading the token the key service granted")?;
                    if !signed.token.starts_with("rcc_") {
                        bail!("the key service granted a token ccc does not recognise");
                    }
                    self.keep(Some(&signed))?;
                    return Ok(signed);
                }
                (_, "authorization_pending") => {}
                (_, "slow_down") => every += Duration::from_secs(5),
                (s, _) if s >= 500 || s == 429 => {}
                _ => bail!(self.fault(&a)),
            }
        }
    }

    // what a person signing in is asked to do with the code
    pub fn asking(&self, code: &Code) -> String {
        format!(
            "ccc: to sign in to runccc{}, open {} and enter the code {}{}",
            self.at(),
            code.verification_uri,
            code.user_code,
            if code.opened { " - it is open in your browser" } else { "" }
        )
    }

    // `ccc logout` - the token revoked and forgotten here with every key it fetched, forgotten even when the service cannot be told
    pub fn logout(&self) -> Result<Option<(String, Option<Fault>)>> {
        let Some(signed) = self.signed_in() else {
            return Ok(None);
        };
        let untold = match self.request("DELETE", "/api/v1/cli/token", Some(&signed.token), None) {
            // a token the service no longer knows is as revoked as it gets
            Ok(a) if a.status == 204 || a.status == 401 => None,
            Ok(a) => Some(self.fault(&a)),
            Err(e) => Some(e),
        };
        self.keep(None)?;
        if let Some(dir) = self.keys_dir() {
            let _ = std::fs::remove_dir_all(dir);
        }
        Ok(Some((signed.login, untold)))
    }

    // `ccc whoami` - who the token says is signed in, and their teams
    pub fn me(&self) -> Result<Value, Fault> {
        let signed = self.signed_in().ok_or_else(|| self.signed_out())?;
        let a = self.request("GET", "/api/v1/me", Some(&signed.token), None)?;
        if a.status == 200 {
            Ok(a.body)
        } else {
            Err(self.fault(&a))
        }
    }

    // a project's public keys fetched fresh, as every save must - keys it gave under a day ago stand in only while it cannot be reached, with how old they are, and a refusal empties them
    pub fn keys(&self, project: &str) -> Result<(ProjectKeys, Option<Duration>), Fault> {
        let signed = self.signed_in().ok_or_else(|| self.signed_out())?;
        let cache = self.keys_dir().map(|d| d.join(format!("{project}.json")));
        let why = match self.request("GET", &format!("/api/v1/projects/{project}/keys"), Some(&signed.token), None) {
            Ok(a) if a.status == 200 => {
                let keys: ProjectKeys = serde_json::from_value(a.body)
                    .map_err(|e| Fault::Unusable(format!("the key service's keys for {project} do not read: {e}")))?;
                if let Some(c) = &cache {
                    let _ = write_private(c, json!({ "fetched": now(), "keys": keys }).to_string().as_bytes());
                }
                return Ok((keys, None));
            }
            Ok(a) if a.status >= 500 => self.fault(&a),
            Ok(a) => {
                if matches!(a.status, 401 | 403 | 404) {
                    if let Some(c) = &cache {
                        let _ = std::fs::remove_file(c);
                    }
                }
                return Err(self.fault(&a));
            }
            Err(Fault::Unreachable(why)) => Fault::Unreachable(why),
            Err(e) => return Err(e),
        };
        let kept = cache.and_then(|c| std::fs::read_to_string(c).ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok());
        let age = kept.as_ref().and_then(|k| k["fetched"].as_u64()).and_then(|t| now().checked_sub(t)).filter(|a| *a < CACHE_FOR.as_secs());
        match (kept, age) {
            (Some(k), Some(age)) => serde_json::from_value(k["keys"].clone()).map(|keys| (keys, Some(Duration::from_secs(age)))).map_err(|_| why),
            _ => Err(Fault::Unreachable(format!("{why} - and no keys it gave in the last day are kept here"))),
        }
    }

    // the file key in each stanza as the service unwraps them for a member - none for one no version of the project's key opens - in one request per 2000
    pub fn unwrap(&self, project: &str, stanzas: &[&Stanza]) -> Result<Vec<Option<FileKeyBytes>>, Fault> {
        let signed = self.signed_in().ok_or_else(|| self.signed_out())?;
        let mut keys = Vec::with_capacity(stanzas.len());
        for batch in stanzas.chunks(BATCH) {
            let wire: Vec<Value> = batch.iter().map(|s| json!({ "type": s.tag, "args": s.args, "body": STANDARD_NO_PAD.encode(&s.body) })).collect();
            let a = self.request("POST", &format!("/api/v1/projects/{project}/unwrap"), Some(&signed.token), Some(&json!({ "stanzas": wire })))?;
            if a.status != 200 {
                return Err(self.fault(&a));
            }
            let got = a.body["file_keys"]
                .as_array()
                .filter(|k| k.len() == batch.len())
                .ok_or_else(|| Fault::Unusable(format!("the key service answered {} stanza(s) with something ccc does not read", batch.len())))?;
            keys.extend(got.iter().map(|k| {
                let mut raw = STANDARD.decode(k.as_str()?).ok()?;
                let key = <[u8; FILE_KEY_BYTES]>::try_from(raw.as_slice()).ok().map(|k| SecretBox::new(Box::new(k)));
                raw.zeroize();
                key
            }));
        }
        Ok(keys)
    }
}

// the key new replay files are sealed to - the current version of a project's
pub struct Seal {
    pub project: String,
    pub version: u32,
    recipient: age::x25519::Recipient,
}

impl Seal {
    // the current version's key, from what the service gave
    pub fn of(keys: &ProjectKeys) -> Result<Seal, Fault> {
        let key = keys.keys.iter().find(|k| k.version == keys.current).ok_or_else(|| {
            Fault::Unusable(format!("the key service names key v{} of {} as current, but gave no such key", keys.current, keys.project))
        })?;
        let recipient = key
            .recipient
            .parse()
            .map_err(|e| Fault::Unusable(format!("key v{} of {} is not an age X25519 key: {e}", key.version, keys.project)))?;
        Ok(Seal { project: keys.project.clone(), version: key.version, recipient })
    }

    // bytes sealed to it - an age file only the service opens, and only for a member
    pub fn seal(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        Ok(age::encrypt(&self.recipient, bytes)?)
    }
}

// the stanzas an age file's header wraps its file key in, read without opening it - none for what is not an age file
pub fn stanzas_of(file: &[u8]) -> Option<Vec<Stanza>> {
    struct Look(RefCell<Vec<Stanza>>);
    impl age::Identity for Look {
        fn unwrap_stanza(&self, _: &Stanza) -> Option<Result<FileKey, age::DecryptError>> {
            None
        }
        fn unwrap_stanzas(&self, stanzas: &[Stanza]) -> Option<Result<FileKey, age::DecryptError>> {
            let copies = stanzas.iter().map(|s| Stanza { tag: s.tag.clone(), args: s.args.clone(), body: s.body.clone() });
            self.0.borrow_mut().extend(copies);
            None
        }
    }
    let look = Look(RefCell::new(Vec::new()));
    let file = age::Decryptor::new_buffered(file).ok()?;
    let _ = file.decrypt(std::iter::once(&look as &dyn age::Identity));
    Some(look.0.into_inner())
}

// a file key the service unwrapped - wiped from memory when it is dropped
pub type FileKeyBytes = SecretBox<[u8; FILE_KEY_BYTES]>;

// file keys the service unwrapped, by the stanza each came from - the age identity a sealed replay opens with, held in memory only
#[derive(Default)]
pub struct Keyring(HashMap<(Vec<String>, Vec<u8>), FileKeyBytes>);

impl age::Identity for Keyring {
    fn unwrap_stanza(&self, stanza: &Stanza) -> Option<Result<FileKey, age::DecryptError>> {
        if stanza.tag != X25519 {
            return None;
        }
        let key = self.0.get(&(stanza.args.clone(), stanza.body.clone()))?;
        Some(Ok(FileKey::init_with_mut(|k| k.copy_from_slice(key.expose_secret()))))
    }
}

impl Keyring {
    // one sealed file opened - an error when none of the keys the service gave opens it
    pub fn open(&self, file: &[u8]) -> Result<Vec<u8>, String> {
        age::decrypt(self, file).map_err(|e| e.to_string())
    }
}

// a repository's replays as map.json seals them - the project, and the service that holds its key
#[derive(Debug, Clone)]
pub struct Team {
    pub project: String,
    pub client: Client,
}

// the team a repository's map.json seals replays to - an unusable map.json that mentions encryption is an error, never a reason to save in the clear
pub fn team(root: &Path) -> Result<Option<Team>, Fault> {
    let config = match crate::changes::ChangesConfig::load(root) {
        Ok(c) => c,
        Err(e) => {
            let raw = crate::changes::ChangesConfig::path(root).and_then(|p| std::fs::read_to_string(p).ok());
            return match raw.is_some_and(|t| t.contains("\"encrypt\"")) {
                true => Err(Fault::Unusable(format!("{e:#} - so ccc cannot tell which runccc project encrypts its replays"))),
                false => Ok(None),
            };
        }
    };
    let Some(encrypt) = config.replays.encrypt else {
        return Ok(None);
    };
    if !project_ok(&encrypt.project) {
        return Err(Fault::Unusable(format!(
            "replays.encrypt.project in .ccc/map.json ({:?}) is not a runccc project id",
            encrypt.project
        )));
    }
    Ok(Some(Team { project: encrypt.project, client: Client::new(&service_url(encrypt.service.as_deref())) }))
}

// the service `ccc login` and `ccc whoami` talk to in a repository - the one its map.json names, else runccc's own
pub fn client_for(root: &Path) -> Client {
    let named = crate::changes::ChangesConfig::load(root).ok().and_then(|c| c.replays.encrypt).and_then(|e| e.service);
    Client::new(&service_url(named.as_deref()))
}

// how a sign-in names this machine on the approval page and in the person's token list
pub fn this_machine() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(|| Command::new("hostname").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()))
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "this machine".to_string());
    format!("ccc {} on {host}", env!("CARGO_PKG_VERSION"))
}

impl Team {
    // the key a save seals to - fetched fresh and only while the project is paid for, with a note when keys cached earlier stood in
    pub fn seal(&self) -> Result<(Seal, Option<String>), Fault> {
        let (keys, cached) = self.client.keys(&self.project)?;
        if keys.status != "active" {
            return Err(Fault::Refused {
                status: 402,
                code: "payment_required".into(),
                message: format!(
                    "runccc project {} (team {}) is not paid for, so nothing sealed to it could be opened - replays are saved again once the team's owner pays",
                    keys.project, keys.team
                ),
            });
        }
        let seal = Seal::of(&keys)?;
        let note = cached.map(|age| {
            format!(
                "the key service could not be reached, so the replay was sealed to key v{} as it was {}h ago",
                seal.version,
                age.as_secs() / 3600
            )
        });
        Ok((seal, note))
    }

    // the file key of every sealed file given, from one request to the service per 2000 stanzas
    pub fn keyring(&self, files: &[&[u8]]) -> Result<Keyring, Fault> {
        let stanzas: Vec<Stanza> = files.iter().filter_map(|f| stanzas_of(f)).flatten().filter(|s| s.tag == X25519).collect();
        let mut seen = HashSet::new();
        let unique: Vec<&Stanza> = stanzas.iter().filter(|s| seen.insert((&s.args, &s.body))).collect();
        let mut ring = Keyring::default();
        if unique.is_empty() {
            return Ok(ring);
        }
        let keys = self.client.unwrap(&self.project, &unique)?;
        for (s, key) in unique.into_iter().zip(keys) {
            if let Some(key) = key {
                ring.0.insert((s.args.clone(), s.body.clone()), key);
            }
        }
        Ok(ring)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    // a stand-in for the key service on a free port - `answer` says what each request gets, and every request is kept
    pub(crate) struct Mock {
        pub base: String,
        pub asked: Arc<Mutex<Vec<String>>>,
        server: Arc<tiny_http::Server>,
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.server.unblock();
        }
    }

    impl Mock {
        // how many requests went to a path
        pub(crate) fn count(&self, path: &str) -> usize {
            self.asked.lock().unwrap().iter().filter(|a| a.ends_with(path)).count()
        }
    }

    pub(crate) fn mock(answer: impl Fn(&str, &str, Option<&str>, &Value) -> (u16, Value) + Send + 'static) -> Mock {
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let base = format!("http://{}", server.server_addr().to_ip().unwrap());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let (s, kept) = (server.clone(), asked.clone());
        std::thread::spawn(move || {
            for mut req in s.incoming_requests() {
                let mut body = String::new();
                let _ = req.as_reader().read_to_string(&mut body);
                let auth = req.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.to_string());
                let (method, path) = (req.method().to_string(), req.url().to_string());
                kept.lock().unwrap().push(format!("{method} {path}"));
                let token = auth.as_deref().and_then(|a| a.strip_prefix("Bearer "));
                let (status, reply) = answer(&method, &path, token, &serde_json::from_str(&body).unwrap_or(Value::Null));
                let json = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
                let _ = req.respond(tiny_http::Response::from_string(reply.to_string()).with_status_code(status).with_header(json));
            }
        });
        Mock { base, asked, server }
    }

    pub(crate) const PROJECT: &str = "prj_test";
    pub(crate) const TOKEN: &str = "rcc_test";

    // the service's side of one project as tests play it - its key versions with their private halves, and a caller who can be taken off the team or left unpaid
    pub(crate) struct Project {
        pub keys: Vec<age::x25519::Identity>,
        pub member: bool,
        pub paid: bool,
        pub down: bool,
    }

    impl Project {
        pub(crate) fn new() -> Arc<Mutex<Project>> {
            Arc::new(Mutex::new(Project { keys: vec![age::x25519::Identity::generate()], member: true, paid: true, down: false }))
        }
    }

    // a key service holding one project, answering as API.md says
    pub(crate) fn service(project: Arc<Mutex<Project>>) -> Mock {
        mock(move |method, path, token, body| {
            let p = project.lock().unwrap();
            let refuse = |status, code: &str, message: &str| (status, json!({ "error": code, "message": message }));
            if p.down {
                return refuse(503, "server_error", "Something went wrong on our side. Try again in a moment.");
            }
            if token != Some(TOKEN) {
                return refuse(401, "invalid_token", "Sign in with `ccc login`.");
            }
            let keys = format!("/api/v1/projects/{PROJECT}/keys");
            let unwrap = format!("/api/v1/projects/{PROJECT}/unwrap");
            if path != keys && path != unwrap {
                return refuse(404, "no_such_project", "There's no runccc project with that id. Check .ccc/map.json.");
            }
            if !p.member {
                return refuse(403, "not_a_member", "@dev isn't on the team that owns this project. Ask its owner for an invite.");
            }
            if method == "GET" {
                let versions: Vec<Value> = p
                    .keys
                    .iter()
                    .enumerate()
                    .rev()
                    .map(|(i, k)| json!({ "version": i + 1, "recipient": k.to_public().to_string(), "created_at": "2026-10-01T00:00:00Z" }))
                    .collect();
                let status = if p.paid { "active" } else { "unpaid" };
                return (200, json!({ "project": PROJECT, "name": "app", "team": "acme", "status": status, "current": p.keys.len(), "keys": versions }));
            }
            if !p.paid {
                return refuse(402, "payment_required", "This repository isn't paid for, so its replays can't be opened.");
            }
            let opened: Vec<Option<String>> = body["stanzas"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| {
                    let stanza = Stanza {
                        tag: s["type"].as_str()?.to_string(),
                        args: s["args"].as_array()?.iter().filter_map(|a| a.as_str().map(str::to_string)).collect(),
                        body: STANDARD_NO_PAD.decode(s["body"].as_str()?.trim_end_matches('=')).ok()?,
                    };
                    let key = p.keys.iter().find_map(|k| age::Identity::unwrap_stanza(k, &stanza)?.ok())?;
                    Some(STANDARD.encode(key.expose_secret()))
                })
                .collect();
            (200, json!({ "file_keys": opened }))
        })
    }

    // a client of `base` keeping its sign-in in `dir`, signed in or not
    pub(crate) fn client(base: &str, dir: &Path, signed: bool) -> Client {
        let c = Client { base: base.to_string(), dir: Some(dir.to_path_buf()) };
        if signed {
            c.keep(Some(&SignedIn { token: TOKEN.into(), login: "dev".into() })).unwrap();
        }
        c
    }

    // the config dir moves with `CCC_CONFIG_DIR`, so a test keeps its sign-in to itself - one at a time, as the variable is the process's
    pub(crate) fn in_config<T>(tag: &str, test: impl FnOnce(&Path) -> T) -> T {
        static ONE: Mutex<()> = Mutex::new(());
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ccc-runccc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("CCC_CONFIG_DIR", &dir);
        std::env::remove_var("RUNCCC_URL");
        let out = test(&dir.join("runccc"));
        std::env::remove_var("CCC_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ccc-runccc-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    // a token goes only to the service that granted it, in a file only its owner reads
    #[test]
    fn a_token_is_kept_for_its_service_alone() {
        let dir = temp("token");
        let c = client("http://localhost:1", &dir, true);
        assert_eq!(c.signed_in().map(|s| s.login).as_deref(), Some("dev"));
        assert!(client("https://elsewhere.example", &dir, false).signed_in().is_none(), "never sent to another service");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("credentials.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // signing out of one forgets it alone, even when the service cannot be told
        let (login, untold) = c.logout().unwrap().unwrap();
        assert_eq!(login, "dev");
        assert!(matches!(untold, Some(Fault::Unreachable(_))), "{untold:?}");
        assert!(c.signed_in().is_none() && c.logout().unwrap().is_none());
        assert!(project_ok("prj_7k2m9x4q8r1t5v3w6y0z") && !project_ok("../x") && !project_ok("") && !project_ok(&"a".repeat(65)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // keys are fetched fresh - an answer under a day old stands in only while the service is down, and a refusal empties it
    #[test]
    fn keys_cached_stand_in_only_while_the_service_is_down() {
        let dir = temp("keys");
        let project = Project::new();
        let s = service(project.clone());
        let c = client(&s.base, &dir, true);
        let (keys, cached) = c.keys(PROJECT).unwrap();
        assert_eq!((keys.current, keys.status.as_str(), cached), (1, "active", None));
        let file = c.keys_dir().unwrap().join(format!("{PROJECT}.json"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }

        project.lock().unwrap().down = true;
        let (_, cached) = c.keys(PROJECT).unwrap();
        assert!(cached.is_some(), "a 5xx falls back to what was kept");
        let mut kept: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        kept["fetched"] = json!(now() - 25 * 3600);
        std::fs::write(&file, kept.to_string()).unwrap();
        let stale = c.keys(PROJECT).unwrap_err();
        assert!(stale.to_string().contains("no keys it gave in the last day"), "{stale}");
        drop(s);
        let gone = client("http://127.0.0.1:1", &dir, true).keys(PROJECT).unwrap_err();
        assert!(matches!(gone, Fault::Unreachable(_)), "{gone:?}");

        let project = Project::new();
        let s = service(project.clone());
        let c = client(&s.base, &dir, true);
        c.keys(PROJECT).unwrap();
        let file = c.keys_dir().unwrap().join(format!("{PROJECT}.json"));
        assert!(file.exists());
        project.lock().unwrap().member = false;
        let refused = c.keys(PROJECT).unwrap_err();
        assert_eq!(refused.status(), 403);
        assert!(refused.to_string().contains("isn't on the team"), "{refused}");
        assert!(!file.exists(), "a refusal empties the cache");
        project.lock().unwrap().down = true;
        assert!(c.keys(PROJECT).is_err(), "and nothing stands in after it");
        assert!(matches!(client(&s.base, &dir.join("nobody"), false).keys(PROJECT), Err(Fault::SignedOut(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // a file sealed to any version of the project's key opens through the service - for a member, while it is paid for
    #[test]
    fn a_sealed_file_opens_only_through_the_service() {
        let dir = temp("open");
        let project = Project::new();
        let s = service(project.clone());
        let team = Team { project: PROJECT.into(), client: client(&s.base, &dir, true) };
        let (seal, note) = team.seal().unwrap();
        assert_eq!((seal.version, note), (1, None));
        let first = seal.seal(b"the first save").unwrap();
        assert!(!first.windows(5).any(|w| w == b"first"), "nothing in the clear");
        project.lock().unwrap().keys.push(age::x25519::Identity::generate());
        let (seal, _) = team.seal().unwrap();
        assert_eq!(seal.version, 2, "a new version is sealed to");
        let second = seal.seal(b"the second save").unwrap();

        let ring = team.keyring(&[&first, &second]).unwrap();
        assert_eq!(s.count("/unwrap"), 1, "one request for both");
        assert_eq!(ring.open(&first).unwrap(), b"the first save");
        assert_eq!(ring.open(&second).unwrap(), b"the second save");
        let other = Seal::of(&ProjectKeys {
            project: "prj_other".into(),
            name: String::new(),
            team: String::new(),
            status: "active".into(),
            current: 1,
            keys: vec![KeyVersion { version: 1, recipient: age::x25519::Identity::generate().to_public().to_string(), created_at: String::new() }],
        })
        .unwrap();
        let foreign = other.seal(b"another project's").unwrap();
        assert!(team.keyring(&[&foreign]).unwrap().open(&foreign).is_err(), "no version of this project's key opens it");

        project.lock().unwrap().paid = false;
        let unpaid = team.keyring(&[&first]).err().unwrap();
        assert_eq!(unpaid.status(), 402);
        assert!(unpaid.to_string().contains("isn't paid for"), "{unpaid}");
        assert!(team.seal().err().unwrap().to_string().contains("not paid for"), "nothing saved while unpaid");
        project.lock().unwrap().member = false;
        assert!(team.keyring(&[&first]).err().unwrap().to_string().contains("isn't on the team"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // a replay of more than 2000 files is opened in as few requests as the service allows
    #[test]
    fn stanzas_go_to_the_service_two_thousand_at_a_time() {
        let dir = temp("batch");
        let s = mock(|_, _, _, body| {
            let n = body["stanzas"].as_array().map_or(0, Vec::len);
            (200, json!({ "file_keys": vec![Value::Null; n] }))
        });
        let c = client(&s.base, &dir, true);
        let stanzas: Vec<Stanza> = (0..2001u32).map(|i| Stanza { tag: X25519.into(), args: vec![i.to_string()], body: vec![1; 32] }).collect();
        let keys = c.unwrap(PROJECT, &stanzas.iter().collect::<Vec<_>>()).unwrap();
        assert_eq!((keys.len(), s.count("/unwrap")), (2001, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // `ccc login` polls while the person has yet to approve, and keeps the token it is granted
    #[test]
    fn a_sign_in_waits_for_approval() {
        let dir = temp("login");
        let polls = Arc::new(Mutex::new(0));
        let p = polls.clone();
        let s = mock(move |_, path, _, body| match path {
            "/api/v1/cli/device" => {
                assert_eq!(body["client"], "ccc test");
                (200, json!({ "device_code": "dc_1", "user_code": "", "verification_uri": "http://x/cli", "verification_uri_complete": "http://x/cli?code=", "expires_in": 60, "interval": 0 }))
            }
            _ => {
                let mut n = p.lock().unwrap();
                *n += 1;
                match *n {
                    1 => (400, json!({ "error": "authorization_pending", "message": "Waiting for you to approve the sign-in in your browser." })),
                    _ => (200, json!({ "token": "rcc_granted", "login": "octocat" })), // ccc:allow-secret
                }
            }
        });
        let c = Client { base: s.base.clone(), dir: Some(dir.clone()) };
        let mut said = Vec::new();
        let signed = c.login("ccc test", false, &mut |code| said.push(c.asking(code))).unwrap();
        assert_eq!((signed.login.as_str(), *polls.lock().unwrap()), ("octocat", 2));
        assert!(said[0].contains("") && said[0].contains("http://x/cli"), "{said:?}");
        assert_eq!(c.signed_in().unwrap().token, "rcc_granted");

        let denied = mock(|_, path, _, _| match path {
            "/api/v1/cli/device" => (200, json!({ "device_code": "dc_1", "user_code": "A", "verification_uri": "u", "verification_uri_complete": "u", "expires_in": 60, "interval": 0 })),
            _ => (400, json!({ "error": "access_denied", "message": "The sign-in was denied in the browser." })),
        });
        let c = Client { base: denied.base.clone(), dir: Some(dir.clone()) };
        assert!(c.login("ccc test", false, &mut |_| {}).unwrap_err().to_string().contains("denied"));
        assert!(c.signed_in().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
