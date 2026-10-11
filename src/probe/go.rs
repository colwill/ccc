// go - a `cccprobe` call atop every function and test, a wrap around every server's handler and client's transport, grpc options on every server and dial

use super::{child_of_kind, collect, dir_of, mapped, named_children, quote, relative, Cx, Prober, Report, Rewrite, Source, RUNTIME_DIR};
use anyhow::{bail, Result};
use tree_sitter::Node;

// the runtime's module path - `.invalid` so no proxy is ever asked for it, a replace directive always points it at the copy
const PROBE_MODULE: &str = "ccc.invalid/probe";
const GRPC_MODULE: &str = "google.golang.org/grpc";
const RUNTIME: &str = include_str!("../../probes/go/cccprobe.go");
const GRPC_GLUE: &str = include_str!("../../probes/go/cccprobegrpc/cccprobegrpc.go");

struct Module {
    // its directory inside the copy - empty at the top
    dir: String,
    path: String,
    grpc: bool,
    vendored: bool,
    probed: bool,
    grpc_probed: bool,
}

#[derive(Default)]
pub(crate) struct Go {
    modules: Vec<Module>,
}

// the local names this file imports the packages it wraps under
struct Names {
    http: Option<String>,
    httptest: Option<String>,
    grpc: Option<String>,
}

impl Go {
    // the module a file belongs to - the nearest go.mod above it
    fn module_of(&self, rel: &str) -> Option<usize> {
        (0..self.modules.len())
            .filter(|&i| {
                let dir = &self.modules[i].dir;
                dir.is_empty() || rel.starts_with(&format!("{dir}/"))
            })
            .max_by_key(|&i| self.modules[i].dir.len())
    }
}

impl Prober for Go {
    fn prepare(&mut self, cx: &Cx) -> Result<()> {
        for rel in &cx.files {
            if !(rel == "go.mod" || rel.ends_with("/go.mod")) || !mapped(rel, 0) || rel.split('/').any(|d| d == "testdata") {
                continue;
            }
            let text = cx.read(rel).unwrap_or_default();
            let Some(path) = module_path(&text) else { continue };
            let dir = dir_of(rel).to_string();
            let vendored = cx.files.contains(&join(&dir, "vendor/modules.txt"));
            self.modules.push(Module { grpc: requires(&text, GRPC_MODULE), dir, path, vendored, probed: false, grpc_probed: false });
        }
        if self.modules.is_empty() {
            bail!("no go.mod in {} - `ccc probe go` instruments go modules", cx.out.display());
        }
        Ok(())
    }

    fn rewrite(&mut self, _cx: &Cx, file: &Source) -> Result<Rewrite> {
        let Some(m) = self.module_of(&file.rel) else { bail!("no go.mod above it") };
        let root = file.root();
        let Some(package) = child_of_kind(root, "package_clause") else { bail!("no package clause") };
        let package_name = child_of_kind(package, "package_identifier").map(|n| file.text_of(n)).unwrap_or_default();
        let test_file = file.rel.ends_with("_test.go");
        let mut rw = Rewrite::default();

        for def in file.defs() {
            let Some(body) = def.node.child_by_field_name("body") else { continue };
            let at = body.start_byte() + 1;
            let method = def.node.kind() == "method_declaration";
            if test_file {
                // a TestXxx(t *testing.T) is a fixture - every other function in a test file is a helper, and helpers count nothing
                if let (false, true, Some(t)) = (method, is_test_name(&def.func.name), test_param(file, def.node)) {
                    rw.edits.insert(at, format!(" cccprobe.Test({t}, {}, {});", quote(&file.path), quote(file.kind)));
                    rw.fixtures += 1;
                }
                continue;
            }
            // init runs before any test or request could have bound a fixture
            if def.func.name == "init" && !method {
                continue;
            }
            rw.edits.insert(at, format!(" cccprobe.Hit({}, {}, {});", quote(&file.path), quote(&def.qualified(".")), def.func.start_line));
            rw.functions += 1;
            if def.func.name == "main" && !method && package_name == "main" {
                rw.edits.insert(at, " defer cccprobe.Flush();");
            }
        }

        let names = Names {
            http: file.import_name("net/http"),
            httptest: file.import_name("net/http/httptest"),
            grpc: self.modules[m].grpc.then(|| file.import_name(GRPC_MODULE)).flatten(),
        };
        let mut grpc_probed = false;
        collect(root, &mut |n| match n.kind() {
            "call_expression" => {
                if let Some(wrapped) = wrap_call(file, n, &names, &mut rw) {
                    rw.wrapped += 1;
                    grpc_probed |= wrapped;
                }
            }
            "composite_literal" => rw.wrapped += wrap_literal(file, n, &names, &mut rw),
            _ => {}
        });
        if rw.edits.is_empty() {
            return Ok(rw);
        }
        let module = &mut self.modules[m];
        // on the package line itself, so every line keeps the number it had
        let mut imports = format!("; import cccprobe {}", quote(PROBE_MODULE));
        if grpc_probed {
            imports.push_str(&format!("; import cccprobegrpc {}", quote(&format!("{}/internal/cccprobegrpc", module.path))));
            module.grpc_probed = true;
        }
        rw.edits.insert(package.end_byte(), imports);
        module.probed = true;
        Ok(rw)
    }

