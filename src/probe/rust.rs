// rust - a `::ccc_probe::hit` atop every function, a fixture guard atop every test, spawned work and outgoing requests carrying the
// fixture, a tower layer on axum and tonic servers, and the runtime crate added to every probed package as a path dependency

use super::{collect, dir_of, mapped, named_children, quote, relative, Cx, Prober, Report, Rewrite, Source, RUNTIME_DIR};
use anyhow::{bail, Result};
use tree_sitter::{Node, Parser};

const RUNTIME: &str = include_str!("../../probes/rust/lib.rs");
const MANIFEST: &str = include_str!("../../probes/rust/Cargo.toml");
// the attributes that make a function a test, by their last path segment - `#[test]`, `#[tokio::test]`, `#[rstest]`
const TEST_ATTRS: &[&str] = &["test", "rstest", "test_case", "quickcheck", "wasm_bindgen_test"];
// spawns whose first argument is a future, then those whose first argument is a closure
const SPAWN_FUTURE: &[&str] = &["tokio::spawn", "tokio::task::spawn", "tokio::task::spawn_local", "task::spawn", "task::spawn_local", "async_std::task::spawn", "actix_rt::spawn", "actix_web::rt::spawn"];
const SPAWN_FN: &[&str] = &["std::thread::spawn", "thread::spawn", "tokio::task::spawn_blocking", "task::spawn_blocking", "rayon::spawn"];
const LAYER: &str = "::ccc_probe::tower::Layer";

struct Package {
    // its directory inside the copy - empty at the top
    dir: String,
    edition: String,
    // the tower layer builds against http 1 - so axum 0.7 and tonic 0.12 on
    axum: bool,
    tonic: bool,
    actix: bool,
    // why the package is left as it was
    skip: Option<&'static str>,
    probed: bool,
    tower_used: bool,
}

#[derive(Default)]
pub(crate) struct Rust {
    packages: Vec<Package>,
}

impl Rust {
    fn package_of(&self, rel: &str) -> Option<usize> {
        (0..self.packages.len())
            .filter(|&i| {
                let dir = &self.packages[i].dir;
                dir.is_empty() || rel.starts_with(&format!("{dir}/"))
            })
            .max_by_key(|&i| self.packages[i].dir.len())
    }
}

impl Prober for Rust {
    fn prepare(&mut self, cx: &Cx) -> Result<()> {
        let root = cx.read("Cargo.toml").unwrap_or_default();
        for rel in &cx.files {
            if !(rel == "Cargo.toml" || rel.ends_with("/Cargo.toml")) || !mapped(rel, 0) {
                continue;
            }
            let text = cx.read(rel).unwrap_or_default();
            if !text.lines().any(|l| l.trim() == "[package]") {
                continue;
            }
            let dir = dir_of(rel).to_string();
            let entry = |name: &str| cx.read(&join(&dir, name)).unwrap_or_default();
            let skip = if text.lines().any(|l| matches!(l.replace(' ', "").as_str(), "proc-macro=true" | "proc_macro=true")) {
                Some("a proc-macro crate runs inside the compiler, not the tests")
            } else if entry("src/lib.rs").contains("#![no_std]") || entry("src/main.rs").contains("#![no_std]") {
                Some("a no_std crate has no std for the runtime to record through")
            } else {
                None
            };
            let modern = |name: &str, floor: u32| dep_version(&text, name).or_else(|| dep_version(&root, name)).map_or(true, |v| at_least(&v, floor));
            let (axum, tonic) = (has_dep(&text, "axum"), has_dep(&text, "tonic"));
            self.packages.push(Package {
                edition: edition(&text, &root),
                axum: axum && modern("axum", 7),
                tonic: tonic && modern("tonic", 12),
                actix: has_dep(&text, "actix-web"),
                dir,
                skip,
                probed: false,
                tower_used: false,
            });
        }
        if self.packages.is_empty() {
            bail!("no Cargo.toml with a [package] in {} - `ccc probe rust` instruments cargo packages", cx.out.display());
        }
        Ok(())
    }

