//! The narration voice - Kokoro, downloaded once into a cache every ccc on the machine shares, served to the visualiser that runs it

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

// the voice's folder in the cache, named for the model and its revision
const PACK: &str = "kokoro-82m-v1.0";
// the model the visualiser asks for by name
pub const MODEL: &str = "onnx-community/Kokoro-82M-v1.0-ONNX-timestamped";
// the speaker it reads with
pub const SPEAKER: &str = "af_heart";
// held while one ccc downloads, so another waits on it rather than fetching the same files
const LOCK: &str = "download.lock";
// why the last download stopped, for the visualiser to say
const FAILED: &str = "download.error";
// a lock no one has touched this long is a download that died
const STALE: Duration = Duration::from_secs(60);
// a line of narration is a few seconds of speech - nothing bigger is kept
const LINE_MAX: usize = 4 << 20;

// one file of the voice: where it comes from, and the size and hash it must have
pub struct VoiceFile {
    pub name: &'static str,
    pub url: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

// every file the voice runs from, pinned to the revision ccc was built against - the model, its speaker, and the runtime the page runs it with
pub const FILES: &[VoiceFile] = &[
    VoiceFile {
        name: "kokoro.web.js",
        url: "https://cdn.jsdelivr.net/npm/kokoro-js@1.2.1/dist/kokoro.web.js",
        size: 2_135_146,
        sha256: "6067712fe4c43cdb36c762f860a5ab8d96ea1d9c8882466438eb9f3d0d20be8f",
    },
    VoiceFile {
        name: "ort-wasm-simd-threaded.jsep.mjs",
        url: "https://cdn.jsdelivr.net/npm/@huggingface/transformers@3.5.1/dist/ort-wasm-simd-threaded.jsep.mjs",
        size: 44_484,
        sha256: "08fb86ec433c78bfb032c5d84a68b8e8e5a8d81268fa39e24314179a5767a5b9",
    },
    VoiceFile {
        name: "ort-wasm-simd-threaded.jsep.wasm",
        url: "https://cdn.jsdelivr.net/npm/@huggingface/transformers@3.5.1/dist/ort-wasm-simd-threaded.jsep.wasm",
        size: 21_596_019,
        sha256: "c46655e8a94afc45338d4cb2b840475f88e5012d524509916e505079c00bfa39",
    },
    VoiceFile {
        name: "config.json",
        url: "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX-timestamped/resolve/dd4401a9add81ac692d20e240d22ec9dda82cc29/config.json",
        size: 44,
        sha256: "df34b4f930b23447cd4dc410fabfb42eb3f24e803e6c3f97d618fb359380a36f",
    },
    VoiceFile {
        name: "tokenizer.json",
        url: "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX-timestamped/resolve/dd4401a9add81ac692d20e240d22ec9dda82cc29/tokenizer.json",
        size: 3_497,
        sha256: "77a02c8e164413299b4b4c403b14f8e0e1c1b727db4d46a09d6327b861060a34",
    },
    VoiceFile {
        name: "tokenizer_config.json",
        url: "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX-timestamped/resolve/dd4401a9add81ac692d20e240d22ec9dda82cc29/tokenizer_config.json",
        size: 113,
        sha256: "be1cb066d6ef6b074b3f15e6a6dd21ac88ff3cdaedf325f0aaed686c70f75d20",
    },
    VoiceFile {
        name: "af_heart.bin",
        url: "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX/resolve/1939ad2a8e416c0acfeecc08a694d14ef25f2231/voices/af_heart.bin",
        size: 522_240,
        sha256: "d583ccff3cdca2f7fae535cb998ac07e9fcb90f09737b9a41fa2734ec44a8f0b",
    },
    VoiceFile {
        name: "model.onnx",
        url: "https://huggingface.co/onnx-community/Kokoro-82M-v1.0-ONNX-timestamped/resolve/dd4401a9add81ac692d20e240d22ec9dda82cc29/onnx/model.onnx",
        size: 325_532_171,
        sha256: "651ea8291843a92276a4a003581a215cb07d15e47dde6fcfb1b768f9a1682054",
    },
];

// what every ccc on this machine shares - the user's cache, never a temp dir a reboot clears; `CCC_CACHE_DIR` moves it
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CCC_CACHE_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = || std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).filter(|h| !h.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        return std::env::var_os("LOCALAPPDATA").filter(|d| !d.is_empty()).map(|d| PathBuf::from(d).join("ccc").join("cache"));
    }
    if cfg!(target_os = "macos") {
        return home().map(|h| h.join("Library").join("Caches").join("ccc"));
    }
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".cache")))
        .map(|d| d.join("ccc"))
}