    fn finish(&mut self, cx: &Cx, report: &mut Report) -> Result<()> {
        let runtime_dir = format!("{RUNTIME_DIR}/go");
        cx.write(&format!("{runtime_dir}/go.mod"), &format!("module {PROBE_MODULE}\n\ngo 1.18\n"))?;
        cx.write(&format!("{runtime_dir}/cccprobe.go"), RUNTIME)?;
        cx.write(&format!("{runtime_dir}/commit.go"), &commit_file(cx.commit.as_deref()))?;
        report.runtime.push(format!("{runtime_dir}/ - module {PROBE_MODULE}"));

        for module in self.modules.iter().filter(|m| m.probed) {
            let gomod = join(&module.dir, "go.mod");
            let to = local(relative(&module.dir, &runtime_dir));
            let mut text = cx.read(&gomod).unwrap_or_default();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!("\n// ccc probe - the runtime the instrumented copy reports through\nrequire {PROBE_MODULE} v0.0.0\n\nreplace {PROBE_MODULE} => {to}\n"));
            cx.write(&gomod, &text)?;
            report.edited.push(format!("{gomod} - requires {PROBE_MODULE}, replaced by {to}"));
            if module.vendored {
                vendor(cx, module, &to)?;
                report.edited.push(format!("{} - the runtime vendored beside the rest", join(&module.dir, "vendor/modules.txt")));
            }
            if module.grpc_probed {
                let glue = join(&module.dir, "internal/cccprobegrpc/cccprobegrpc.go");
                cx.write(&glue, GRPC_GLUE)?;
                report.runtime.push(format!("{glue} - grpc interceptors for {}", module.path));
            }
        }
        // a workspace's replaces win over its modules' own
        for rel in cx.files.iter().filter(|r| *r == "go.work" || r.ends_with("/go.work")) {
            let mut text = cx.read(rel).unwrap_or_default();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            let to = local(relative(dir_of(rel), &runtime_dir));
            text.push_str(&format!("\nreplace {PROBE_MODULE} => {to}\n"));
            cx.write(rel, &text)?;
            report.edited.push(format!("{rel} - replaces {PROBE_MODULE} with {to}"));
        }
        report.run.push("go test -count=1 ./...".to_string());
        Ok(())
    }
}

// a vendored module builds only from vendor/, so the runtime goes there too and modules.txt names it
fn vendor(cx: &Cx, module: &Module, to: &str) -> Result<()> {
    let vendored = join(&module.dir, &format!("vendor/{PROBE_MODULE}"));
    cx.write(&format!("{vendored}/cccprobe.go"), RUNTIME)?;
    cx.write(&format!("{vendored}/commit.go"), &commit_file(cx.commit.as_deref()))?;
    let list = join(&module.dir, "vendor/modules.txt");
    let mut text = cx.read(&list).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("# {PROBE_MODULE} v0.0.0 => {to}\n## explicit; go 1.18\n{PROBE_MODULE}\n# {PROBE_MODULE} => {to}\n"));
    cx.write(&list, &text)
}