    fn rewrite(&mut self, _cx: &Cx, file: &Source) -> Result<Rewrite> {
        let Some(p) = self.package_of(&file.rel) else { bail!("no Cargo.toml above it") };
        let mut rw = Rewrite::default();
        let pkg = &self.packages[p];
        if pkg.skip.is_some() {
            return Ok(rw);
        }
        let inside = file.rel.strip_prefix(&format!("{}/", pkg.dir)).unwrap_or(&file.rel);
        if inside == "build.rs" {
            return Ok(rw);
        }
        // integration tests and benches are test code - their tests are fixtures and nothing else in them counts
        let test_file = inside.starts_with("tests/") || inside.starts_with("benches/");
        for def in file.defs() {
            let Some(body) = def.node.child_by_field_name("body") else { continue };
            let at = body.start_byte() + 1;
            if is_test(file, def.node) {
                rw.edits.insert(at, format!(" let __ccc_test = ::ccc_probe::fixture({}, {}, {});", quote(&def.func.name), quote(&file.path), quote(file.kind)));
                rw.fixtures += 1;
                continue;
            }
            if test_file || def.func.test_ctx || is_const(file, def.node) {
                continue;
            }
            rw.edits.insert(at, format!(" ::ccc_probe::hit({}, {}, {});", quote(&file.path), quote(&def.qualified("::")), def.func.start_line));
            rw.functions += 1;
            if def.func.name == "main" && def.func.owner.is_none() && def.node.parent().is_some_and(|p| p.kind() == "source_file") {
                rw.edits.insert(at, " let __ccc_main = ::ccc_probe::main_guard();");
            }
        }
        let mut tower_used = false;
        collect(file.root(), &mut |n| {
            if n.kind() == "call_expression" {
                if let Some(tower) = wrap_call(file, n, pkg, &mut rw) {
                    rw.wrapped += 1;
                    tower_used |= tower;
                }
            }
        });
        if !rw.edits.is_empty() {
            let pkg = &mut self.packages[p];
            pkg.probed = true;
            pkg.tower_used |= tower_used;
        }
        Ok(rw)
    }

    fn finish(&mut self, cx: &Cx, report: &mut Report) -> Result<()> {
        let runtime_dir = format!("{RUNTIME_DIR}/rust");
        cx.write(&format!("{runtime_dir}/Cargo.toml"), MANIFEST)?;
        let commit = cx.commit.as_deref().unwrap_or_default();
        cx.write(&format!("{runtime_dir}/lib.rs"), &RUNTIME.replacen("const COMMIT: &str = \"\";", &format!("const COMMIT: &str = {};", quote(commit)), 1))?;
        report.runtime.push(format!("{runtime_dir}/ - crate ccc_probe"));
        for pkg in &self.packages {
            if let Some(why) = pkg.skip {
                report.skipped.push(format!("{} - {why}", join(&pkg.dir, "Cargo.toml")));
            }
        }
        for pkg in self.packages.iter().filter(|p| p.probed) {
            let manifest = join(&pkg.dir, "Cargo.toml");
            let path = relative(&pkg.dir, &runtime_dir);
            let mut text = cx.read(&manifest).unwrap_or_default();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            let features = if pkg.tower_used { "\nfeatures = [\"tower\"]" } else { "" };
            text.push_str(&format!("\n# ccc probe - the runtime the instrumented copy reports through\n[dependencies.ccc_probe]\npath = {}{features}\n", quote(&path)));
            cx.write(&manifest, &text)?;
            report.edited.push(format!("{manifest} - depends on ccc_probe at {path}"));
            // a 2015 crate names another crate only once its root declares it
            if pkg.edition == "2015" {
                for root in crate_roots(cx, &pkg.dir) {
                    if let Some(text) = cx.read(&root) {
                        cx.write(&root, &declare_extern(&text))?;
                    }
                }
            }
        }
        report.run.push("cargo test   # --test-threads=1 attributes work a test hands to other threads too".to_string());
        Ok(())
    }
}

