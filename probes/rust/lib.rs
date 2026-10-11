//! ccc's rust probe - records which functions ran on behalf of which test fixture and how many times, as ccc-observed/1 lines
//! off unless CCC_OBSERVED_DIR names a directory - `ccc probe rust` puts the calls into a copy of the crate and this crate beside it

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, Once, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

const SCHEMA: &str = "ccc-observed/1";
// how often a service writes what its fixtures ran - a process killed rather than stopped loses at most this much
const FLUSH_EVERY: Duration = Duration::from_secs(2);
// the commit the copy was made from - CCC_COMMIT overrides it
const COMMIT: &str = "";

// one test, named the way its runner selects it
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Fixture {
    pub name: String,
    pub file: String,
    pub language: String,
    pub kind: String,
}

type Site = (&'static str, &'static str, u32);

#[derive(Default)]
struct State {
    // what each fixture ran that is not written yet
    ran: HashMap<Arc<Fixture>, HashMap<Site, u64>>,
    // this process's own tests still running - each writes its record when it ends
    local: HashMap<Arc<Fixture>, usize>,
}

thread_local! {
    // the fixture this thread works for
    static BOUND: RefCell<Option<Arc<Fixture>>> = const { RefCell::new(None) };
}

fn dir() -> Option<&'static str> {
    static DIR: OnceLock<Option<String>> = OnceLock::new();
    DIR.get_or_init(|| std::env::var("CCC_OBSERVED_DIR").ok().filter(|d| !d.is_empty())).as_deref()
}

fn state() -> MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(Default::default).lock().unwrap_or_else(|p| p.into_inner())
}

// the fixture this work runs for - bound to this thread, or this process's one running test
pub fn current() -> Option<Arc<Fixture>> {
    dir()?;
    if let Some(f) = BOUND.try_with(|b| b.borrow().clone()).ok().flatten() {
        return Some(f);
    }
    let s = state();
    if s.local.len() == 1 {
        return s.local.keys().next().cloned();
    }
    None
}

// counts one run of a function for the fixture this work runs for
pub fn hit(file: &'static str, name: &'static str, line: u32) {
    if dir().is_none() {
        return;
    }
    tick();
    if let Some(f) = current() {
        *state().ran.entry(f).or_default().entry((file, name, line)).or_default() += 1;
    }
}

// makes this thread work for a fixture until it drops - `None` leaves whatever was bound
pub struct Bind {
    prev: Option<Option<Arc<Fixture>>>,
}

pub fn bind(f: Option<Arc<Fixture>>) -> Bind {
    match f {
        Some(f) => Bind { prev: BOUND.try_with(|b| b.replace(Some(f))).ok() },
        None => Bind { prev: None },
    }
}

impl Drop for Bind {
    fn drop(&mut self) {
        if let Some(prev) = self.prev.take() {
            let _ = BOUND.try_with(|b| b.replace(prev));
        }
    }
}

// a test as a fixture until it drops - its record is written then
pub struct Test {
    open: Option<(Arc<Fixture>, Instant, Bind)>,
}

pub fn fixture(name: &'static str, file: &'static str, kind: &'static str) -> Test {
    if dir().is_none() {
        return Test { open: None };
    }
    let f = Arc::new(Fixture { name: name.into(), file: file.into(), language: "rust".into(), kind: kind.into() });
    *state().local.entry(f.clone()).or_default() += 1;
    let bound = bind(Some(f.clone()));
    Test { open: Some((f, Instant::now(), bound)) }
}

impl Drop for Test {
    fn drop(&mut self) {
        let Some((f, start, bound)) = self.open.take() else { return };
        drop(bound);
        let sites = {
            let mut s = state();
            if let Some(n) = s.local.get_mut(&f) {
                *n -= 1;
                if *n == 0 {
                    s.local.remove(&f);
                }
            }
            s.ran.remove(&f).unwrap_or_default()
        };
        write(&f, &sites, Some(start.elapsed().as_millis() as u64));
    }
}