fn commit_file(commit: Option<&str>) -> String {
    format!("package cccprobe\n\n// the commit the copy was made from - CCC_COMMIT overrides it\nconst commit = {}\n", quote(commit.unwrap_or_default()))
}

// the server or client a call makes, wrapped - `Some(true)` when the wrap is grpc's
fn wrap_call(file: &Source, call: Node, names: &Names, rw: &mut Rewrite) -> Option<bool> {
    let function = call.child_by_field_name("function")?;
    let args = call.child_by_field_name("arguments")?;
    if function.kind() != "selector_expression" {
        return None;
    }
    let package = file.text_of(function.child_by_field_name("operand")?);
    let name = file.text_of(function.child_by_field_name("field")?);
    let list = named_children(args);
    let is = |local: &Option<String>| local.as_deref() == Some(package);
    // the handler each server serves, by its place among the arguments
    let handler = match name {
        "NewServer" | "NewTLSServer" | "NewUnstartedServer" if is(&names.httptest) => Some(0),
        "ListenAndServe" | "Serve" | "ServeTLS" if is(&names.http) => Some(1),
        "ListenAndServeTLS" if is(&names.http) => Some(3),
        _ => None,
    };
    if let Some(at) = handler {
        let arg = *list.get(at)?;
        rw.edits.wrap(arg, "cccprobe.Handler(", ")");
        return Some(false);
    }
    if !is(&names.grpc) {
        return None;
    }
    let (first, options, ty) = match name {
        "NewServer" => (0, "cccprobegrpc.UnaryServer(), cccprobegrpc.StreamServer()", "ServerOption"),
        "Dial" | "NewClient" => (1, "cccprobegrpc.UnaryClient(), cccprobegrpc.StreamClient()", "DialOption"),
        "DialContext" => (2, "cccprobegrpc.UnaryClient(), cccprobegrpc.StreamClient()", "DialOption"),
        _ => return None,
    };
    add_options(args, &list, first, options, &format!("{package}.{ty}"), rw)?;
    Some(true)
}

// grpc options put in ahead of the caller's own - through an append when the caller spreads a slice
fn add_options(args: Node, list: &[Node], first: usize, options: &str, ty: &str, rw: &mut Rewrite) -> Option<()> {
    let spread = list.iter().position(|a| a.kind() == "variadic_argument");
    match spread {
        None => match list.get(first) {
            Some(arg) => rw.edits.insert(arg.start_byte(), format!("{options}, ")),
            None if first == 0 => rw.edits.insert(args.start_byte() + 1, options.to_string()),
            None => rw.edits.close(list.get(first - 1)?.end_byte(), format!(", {options}")),
        },
        Some(at) if at >= first => {
            let spread = list[at];
            let start = list.get(first).copied().unwrap_or(spread);
            if at > first {
                rw.edits.insert(start.start_byte(), format!("append([]{ty}{{{options}, "));
                rw.edits.close(list[at - 1].end_byte(), "}");
            } else {
                rw.edits.insert(start.start_byte(), format!("append([]{ty}{{{options}}}, "));
            }
            rw.edits.close(spread.end_byte(), ")...");
        }
        Some(_) => return None,
    }
    Some(())
}

// an http.Server's handler or an http.Client's transport, wrapped - a server with none gets one that serves the default mux
fn wrap_literal(file: &Source, lit: Node, names: &Names, rw: &mut Rewrite) -> usize {
    let (Some(ty), Some(body)) = (lit.child_by_field_name("type"), lit.child_by_field_name("body")) else { return 0 };
    if ty.kind() != "qualified_type" || names.http.is_none() {
        return 0;
    }
    let package = ty.child_by_field_name("package").map(|n| file.text_of(n));
    let name = ty.child_by_field_name("name").map(|n| file.text_of(n)).unwrap_or_default();
    if package != names.http.as_deref() {
        return 0;
    }
    let (field, wrap) = match name {
        "Server" => ("Handler", "cccprobe.Handler("),
        "Client" => ("Transport", "cccprobe.Transport("),
        _ => return 0,
    };
    let elements = named_children(body);
    let keyed = elements.iter().all(|e| e.kind() == "keyed_element");
    for e in &elements {
        let key = e.child_by_field_name("key").map(|k| file.text_of(k).trim().to_string());
        if key.as_deref() == Some(field) {
            if let Some(value) = e.child_by_field_name("value") {
                rw.edits.wrap(value, wrap, ")");
                return 1;
            }
        }
    }
    // a client with no transport already sends through the default one, which the runtime wraps
    if name == "Server" && keyed {
        rw.edits.insert(body.start_byte() + 1, "Handler: cccprobe.Handler(nil), ");
        return 1;
    }
    0
}