// the call a spawn, a client or a server makes, wrapped to carry the fixture - `Some(true)` when the wrap needs the tower layer
fn wrap_call(file: &Source, call: Node, pkg: &Package, rw: &mut Rewrite) -> Option<bool> {
    let function = call.child_by_field_name("function")?;
    let args = call.child_by_field_name("arguments")?;
    let callee = file.text_of(function).replace(char::is_whitespace, "");
    let list = named_children(args);
    if SPAWN_FUTURE.contains(&callee.as_str()) || SPAWN_FN.contains(&callee.as_str()) {
        let wrap = if SPAWN_FN.contains(&callee.as_str()) { "::ccc_probe::bind_current_fn(" } else { "::ccc_probe::bind_current(" };
        rw.edits.wrap(*list.first()?, wrap, ")");
        return Some(false);
    }
    // a reqwest request builder sent with the baggage header on
    if function.kind() == "field_expression" && list.is_empty() && file.text.contains("reqwest") {
        let field = function.child_by_field_name("field").map(|f| file.text_of(f));
        if field == Some("send") {
            let receiver = function.child_by_field_name("value")?;
            rw.edits.wrap(receiver, "::ccc_probe::with_baggage(", ", |__b, __v| __b.header(\"baggage\", __v))");
            return Some(false);
        }
    }
    let imports = |module: &str, name: &str| file.imports.iter().any(|i| i.module.starts_with(module) && i.names.iter().any(|n| n == name));
    // a tonic request with the baggage in its metadata
    if pkg.tonic && (callee == "tonic::Request::new" || (callee == "Request::new" && imports("tonic", "Request"))) {
        rw.edits.wrap(
            call,
            "{ let mut __ccc_r = ",
            "; if let Some(__v) = ::ccc_probe::baggage().and_then(|b| b.parse().ok()) { __ccc_r.metadata_mut().insert(\"baggage\", __v); } __ccc_r }",
        );
        return Some(false);
    }
    if pkg.tonic && list.is_empty() && callee.ends_with("Server::builder") && file.text.contains("tonic") {
        rw.edits.close(call.end_byte(), format!(".layer({LAYER})"));
        return Some(true);
    }
    if pkg.axum && (callee == "axum::serve" || (callee == "serve" && imports("axum", "serve"))) {
        let app = *list.get(1)?;
        // a router turned into a make-service takes the layer just before it turns
        let made = (app.kind() == "call_expression")
            .then(|| app.child_by_field_name("function"))
            .flatten()
            .filter(|f| f.kind() == "field_expression" && f.child_by_field_name("field").is_some_and(|n| file.text_of(n).starts_with("into_make_service")));
        match made.and_then(|f| f.child_by_field_name("value")) {
            Some(router) => rw.edits.close(router.end_byte(), format!(".layer({LAYER})")),
            None => rw.edits.wrap(app, "(", format!(").layer({LAYER})")),
        }
        return Some(true);
    }
    if pkg.actix && list.is_empty() && (callee == "App::new" || callee == "actix_web::App::new") {
        rw.edits.close(
            call.end_byte(),
            ".wrap_fn(|__req, __srv| { let __f = ::ccc_probe::from_baggage(__req.headers().get(\"baggage\").and_then(|v| v.to_str().ok())); ::ccc_probe::bound(__f, ::actix_web::dev::Service::call(__srv, __req)) })",
        );
        return Some(false);
    }
    None
}

// `#[test]`, `#[tokio::test(..)]`, `#[rstest]` above the function
fn is_test(file: &Source, func: Node) -> bool {
    let mut at = func.prev_sibling();
    while let Some(node) = at {
        match node.kind() {
            "attribute_item" => {
                let path = node.named_child(0).and_then(|a| a.named_child(0)).map(|p| file.text_of(p)).unwrap_or_default();
                let last = path.rsplit("::").next().unwrap_or_default();
                if TEST_ATTRS.contains(&last) {
                    return true;
                }
            }
            "line_comment" | "block_comment" => {}
            _ => return false,
        }
        at = node.prev_sibling();
    }
    false
}

// a const fn cannot call the runtime
fn is_const(file: &Source, func: Node) -> bool {
    named_children(func).iter().any(|c| c.kind() == "function_modifiers" && file.text_of(*c).split_whitespace().any(|w| w == "const"))
}

// the files cargo compiles as crate roots
fn crate_roots(cx: &Cx, dir: &str) -> Vec<String> {
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    cx.files
        .iter()
        .filter(|rel| {
            let Some(inside) = rel.strip_prefix(&prefix) else { return false };
            matches!(inside, "src/lib.rs" | "src/main.rs")
                || ["src/bin/", "tests/", "examples/", "benches/"].iter().any(|d| inside.strip_prefix(d).is_some_and(|f| f.ends_with(".rs") && !f.contains('/')))
        })
        .cloned()
        .collect()
}

// `extern crate ccc_probe;` after a crate root's inner attributes, on a line it shares
fn declare_extern(text: &str) -> String {
    let mut parser = Parser::new();
    if parser.set_language(&crate::languages::Language::Rust.ts_language()).is_err() {
        return text.to_string();
    }
    let Some(tree) = parser.parse(text, None) else { return text.to_string() };
    let at = named_children(tree.root_node())
        .into_iter()
        .find(|n| !matches!(n.kind(), "inner_attribute_item" | "line_comment" | "block_comment"))
        .map_or(text.len(), |n| n.start_byte());
    format!("{}extern crate ccc_probe; {}", &text[..at], &text[at..])
}