// a future that works for a fixture on every poll, whichever thread polls it
pub struct Bound<F> {
    fixture: Option<Arc<Fixture>>,
    inner: Pin<Box<F>>,
}

pub fn bound<F: Future>(fixture: Option<Arc<Fixture>>, inner: F) -> Bound<F> {
    Bound { fixture, inner: Box::pin(inner) }
}

impl<F: Future> Future for Bound<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let this = self.get_mut();
        let _bound = bind(this.fixture.clone());
        this.inner.as_mut().poll(cx)
    }
}

// a spawned future carries the fixture of whoever spawned it
pub fn bind_current<F: Future>(inner: F) -> Bound<F> {
    bound(current(), inner)
}

// a spawned closure carries the fixture of whoever spawned it
pub fn bind_current_fn<F: FnOnce() -> R, R>(f: F) -> impl FnOnce() -> R {
    let fixture = current();
    move || {
        let _bound = bind(fixture);
        f()
    }
}

// the fixture this work runs for as w3c baggage - the same four members every ccc probe sends
pub fn baggage() -> Option<String> {
    current().map(|f| to_baggage(&f))
}

pub fn to_baggage(f: &Fixture) -> String {
    format!(
        "ccc.fixture={},ccc.file={},ccc.lang={},ccc.kind={}",
        encode(&f.name),
        encode(&f.file),
        encode(&f.language),
        encode(&f.kind)
    )
}

// the fixture a baggage header names, among whatever other members it carries
pub fn from_baggage(header: Option<&str>) -> Option<Arc<Fixture>> {
    let mut f = Fixture { name: String::new(), file: String::new(), language: String::new(), kind: String::new() };
    for member in header?.split(',') {
        let pair = member.split(';').next().unwrap_or_default();
        let Some((k, v)) = pair.split_once('=') else { continue };
        let v = decode(v.trim());
        match k.trim() {
            "ccc.fixture" => f.name = v,
            "ccc.file" => f.file = v,
            "ccc.lang" => f.language = v,
            "ccc.kind" => f.kind = v,
            _ => {}
        }
    }
    (!f.name.is_empty() && !f.kind.is_empty()).then(|| Arc::new(f))
}

// `t` with the baggage header set by `set`, when this work runs for a fixture - a request builder on its way out
pub fn with_baggage<T>(t: T, set: impl FnOnce(T, String) -> T) -> T {
    match baggage() {
        Some(b) => set(t, b),
        None => t,
    }
}

// writes what fixtures ran in this process when the guard drops - put atop main
pub struct Main;

pub fn main_guard() -> Main {
    Main
}

impl Drop for Main {
    fn drop(&mut self) {
        flush();
    }
}

// writes what fixtures ran in this process that no test of its own is still running
pub fn flush() {
    if dir().is_none() {
        return;
    }
    let pending: Vec<(Arc<Fixture>, HashMap<Site, u64>)> = {
        let mut s = state();
        let done: Vec<Arc<Fixture>> = s.ran.keys().filter(|f| !s.local.contains_key(*f)).cloned().collect();
        done.into_iter().filter_map(|f| s.ran.remove(&f).map(|sites| (f, sites))).collect()
    };
    for (f, sites) in pending {
        write(&f, &sites, None);
    }
}

fn tick() {
    static TICKING: Once = Once::new();
    TICKING.call_once(|| {
        let _ = std::thread::Builder::new().name("ccc-probe-flush".into()).spawn(|| loop {
            std::thread::sleep(FLUSH_EVERY);
            flush();
        });
    });
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(hi * 16 + lo);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

fn json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn service() -> String {
    if let Ok(s) = std::env::var("CCC_SERVICE") {
        if !s.is_empty() {
            return s;
        }
    }
    let exe = std::env::current_exe().ok().and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned())).unwrap_or_else(|| "rust".into());
    // a test binary is `<name>-<16 hex digits>`
    match exe.rsplit_once('-') {
        Some((name, hash)) if hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => name.to_string(),
        _ => exe,
    }
}