// the *testing.T a test takes, by name
fn test_param(file: &Source, func: Node) -> Option<String> {
    let params = func.child_by_field_name("parameters")?;
    let list = named_children(params);
    let [param] = list.as_slice() else { return None };
    let ty = param.child_by_field_name("type")?;
    let name = param.child_by_field_name("name")?;
    let ty = file.text_of(ty).replace(' ', "");
    let name = file.text_of(name);
    (ty.ends_with(".T") && ty.starts_with('*') && name != "_").then(|| name.to_string())
}

// `Test`, `TestCharge`, `Test_charge` - go's own rule, the rest after `Test` must not start lowercase
fn is_test_name(name: &str) -> bool {
    name.strip_prefix("Test").is_some_and(|rest| !rest.starts_with(|c: char| c.is_lowercase()))
}

fn module_path(gomod: &str) -> Option<String> {
    gomod.lines().find_map(|l| l.trim().strip_prefix("module ").map(|m| m.trim().trim_matches('"').to_string()))
}

// does a go.mod require the module, in a block or on its own line
fn requires(gomod: &str, module: &str) -> bool {
    gomod.lines().any(|l| {
        let l = l.trim();
        let l = l.strip_prefix("require ").unwrap_or(l).trim();
        l.split_whitespace().next() == Some(module)
    })
}