// a dependency's version as a manifest names it - inline, in a table or in its own `[dependencies.name]` section
fn dep_version(manifest: &str, name: &str) -> Option<String> {
    let mut in_table = false;
    for line in manifest.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            in_table = l.trim_matches(['[', ']']).rsplit('.').next() == Some(name) && l.contains("dependencies");
            continue;
        }
        if in_table {
            if let Some(v) = l.strip_prefix("version").map(str::trim).and_then(|v| v.strip_prefix('=')) {
                return Some(v.trim().trim_matches('"').to_string());
            }
            continue;
        }
        let Some(rest) = l.strip_prefix(name) else { continue };
        let rest = rest.trim_start();
        let Some(value) = rest.strip_prefix('=') else { continue };
        let value = value.trim();
        if let Some(v) = value.strip_prefix('"') {
            return v.split('"').next().map(str::to_string);
        }
        if let Some(at) = value.find("version") {
            return value[at..].split('"').nth(1).map(str::to_string);
        }
    }
    None
}

fn has_dep(manifest: &str, name: &str) -> bool {
    manifest.lines().any(|l| {
        let l = l.trim();
        l.strip_prefix(name).is_some_and(|r| r.trim_start().starts_with('=') || r.starts_with('.'))
            || (l.starts_with('[') && l.contains("dependencies") && l.trim_matches(['[', ']']).rsplit('.').next() == Some(name))
    })
}

// does a `0.x` or `x.y` requirement allow the release the layer needs - `^0.7`, `0.8.1`, `1`
fn at_least(version: &str, minor_floor: u32) -> bool {
    let v = version.trim_start_matches(['^', '~', '=', '>', ' ']);
    let mut parts = v.split('.').map(|p| p.parse::<u32>().ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(0), Some(minor)) => minor >= minor_floor,
        (Some(major), _) => major >= 1,
        _ => true,
    }
}

// the edition a package compiles as - its own, its workspace's, or 2015 when it names none
fn edition(manifest: &str, root: &str) -> String {
    let find = |text: &str| {
        text.lines().find_map(|l| {
            let l = l.trim();
            let v = l.strip_prefix("edition")?.trim_start().strip_prefix('=')?.trim();
            v.strip_prefix('"').and_then(|v| v.split('"').next()).map(str::to_string)
        })
    };
    if manifest.lines().any(|l| l.replace(' ', "").starts_with("edition.workspace=true") || l.replace(' ', "").starts_with("edition={workspace=true")) {
        return find(root).unwrap_or_else(|| "2015".to_string());
    }
    find(manifest).unwrap_or_else(|| "2015".to_string())
}