// one ccc-observed/1 line - a fixture, and the functions it ran in this process with how many times each ran
fn write(f: &Fixture, sites: &HashMap<Site, u64>, ms: Option<u64>) {
    let Some(dir) = dir() else { return };
    if sites.is_empty() && ms.is_none() {
        return;
    }
    let mut files: std::collections::BTreeMap<&str, std::collections::BTreeMap<(u32, &str), u64>> = Default::default();
    for (&(file, name, line), &calls) in sites {
        *files.entry(file).or_default().entry((line, name)).or_default() += calls;
    }
    let files: Vec<String> = files
        .iter()
        .map(|(path, fns)| {
            let fns: Vec<String> = fns.iter().map(|((line, name), calls)| format!("{{\"name\":{},\"line\":{line},\"calls\":{calls}}}", json(name))).collect();
            format!("{{\"path\":{},\"functions\":[{}]}}", json(path), fns.join(","))
        })
        .collect();
    let commit = std::env::var("CCC_COMMIT").ok().filter(|c| !c.is_empty()).unwrap_or_else(|| COMMIT.to_string());
    let commit = if commit.is_empty() { String::new() } else { format!(",\"commit\":{}", json(&commit)) };
    let run = std::env::var("CCC_RUN").ok().filter(|r| !r.is_empty()).unwrap_or_else(|| "local".into());
    let duration = ms.map(|ms| format!(",\"duration_ms\":{ms}")).unwrap_or_default();
    let line = format!(
        "{{\"schema\":{},\"fixture\":{{\"name\":{},\"kind\":{},\"file\":{},\"language\":{}}},\"run\":{},\"service\":{{\"name\":{}{commit}}},\"files\":[{}]{duration}}}\n",
        json(SCHEMA),
        json(&f.name),
        json(&f.kind),
        json(&f.file),
        json(&f.language),
        json(&run),
        json(&service()),
        files.join(",")
    );
    static WRITING: Mutex<()> = Mutex::new(());
    let _writing = WRITING.lock().unwrap_or_else(|p| p.into_inner());
    let _ = std::fs::create_dir_all(dir);
    let path = std::path::Path::new(dir).join(format!("{}-{}.jsonl", service(), std::process::id()));
    if let Ok(mut out) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = out.write_all(line.as_bytes());
    }
}

// a tower layer that serves each request for the fixture its baggage names - axum, tonic and anything else built on tower
#[cfg(feature = "tower")]
pub mod tower {
    use super::{bind, bound, from_baggage, Bound};
    use std::task::{Context, Poll};

    #[derive(Clone, Copy, Debug, Default)]
    pub struct Layer;

    impl<S> tower_layer::Layer<S> for Layer {
        type Service = Service<S>;

        fn layer(&self, inner: S) -> Service<S> {
            Service { inner }
        }
    }

    #[derive(Clone, Debug)]
    pub struct Service<S> {
        inner: S,
    }

    impl<S, B> tower_service::Service<http::Request<B>> for Service<S>
    where
        S: tower_service::Service<http::Request<B>>,
    {
        type Response = S::Response;
        type Error = S::Error;
        type Future = Bound<S::Future>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
            self.inner.poll_ready(cx)
        }

        fn call(&mut self, req: http::Request<B>) -> Self::Future {
            let f = from_baggage(req.headers().get("baggage").and_then(|v| v.to_str().ok()));
            let inner = {
                let _bound = bind(f.clone());
                self.inner.call(req)
            };
            bound(f, inner)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baggage_round_trips_through_the_encoding_every_probe_shares() {
        let f = Fixture { name: "it(\"a b\")".into(), file: "src/a,b.rs".into(), language: "rust".into(), kind: "unit".into() };
        let back = from_baggage(Some(&format!("other=1,{};p=1", to_baggage(&f)))).unwrap();
        assert_eq!(*back, f);
        // go's query escaping writes a space as +
        assert_eq!(from_baggage(Some("ccc.fixture=a+b,ccc.kind=unit")).unwrap().name, "a b");
        assert!(from_baggage(Some("ccc.fixture=a")).is_none());
    }
}