// go reads a replace target as a directory only when it starts `./` or `../`
fn local(path: String) -> String {
    if path.starts_with("./") || path.starts_with("../") || path == ".." {
        path
    } else {
        format!("./{path}")
    }
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

    fn cx() -> Cx {
        Cx { target: Target::Go, out: PathBuf::new(), prefix: String::new(), commit: None, files: BTreeSet::new(), rules: Vec::new() }
    }

    fn rewrite(rel: &str, src: &str, grpc: bool) -> String {
        let cx = cx();
        let mut go = Go { modules: vec![Module { dir: String::new(), path: "example.com/shop".into(), grpc, vendored: false, probed: false, grpc_probed: false }] };
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&Language::Go.ts_language()).unwrap();
        let ex = extract::extract(Language::Go, src).unwrap();
        let file = Source {
            rel: rel.into(),
            path: rel.into(),
            lang: Language::Go,
            text: src.into(),
            tree: parser.parse(src, None).unwrap(),
            funcs: ex.funcs,
            imports: ex.imports,
            test: rel.ends_with("_test.go"),
            kind: cx.kind(rel),
        };
        let rw = go.rewrite(&cx, &file).unwrap();
        rw.edits.apply(src)
    }

    #[test]
    fn every_function_counts_its_runs_on_the_line_it_started_on() {
        let src = "package shop // the shop\n\nimport \"fmt\"\n\nfunc (s *Store[T]) Put(id string) error {\n\treturn nil\n}\n\nfunc init() { fmt.Println() }\n\nfunc main() {}\n";
        let out = rewrite("store.go", src, false);
        assert!(out.starts_with("package shop; import cccprobe \"ccc.invalid/probe\" // the shop\n"), "{out}");
        // the owner is spelled the way the map spells it, so a record ties back by owner as well as name
        assert!(out.contains("func (s *Store[T]) Put(id string) error { cccprobe.Hit(\"store.go\", \"Store[T].Put\", 5);\n"), "{out}");
        assert!(out.contains("func init() { fmt.Println() }"), "init counts nothing: {out}");
        // main is only a program's entry in package main
        assert!(!out.contains("Flush"), "{out}");
        assert_eq!(out.lines().count(), src.lines().count(), "every line keeps its number");
        let main = rewrite("cmd/shop/main.go", "package main\n\nfunc main() {\n}\n", false);
        assert!(main.contains("func main() { cccprobe.Hit(\"cmd/shop/main.go\", \"main\", 3); defer cccprobe.Flush();\n"), "{main}");
    }

    #[test]
    fn a_test_names_itself_and_its_helpers_count_nothing() {
        let src = "package shop\n\nimport \"testing\"\n\nfunc TestPut(t *testing.T) {\n\thelper()\n}\n\nfunc helper() {}\n\nfunc Testify() {}\n";
        let out = rewrite("integration/put_test.go", src, false);
        assert!(out.contains("func TestPut(t *testing.T) { cccprobe.Test(t, \"integration/put_test.go\", \"integration\");\n"), "{out}");
        assert!(out.contains("func helper() {}"), "{out}");
        assert!(out.contains("func Testify() {}"), "{out}");
    }

    #[test]
    fn servers_and_clients_carry_the_fixture() {
        let src = r#"package shop

import (
	web "net/http"
	"net/http/httptest"
)

func serve(h web.Handler) {
	_ = httptest.NewServer(h)
	_ = &web.Server{Addr: ":80", Handler: h}
	_ = web.Server{Addr: ":81"}
	_ = &web.Client{Transport: rt}
	_ = web.ListenAndServe(":82", nil)
}
"#;
        let out = rewrite("serve.go", src, false);
        assert!(out.contains("httptest.NewServer(cccprobe.Handler(h))"), "{out}");
        assert!(out.contains("Handler: cccprobe.Handler(h)}"), "{out}");
        assert!(out.contains("web.Server{Handler: cccprobe.Handler(nil), Addr: \":81\"}"), "{out}");
        assert!(out.contains("Transport: cccprobe.Transport(rt)}"), "{out}");
        assert!(out.contains("web.ListenAndServe(\":82\", cccprobe.Handler(nil))"), "{out}");
    }

    #[test]
    fn grpc_servers_and_dials_take_the_interceptors_first() {
        let src = r#"package shop

import g "google.golang.org/grpc"

func run(opts []g.ServerOption, dial []g.DialOption) {
	_ = g.NewServer()
	_ = g.NewServer(a, b)
	_ = g.NewServer(opts...)
	_ = g.NewServer(a, opts...)
	_, _ = g.NewClient("x")
	_, _ = g.DialContext(ctx, "x", dial...)
}
"#;
        let out = rewrite("server.go", src, true);
        assert!(out.contains("; import cccprobegrpc \"example.com/shop/internal/cccprobegrpc\""), "{out}");
        assert!(out.contains("g.NewServer(cccprobegrpc.UnaryServer(), cccprobegrpc.StreamServer())"), "{out}");
        assert!(out.contains("g.NewServer(cccprobegrpc.UnaryServer(), cccprobegrpc.StreamServer(), a, b)"), "{out}");
        assert!(out.contains("g.NewServer(append([]g.ServerOption{cccprobegrpc.UnaryServer(), cccprobegrpc.StreamServer()}, opts...)...)"), "{out}");
        assert!(out.contains("g.NewServer(append([]g.ServerOption{cccprobegrpc.UnaryServer(), cccprobegrpc.StreamServer(), a}, opts...)...)"), "{out}");
        assert!(out.contains("g.NewClient(\"x\", cccprobegrpc.UnaryClient(), cccprobegrpc.StreamClient())"), "{out}");
        assert!(out.contains("g.DialContext(ctx, \"x\", append([]g.DialOption{cccprobegrpc.UnaryClient(), cccprobegrpc.StreamClient()}, dial...)...)"), "{out}");
        // a module that does not require grpc keeps grpc's calls as they were
        let plain = rewrite("server.go", src, false);
        assert!(!plain.contains("cccprobegrpc"), "{plain}");
    }

    #[test]
    fn a_go_mod_names_its_module_and_requirements() {
        let gomod = "module example.com/shop\n\ngo 1.22\n\nrequire (\n\tgoogle.golang.org/grpc v1.81.1\n)\n";
        assert_eq!(module_path(gomod).as_deref(), Some("example.com/shop"));
        assert!(requires(gomod, GRPC_MODULE));
        assert!(requires("module x\nrequire google.golang.org/grpc v1.0.0\n", GRPC_MODULE));
        assert!(!requires(gomod, "google.golang.org/grpc/examples"));
    }
}