fn join(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        rel.to_string()
    } else {
        format!("{dir}/{rel}")
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Cx, Prober, Source, Target};
    use super::*;
    use crate::extract;
    use crate::languages::Language;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn package(axum: bool, tonic: bool) -> Package {
        Package { dir: String::new(), edition: "2021".into(), axum, tonic, actix: true, skip: None, probed: false, tower_used: false }
    }

    fn rewrite(rel: &str, src: &str, pkg: Package) -> String {
        let cx = Cx { target: Target::Rust, out: PathBuf::new(), prefix: String::new(), commit: None, files: BTreeSet::new(), rules: Vec::new() };
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&Language::Rust.ts_language()).unwrap();
        let ex = extract::extract(Language::Rust, src).unwrap();
        let file = Source {
            rel: rel.into(),
            path: rel.into(),
            lang: Language::Rust,
            text: src.into(),
            tree: parser.parse(src, None).unwrap(),
            funcs: ex.funcs,
            imports: ex.imports,
            test: false,
            kind: cx.kind(rel),
        };
        Rust { packages: vec![pkg] }.rewrite(&cx, &file).unwrap().edits.apply(src)
    }

    #[test]
    fn every_function_counts_its_runs_and_every_test_is_a_fixture() {
        let src = "pub struct Store;\nimpl Store {\n    pub const fn new() -> Self { Store }\n    pub fn put(&self) -> u8 {\n        1\n    }\n}\nfn main() {}\n#[cfg(test)]\nmod tests {\n    fn helper() {}\n    #[tokio::test(flavor = \"multi_thread\")]\n    async fn puts() {\n        helper();\n    }\n}\n";
        let out = rewrite("src/main.rs", src, package(false, false));
        assert!(out.contains("pub const fn new() -> Self { Store }"), "a const fn cannot call the runtime: {out}");
        assert!(out.contains("pub fn put(&self) -> u8 { ::ccc_probe::hit(\"src/main.rs\", \"Store::put\", 4);\n"), "{out}");
        assert!(out.contains("fn main() { ::ccc_probe::hit(\"src/main.rs\", \"main\", 8); let __ccc_main = ::ccc_probe::main_guard();}"), "{out}");
        assert!(out.contains("    fn helper() {}"), "a test helper counts nothing: {out}");
        assert!(out.contains("async fn puts() { let __ccc_test = ::ccc_probe::fixture(\"puts\", \"src/main.rs\", \"unit\");\n"), "{out}");
        assert_eq!(out.lines().count(), src.lines().count());
    }

    #[test]
    fn spawns_and_requests_carry_the_fixture() {
        let src = "use tonic::Request;\nfn go() {\n    tokio::spawn(async move { work().await });\n    std::thread::spawn(|| work());\n    let r = client.get(url).send();\n    let t = Request::new(Ping {});\n}\nfn reqwest() {}\n";
        let out = rewrite("src/lib.rs", src, package(false, true));
        assert!(out.contains("tokio::spawn(::ccc_probe::bind_current(async move { work().await }));"), "{out}");
        assert!(out.contains("std::thread::spawn(::ccc_probe::bind_current_fn(|| work()));"), "{out}");
        assert!(out.contains("::ccc_probe::with_baggage(client.get(url), |__b, __v| __b.header(\"baggage\", __v)).send();"), "{out}");
        assert!(out.contains("let t = { let mut __ccc_r = Request::new(Ping {}); if let Some(__v)"), "{out}");
    }

    #[test]
    fn servers_take_the_layer() {
        let src = "use axum::serve;\nasync fn run() {\n    axum::serve(l, app.into_make_service()).await;\n    serve(l, router(state)).await;\n    tonic::transport::Server::builder().add_service(s);\n    HttpServer::new(|| App::new().route(\"/\", h));\n}\n";
        let out = rewrite("src/server.rs", src, package(true, true));
        assert!(out.contains("axum::serve(l, app.layer(::ccc_probe::tower::Layer).into_make_service())"), "{out}");
        assert!(out.contains("serve(l, (router(state)).layer(::ccc_probe::tower::Layer))"), "{out}");
        assert!(out.contains("Server::builder().layer(::ccc_probe::tower::Layer).add_service(s)"), "{out}");
        assert!(out.contains("App::new().wrap_fn(|__req, __srv| {"), "{out}");
        // axum 0.6 and tonic before 0.12 speak http 0.2, which the layer does not
        let old = rewrite("src/server.rs", src, package(false, false));
        assert!(!old.contains("tower::Layer"), "{old}");
    }

    #[test]
    fn a_manifest_names_versions_and_editions_every_way_cargo_allows() {
        let m = "[package]\nname = \"a\"\n\n[dependencies]\naxum = \"0.6.20\"\ntonic = { version = \"0.12\", features = [\"tls\"] }\n\n[dependencies.actix-web]\nversion = \"4\"\n";
        assert_eq!(dep_version(m, "axum").as_deref(), Some("0.6.20"));
        assert_eq!(dep_version(m, "tonic").as_deref(), Some("0.12"));
        assert_eq!(dep_version(m, "actix-web").as_deref(), Some("4"));
        assert!(has_dep(m, "actix-web") && has_dep(m, "axum") && !has_dep(m, "reqwest"));
        assert!(!at_least("0.6.20", 7) && at_least("^0.7", 7) && at_least("0.12", 12) && at_least("1", 12));
        assert_eq!(edition(m, ""), "2015");
        assert_eq!(edition("edition = \"2021\"\n", ""), "2021");
        assert_eq!(edition("edition.workspace = true\n", "[workspace.package]\nedition = \"2024\"\n"), "2024");
    }

    #[test]
    fn a_2015_crate_root_declares_the_runtime_after_its_inner_attributes() {
        assert_eq!(declare_extern("#![allow(x)]\n// c\nmod a;\n"), "#![allow(x)]\n// c\nextern crate ccc_probe; mod a;\n");
    }
}