// where the voice's files live
pub fn pack_dir() -> Option<PathBuf> {
    cache_dir().map(|d| d.join("voices").join(PACK))
}

// where the lines it has read are kept
pub fn lines_dir() -> Option<PathBuf> {
    cache_dir().map(|d| d.join("voices").join("lines"))
}

// the whole download, in bytes
pub fn total() -> u64 {
    FILES.iter().map(|f| f.size).sum()
}

// a file the download finished - it is only put in place once its hash matched
fn have(dir: &Path, f: &VoiceFile) -> bool {
    std::fs::metadata(dir.join(f.name)).is_ok_and(|m| m.len() == f.size)
}

fn part(dir: &Path, f: &VoiceFile) -> PathBuf {
    dir.join(format!("{}.part", f.name))
}

// is another download at it - its lock touched lately
fn downloading(dir: &Path) -> bool {
    std::fs::metadata(dir.join(LOCK)).and_then(|m| m.modified()).is_ok_and(|t| SystemTime::now().duration_since(t).unwrap_or_default() < STALE)
}

// the voice is on this machine, every file of it
pub fn ready() -> bool {
    pack_dir().is_some_and(|d| FILES.iter().all(|f| have(&d, f)))
}

// where the voice stands on this machine, for the visualiser to say
pub fn status() -> Value {
    let Some(dir) = pack_dir() else {
        return json!({"state": "unavailable", "error": "this machine has no cache directory to keep the voice in", "total": total()});
    };
    let done: u64 = FILES
        .iter()
        .map(|f| if have(&dir, f) { f.size } else { std::fs::metadata(part(&dir, f)).map_or(0, |m| m.len().min(f.size)) })
        .sum();
    let error = std::fs::read_to_string(dir.join(FAILED)).ok().map(|e| e.trim().to_string()).filter(|e| !e.is_empty());
    let state = if FILES.iter().all(|f| have(&dir, f)) {
        "ready"
    } else if downloading(&dir) {
        "downloading"
    } else if error.is_some() {
        "failed"
    } else {
        "absent"
    };
    json!({
        "state": state,
        "done": done,
        "total": total(),
        "dir": dir,
        "model": MODEL,
        "speaker": SPEAKER,
        "error": error,
    })
}

// whether this ccc took the download - false while another has it, and a lock left by one that died is taken over
fn take_lock(dir: &Path) -> Result<bool> {
    let lock = dir.join(LOCK);
    for _ in 0..2 {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&lock) {
            Ok(_) => return Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if downloading(dir) {
                    return Ok(false);
                }
                let _ = std::fs::remove_file(&lock);
            }
            Err(e) => return Err(e).with_context(|| format!("locking {}", lock.display())),
        }
    }
    Ok(false)
}

// the lock kept fresh, so another ccc knows this download is alive
fn touch(path: &Path) {
    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

// the voice fetched in the background, once - a second ccc asking meanwhile leaves it to the first, and the visualiser follows either
pub fn start_download() -> Result<()> {
    let dir = pack_dir().context("this machine has no cache directory to keep the voice in")?;
    if ready() {
        return Ok(());
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    if !take_lock(&dir)? {
        return Ok(());
    }
    let _ = std::fs::remove_file(dir.join(FAILED));
    std::thread::spawn(move || finish(&dir, fetch_all(&dir)));
    Ok(())
}

// fetch the voice and wait for it - `ccc voice download` - saying how far it has got
pub fn download_now(mut said: impl FnMut(u64, u64)) -> Result<()> {
    let dir = pack_dir().context("this machine has no cache directory to keep the voice in")?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    while !ready() {
        if take_lock(&dir)? {
            let _ = std::fs::remove_file(dir.join(FAILED));
            let watching = dir.clone();
            let fetch = std::thread::spawn(move || fetch_all(&watching));
            while !fetch.is_finished() {
                said(status()["done"].as_u64().unwrap_or(0), total());
                std::thread::sleep(Duration::from_millis(500));
            }
            let result = fetch.join().unwrap_or_else(|_| Err(anyhow::anyhow!("the download stopped")));
            finish(&dir, result);
            if let Some(e) = status()["error"].as_str() {
                bail!("{e}");
            }
        } else {
            // another ccc is at it
            said(status()["done"].as_u64().unwrap_or(0), total());
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    said(total(), total());
    Ok(())
}

// a download done: the lock let go, and why it stopped kept if it failed
fn finish(dir: &Path, result: Result<()>) {
    if let Err(e) = result {
        let _ = std::fs::write(dir.join(FAILED), format!("{e:#}"));
    }
    let _ = std::fs::remove_file(dir.join(LOCK));
}

fn fetch_all(dir: &Path) -> Result<()> {
    for f in FILES.iter().filter(|f| !have(dir, f)) {
        fetch(dir, f)?;
    }
    Ok(())
}

// one file, carried on from where an earlier try stopped, checked against its pin and only then put in place
fn fetch(dir: &Path, f: &VoiceFile) -> Result<()> {
    let part = part(dir, f);
    let lock = dir.join(LOCK);
    let got = std::fs::metadata(&part).map_or(0, |m| m.len());
    if got > f.size {
        let _ = std::fs::remove_file(&part);
    }
    if got != f.size {
        let mut curl = Command::new("curl")
            .args(["-fsSL", "--retry", "3", "--retry-delay", "2", "-C", "-", "-o"])
            .arg(&part)
            .arg(f.url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("running curl - ccc downloads the voice with it, so it must be on the PATH")?;
        let status = loop {
            if let Some(status) = curl.try_wait()? {
                break status;
            }
            touch(&lock);
            std::thread::sleep(Duration::from_millis(500));
        };
        if !status.success() {
            let mut said = String::new();
            if let Some(mut e) = curl.stderr.take() {
                let _ = e.read_to_string(&mut said);
            }
            bail!("downloading {} from {}: {}", f.name, f.url, said.trim());
        }
    }
    if let Err(e) = verify(&part, f) {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    std::fs::rename(&part, dir.join(f.name)).with_context(|| format!("putting {} in place", f.name))?;
    Ok(())
}

// a file is used only when it is the size and hash pinned for it
fn verify(path: &Path, f: &VoiceFile) -> Result<()> {
    let len = std::fs::metadata(path)?.len();
    if len != f.size {
        bail!("{} came to {len} bytes, not the {} pinned for it", f.name, f.size);
    }
    let got = sha256_file(path)?;
    if got != f.sha256 {
        bail!("{} does not match the hash pinned for it, so it is not used", f.name);
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hex(&hash.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// the voice off this machine, and every line it read - none while a download runs
pub fn remove() -> Result<u64> {
    let mut freed = 0;
    if let Some(dir) = pack_dir() {
        if downloading(&dir) {
            bail!("the voice is downloading - wait for it, then remove it");
        }
    }
    for dir in [pack_dir(), lines_dir()].into_iter().flatten() {
        freed += size_of(&dir);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        }
    }
    Ok(freed)
}

fn size_of(dir: &Path) -> u64 {
    std::fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
}

// one of the voice's files, to serve - with its media type - once it is in place
pub fn file(name: &str) -> Option<(PathBuf, &'static str)> {
    let dir = pack_dir()?;
    let f = FILES.iter().find(|f| f.name == name)?;
    if !have(&dir, f) {
        return None;
    }
    let ty = match Path::new(f.name).extension().and_then(|e| e.to_str()) {
        Some("js" | "mjs") => "text/javascript",
        Some("wasm") => "application/wasm",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    };
    Some((dir.join(f.name), ty))
}

// a kept line's file, its audio or its word timings - named for the hex sha-256 of what it says, and nothing else names a file here
fn line_file(name: &str) -> Option<(PathBuf, &'static str)> {
    let (fp, ext) = name.rsplit_once('.')?;
    let ty = match ext {
        "webm" => "audio/webm",
        "json" => "application/json",
        _ => return None,
    };
    let named = fp.len() == 64 && fp.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    named.then(|| lines_dir().map(|d| (d.join(name), ty))).flatten()
}

// a line the voice read before, with its media type
pub fn line(name: &str) -> Option<(Vec<u8>, &'static str)> {
    let (path, ty) = line_file(name)?;
    std::fs::read(path).ok().map(|b| (b, ty))
}

// the media type a line's file is served as - none for a name that is not a line's
pub fn line_type(name: &str) -> Option<&'static str> {
    line_file(name).map(|(_, ty)| ty)
}

// where a project notes the step each kept line reads, so a branch's replay knows the lines it carries
const INDEX: &str = "voice-lines.jsonl";

// a step as the voice's index knows it - when it landed, its changeset, its call and its part, which a saved replay keeps too
pub fn step_key(v: &Value) -> String {
    let part = v["part"]["index"].as_u64().map_or(String::new(), |i| i.to_string());
    format!("{}|{}|{}|{part}", v["at"].as_u64().unwrap_or(0), v["changeset"].as_str().unwrap_or(""), v["call"].as_str().unwrap_or(""))
}

// an ask's plan or outcome as the voice's index knows it - the story's first and last lines, told beside its steps
pub fn ask_key(id: &Value, moment: &str) -> Option<String> {
    let id = id.as_str().filter(|i| !i.is_empty())?;
    matches!(moment, "plan" | "outcome").then(|| format!("ask|{id}|{moment}"))
}

// note that a kept line reads a step of this project, or its ask's plan or outcome - its timings name which
pub fn note_line(root: &Path, name: &str, meta: &[u8]) -> Result<()> {
    let Some((fp, "json")) = name.rsplit_once('.') else {
        return Ok(());
    };
    let v: Value = serde_json::from_slice(meta).context("a line's timings are json")?;
    let key = if v["step"].is_null() {
        match ask_key(&v["ask"]["id"], v["ask"]["moment"].as_str().unwrap_or_default()) {
            Some(k) => k,
            None => return Ok(()),
        }
    } else {
        step_key(&v["step"])
    };
    let path = root.join(".ccc").join(INDEX);
    std::fs::create_dir_all(path.parent().unwrap_or(root))?;
    let line = format!("{}\n", json!({"fp": fp, "step": key}));
    // a step read with the same line again is noted once
    if std::fs::read_to_string(&path).is_ok_and(|t| t.contains(&line)) {
        return Ok(());
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    std::io::Write::write_all(&mut f, line.as_bytes())?;
    let _ = crate::prompts::ignore_path(root, &format!("/.ccc/{INDEX}"), "the narrated lines each step was read with");
    Ok(())
}

// has this project noted any line it read - without one there is no narration to share
pub fn noted(root: &Path) -> bool {
    std::fs::metadata(root.join(".ccc").join(INDEX)).is_ok_and(|m| m.len() > 0)
}

// the lines kept on this machine for these steps and the plan and outcome of each ask they carry, by what each reads - the latest reading wins
pub fn lines_for(root: &Path, steps: &[Value]) -> std::collections::BTreeMap<String, String> {
    let mut want: std::collections::BTreeSet<String> = steps.iter().map(step_key).collect();
    want.extend(steps.iter().flat_map(|v| ["plan", "outcome"].into_iter().filter_map(|m| ask_key(&v["ask"]["id"], m))));
    let mut found = std::collections::BTreeMap::new();
    let text = std::fs::read_to_string(root.join(".ccc").join(INDEX)).unwrap_or_default();
    for l in text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()) {
        if let (Some(step), Some(fp)) = (l["step"].as_str(), l["fp"].as_str()) {
            if want.contains(step) {
                found.insert(step.to_string(), fp.to_string());
            }
        }
    }
    found.retain(|_, fp| line(&format!("{fp}.webm")).is_some() && line(&format!("{fp}.json")).is_some());
    found
}

// keep a line the visualiser read, for every later replay of it
pub fn keep_line(name: &str, bytes: &[u8]) -> Result<()> {
    let (path, _) = line_file(name).context("a line is named `<sha-256 in hex>.webm` or `.json`")?;
    if bytes.is_empty() || bytes.len() > LINE_MAX {
        bail!("a line is between 1 byte and {} MiB", LINE_MAX >> 20);
    }
    let dir = path.parent().context("the lines' folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    // written whole, then put in place, so a reader never sees half a line
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // the cache dir moves with `CCC_CACHE_DIR`, so a test keeps to its own - one at a time, as the variable is the process's
    pub(crate) fn in_cache<T>(tag: &str, test: impl FnOnce(&Path) -> T) -> T {
        static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _held = ONE.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("ccc-voice-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("CCC_CACHE_DIR", &dir);
        let out = test(&dir);
        std::env::remove_var("CCC_CACHE_DIR");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    // nothing downloaded is absent; a download another ccc holds is downloading; a stale lock is not
    #[test]
    fn the_voice_says_where_it_stands() {
        in_cache("status", |dir| {
            assert_eq!(status()["state"], "absent");
            assert_eq!(status()["total"], total());
            let pack = dir.join("voices").join(PACK);
            std::fs::create_dir_all(&pack).unwrap();
            assert!(take_lock(&pack).unwrap());
            assert!(!take_lock(&pack).unwrap(), "one download at a time");
            assert_eq!(status()["state"], "downloading");
            std::fs::write(part(&pack, &FILES[0]), b"abc").unwrap();
            assert_eq!(status()["done"], 3);
            finish(&pack, Err(anyhow::anyhow!("no network")));
            assert_eq!(status()["state"], "failed");
            assert_eq!(status()["error"], "no network");
            assert!(file("model.onnx").is_none(), "nothing is served until it is in place");
        });
    }

    // a file is put in place only when it is the size and hash pinned for it
    #[test]
    fn a_file_must_match_its_pin() {
        in_cache("verify", |dir| {
            std::fs::create_dir_all(dir).unwrap();
            let path = dir.join("f");
            std::fs::write(&path, b"abc").unwrap();
            let pin = |size, sha256| VoiceFile { name: "f", url: "", size, sha256 };
            assert!(verify(&path, &pin(3, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")).is_ok());
            assert!(verify(&path, &pin(3, "00")).is_err());
            assert!(verify(&path, &pin(4, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")).is_err());
        });
    }

    // a line is kept under its fingerprint, and nothing else names a file
    #[test]
    fn a_line_is_kept_by_its_fingerprint() {
        in_cache("lines", |_| {
            let fp = "a".repeat(64);
            keep_line(&format!("{fp}.webm"), b"opus").unwrap();
            assert_eq!(line(&format!("{fp}.webm")), Some((b"opus".to_vec(), "audio/webm")));
            assert!(line(&format!("{fp}.json")).is_none());
            for bad in ["../x.webm", &format!("{}.webm", "A".repeat(64)), &format!("{fp}.wav"), "short.webm"] {
                assert!(keep_line(bad, b"x").is_err(), "{bad}");
            }
            assert!(keep_line(&format!("{fp}.json"), b"").is_err());
            assert!(remove().unwrap() >= 4);
            assert!(line(&format!("{fp}.webm")).is_none());
        });
    }

    // an ask's plan is noted by its ask, and goes with the steps that carry that ask
    #[test]
    fn an_asks_plan_goes_with_its_steps() {
        in_cache("asks", |dir| {
            let root = dir.join("project");
            let fp = "c".repeat(64);
            let meta = json!({"duration": 1.0, "words": [], "step": null, "ask": {"id": "claude:s:1", "moment": "plan"}}).to_string();
            keep_line(&format!("{fp}.webm"), b"opus").unwrap();
            keep_line(&format!("{fp}.json"), meta.as_bytes()).unwrap();
            note_line(&root, &format!("{fp}.json"), meta.as_bytes()).unwrap();
            let step = json!({"at": 1, "changeset": "c1-x", "ask": {"id": "claude:s:1"}});
            assert_eq!(lines_for(&root, &[step]).get("ask|claude:s:1|plan"), Some(&fp));
            assert!(lines_for(&root, &[json!({"at": 1, "ask": {"id": "claude:s:2"}})]).is_empty(), "another ask's plan stays home");
            assert_eq!(ask_key(&json!("claude:s:1"), "beat"), None);
        });
    }
}
