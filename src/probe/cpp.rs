// c++ - a `::ccc_probe::hit` atop every function, a fixture guard atop every gtest, catch2, doctest and boost test, threads and
// cpp-httplib handlers and clients carrying the fixture, grpc++ servers and channels intercepted - the header-only runtime is
// included by relative path, with a `#line` after it so every line keeps its number

use super::{collect, dir_of, mapped, named_children, quote, relative, Cx, Prober, Report, Rewrite, Source, RUNTIME_DIR};
use anyhow::Result;
use tree_sitter::Node;

const RUNTIME: &str = include_str!("../../probes/cpp/ccc_probe.hpp");
const HTTPLIB: &str = include_str!("../../probes/cpp/ccc_probe_httplib.hpp");
const GRPC: &str = include_str!("../../probes/cpp/ccc_probe_grpc.hpp");
// macros whose body is a test, named by their arguments - `TEST(Suite, Name)` runs as `Suite.Name`
const GTEST: &[&str] = &["TEST", "TEST_F", "TEST_P", "TYPED_TEST", "TYPED_TEST_P"];
const BOOST: &[&str] = &["BOOST_AUTO_TEST_CASE", "BOOST_FIXTURE_TEST_CASE", "BOOST_DATA_TEST_CASE"];
// macros named by a string, whose body follows the call - catch2 and doctest
const CATCH: &[&str] = &["TEST_CASE", "SCENARIO", "TEST_CASE_METHOD", "TEMPLATE_TEST_CASE"];
const HTTPLIB_ROUTES: &[&str] = &["Get", "Post", "Put", "Patch", "Delete", "Options"];

#[derive(Default)]
pub(crate) struct Cpp {
    // a .h may be included by c sources, which cannot read the runtime
    c_sources: bool,
    headers_left: usize,
    httplib: bool,
    grpc: bool,
}

impl Prober for Cpp {
    fn prepare(&mut self, cx: &Cx) -> Result<()> {
        self.c_sources = cx.files.iter().any(|f| f.ends_with(".c") && mapped(f, 0));
        Ok(())
    }

    fn rewrite(&mut self, _cx: &Cx, file: &Source) -> Result<Rewrite> {
        let mut rw = Rewrite::default();
        if self.c_sources && file.rel.ends_with(".h") {
            self.headers_left += 1;
            return Ok(rw);
        }
        for def in file.defs() {
            let Some(body) = def.node.child_by_field_name("body").filter(|b| b.kind() == "compound_statement") else { continue };
            let at = body.start_byte() + 1;
            if let Some(name) = macro_test(file, def.node, &def.func.name) {
                rw.edits.insert(at, fixture(&name, file));
                rw.fixtures += 1;
                continue;
            }
            if file.test || def.is_test() || constant(file, def.node) {
                continue;
            }
            rw.edits.insert(at, format!(" ::ccc_probe::hit({}, {}, {});", quote(&file.path), quote(&def.qualified("::")), def.func.start_line));
            rw.functions += 1;
        }
        let uses = |needle: &str| file.imports.iter().any(|i| i.module.contains(needle));
        let (httplib, grpc) = (uses("httplib.h"), uses("grpcpp/") || uses("grpc++/"));
        let servers = if httplib { declared(file, &["httplib::Server", "httplib::SSLServer"]) } else { Vec::new() };
        let (mut httplib_used, mut grpc_used) = (false, false);
        collect(file.root(), &mut |n| match n.kind() {
            "expression_statement" => {
                if let Some(name) = catch_test(file, n) {
                    if let Some(body) = n.next_named_sibling().filter(|b| b.kind() == "compound_statement") {
                        rw.edits.insert(body.start_byte() + 1, fixture(&name, file));
                        rw.fixtures += 1;
                    }
                }
            }
            "call_expression" => {
                if wrap_thread(file, n, &mut rw) {
                    rw.wrapped += 1;
                } else if httplib && wrap_route(file, n, &servers, &mut rw) {
                    rw.wrapped += 1;
                    httplib_used = true;
                } else if grpc && wrap_grpc(file, n, &mut rw) {
                    rw.wrapped += 1;
                    grpc_used = true;
                }
            }
            "declaration" => {
                if wrap_thread_declaration(file, n, &mut rw) {
                    rw.wrapped += 1;
                } else if httplib && client_declaration(file, n, &mut rw) {
                    rw.wrapped += 1;
                    httplib_used = true;
                }
            }
            _ => {}
        });
        if rw.edits.is_empty() {
            return Ok(rw);
        }
        let dir = dir_of(&file.rel);
        let (at, line) = include_at(file);
        rw.edits.insert(at, format!("#include {}\n#line {line}\n", quote(&relative(dir, &format!("{RUNTIME_DIR}/cpp/ccc_probe.hpp")))));
        for (used, needle, header) in [(httplib_used, "httplib.h", "ccc_probe_httplib.hpp"), (grpc_used, "grpc", "ccc_probe_grpc.hpp")] {
            if !used {
                continue;
            }
            if let Some((end, next)) = after_include(file, needle) {
                rw.edits.insert(end, format!("#include {}\n#line {next}\n", quote(&relative(dir, &format!("{RUNTIME_DIR}/cpp/{header}")))));
            }
        }
        self.httplib |= httplib_used;
        self.grpc |= grpc_used;
        Ok(rw)
    }

    fn finish(&mut self, cx: &Cx, report: &mut Report) -> Result<()> {
        let dir = format!("{RUNTIME_DIR}/cpp");
        let commit = cx.commit.as_deref().unwrap_or_default();
        cx.write(&format!("{dir}/ccc_probe.hpp"), &RUNTIME.replacen("inline const char* commit() { return \"\"; }", &format!("inline const char* commit() {{ return {}; }}", quote(commit)), 1))?;
        report.runtime.push(format!("{dir}/ccc_probe.hpp - header-only, included by relative path, c++17"));
        if self.httplib {
            cx.write(&format!("{dir}/ccc_probe_httplib.hpp"), HTTPLIB)?;
            report.runtime.push(format!("{dir}/ccc_probe_httplib.hpp - cpp-httplib handlers and clients"));
        }
        if self.grpc {
            cx.write(&format!("{dir}/ccc_probe_grpc.hpp"), GRPC)?;
            report.runtime.push(format!("{dir}/ccc_probe_grpc.hpp - grpc++ interceptors"));
        }
        if self.headers_left > 0 {
            report.skipped.push(format!("{} .h files - the project has c sources that may include them, and c cannot read the runtime", self.headers_left));
        }
        report.run.push("cmake --build build && ctest --test-dir build   # or however the suites build and run".to_string());
        Ok(())
    }
}

fn fixture(name: &str, file: &Source) -> String {
    format!(" auto __ccc_test = ::ccc_probe::fixture({}, {}, {});", quote(name), quote(&file.path), quote(file.kind))
}

// a gtest or boost test macro's name for the test - `TEST(Billing, Charges)` is `Billing.Charges`
fn macro_test(file: &Source, func: Node, name: &str) -> Option<String> {
    let gtest = GTEST.contains(&name);
    if !gtest && !BOOST.contains(&name) {
        return None;
    }
    let declarator = func.child_by_field_name("declarator")?;
    let params = declarator.child_by_field_name("parameters")?;
    let args: Vec<String> = named_children(params).iter().map(|p| file.text_of(*p).trim().to_string()).collect();
    match (gtest, args.as_slice()) {
        (true, [suite, test, ..]) => Some(format!("{suite}.{test}")),
        (false, [test, ..]) => Some(test.clone()),
        _ => None,
    }
}

// a catch2 or doctest registration - `TEST_CASE("prices", "[billing]")` names its test by the string
fn catch_test(file: &Source, stmt: Node) -> Option<String> {
    let call = stmt.named_child(0).filter(|c| c.kind() == "call_expression")?;
    let function = call.child_by_field_name("function")?;
    if function.kind() != "identifier" || !CATCH.contains(&file.text_of(function)) {
        return None;
    }
    let args = call.child_by_field_name("arguments")?;
    let title = named_children(args).into_iter().find(|a| a.kind() == "string_literal")?;
    let text = file.text_of(title);
    Some(text.trim_matches('"').to_string())
}

// a constexpr or consteval function cannot call the runtime
fn constant(file: &Source, func: Node) -> bool {
    let Some(declarator) = func.child_by_field_name("declarator") else { return false };
    file.text[func.start_byte()..declarator.start_byte()].split(|c: char| !c.is_alphanumeric() && c != '_').any(|w| w == "constexpr" || w == "consteval")
}

// the names declared with one of these types, local or member
fn declared(file: &Source, types: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    collect(file.root(), &mut |n| {
        if !matches!(n.kind(), "declaration" | "field_declaration") {
            return;
        }
        let Some(ty) = n.child_by_field_name("type") else { return };
        if !types.contains(&file.text_of(ty).replace(char::is_whitespace, "").as_str()) {
            return;
        }
        let mut cursor = n.walk();
        for d in n.children_by_field_name("declarator", &mut cursor) {
            let name = d.child_by_field_name("declarator").unwrap_or(d);
            out.push(file.text_of(name).to_string());
        }
    });
    out
}

// `std::thread(fn, args)` and `std::async(policy, fn, args)` hand fn the fixture of whoever started it
fn wrap_thread(file: &Source, call: Node, rw: &mut Rewrite) -> bool {
    let (Some(function), Some(args)) = (call.child_by_field_name("function"), call.child_by_field_name("arguments")) else { return false };
    let callee = file.text_of(function).replace(char::is_whitespace, "");
    let list = named_children(args);
    let at = match callee.as_str() {
        "std::thread" | "std::jthread" => 0,
        "std::async" if list.first().is_some_and(|a| file.text_of(*a).contains("launch")) => 1,
        "std::async" => 0,
        _ => return false,
    };
    let Some(callable) = list.get(at) else { return false };
    rw.edits.wrap(*callable, "::ccc_probe::bind_current(", ")");
    true
}

// `std::thread t(fn, args);` - the same, declared rather than called
fn wrap_thread_declaration(file: &Source, decl: Node, rw: &mut Rewrite) -> bool {
    let Some(ty) = decl.child_by_field_name("type") else { return false };
    if !matches!(file.text_of(ty).replace(char::is_whitespace, "").as_str(), "std::thread" | "std::jthread") {
        return false;
    }
    let Some(declarator) = decl.child_by_field_name("declarator") else { return false };
    // `std::thread t(work, n)` with only names inside parses as a function declaration - the vexing parse
    let args = match declarator.kind() {
        "init_declarator" => declarator.child_by_field_name("value").filter(|v| matches!(v.kind(), "argument_list" | "initializer_list")),
        "function_declarator" => declarator.child_by_field_name("parameters"),
        _ => None,
    };
    let Some(callable) = args.and_then(|a| named_children(a).first().copied()) else { return false };
    rw.edits.wrap(callable, "::ccc_probe::bind_current(", ")");
    true
}

// `svr.Get("/x", [](auto& req, auto& res) {...})` on a server declared here - the handler serves each request for its baggage's fixture
fn wrap_route(file: &Source, call: Node, servers: &[String], rw: &mut Rewrite) -> bool {
    let (Some(function), Some(args)) = (call.child_by_field_name("function"), call.child_by_field_name("arguments")) else { return false };
    if function.kind() != "field_expression" {
        return false;
    }
    let receiver = function.child_by_field_name("argument").map(|a| file.text_of(a).to_string()).unwrap_or_default();
    let route = function.child_by_field_name("field").map(|f| file.text_of(f)).unwrap_or_default();
    if !servers.contains(&receiver) || !HTTPLIB_ROUTES.contains(&route) {
        return false;
    }
    let Some(handler) = named_children(args).last().copied().filter(|h| h.kind() == "lambda_expression") else { return false };
    rw.edits.wrap(handler, "::ccc_probe::httplib_serve(", ")");
    true
}

// `httplib::Client cli(..);` gains a header writer that sends the fixture on every request
fn client_declaration(file: &Source, decl: Node, rw: &mut Rewrite) -> bool {
    let Some(ty) = decl.child_by_field_name("type") else { return false };
    if !matches!(file.text_of(ty).replace(char::is_whitespace, "").as_str(), "httplib::Client" | "httplib::SSLClient") {
        return false;
    }
    if !decl.parent().is_some_and(|p| p.kind() == "compound_statement") {
        return false;
    }
    let Some(name) = decl.child_by_field_name("declarator").map(|d| d.child_by_field_name("declarator").unwrap_or(d)) else { return false };
    rw.edits.close(decl.end_byte(), format!(" ::ccc_probe::httplib_client({});", file.text_of(name)));
    true
}

// grpc++ channels made with interceptors and servers built with them
fn wrap_grpc(file: &Source, call: Node, rw: &mut Rewrite) -> bool {
    let Some(function) = call.child_by_field_name("function") else { return false };
    match file.text_of(function).replace(char::is_whitespace, "").as_str() {
        "grpc::CreateChannel" => rw.edits.replace(function, "::ccc_probe::grpc_channel"),
        "grpc::CreateCustomChannel" => rw.edits.replace(function, "::ccc_probe::grpc_custom_channel"),
        _ => {
            let builds = function.kind() == "field_expression" && function.child_by_field_name("field").is_some_and(|f| file.text_of(f) == "BuildAndStart");
            let Some(builder) = function.child_by_field_name("argument").filter(|_| builds) else { return false };
            rw.edits.wrap(builder, "::ccc_probe::grpc_server(", ")");
        }
    }
    true
}

// where the runtime's include goes, and the number of the line it goes ahead of - before the first include the file makes
// at its top, or inside its include guard, so a macro the file defines first still comes first
fn include_at(file: &Source) -> (usize, usize) {
    let mut scope = file.root();
    loop {
        let children = named_children(scope);
        let guard = children.iter().find(|c| !matches!(c.kind(), "comment")).copied().filter(|c| is_guard(file, *c));
        match guard {
            Some(g) => scope = g,
            None => break,
        }
    }
    let children = named_children(scope);
    let first_include = children.iter().find(|c| c.kind() == "preproc_include");
    let first_code = children.iter().find(|c| !matches!(c.kind(), "comment" | "preproc_def" | "preproc_call" | "preproc_function_def" | "identifier"));
    let node = first_include.or(first_code).copied().unwrap_or(scope);
    let start = file.text[..node.start_byte()].rfind('\n').map_or(0, |i| i + 1);
    (start, file.text[..start].matches('\n').count() + 1)
}

// `#ifndef X` followed by `#define X` - a header's include guard
fn is_guard(file: &Source, node: Node) -> bool {
    if node.kind() != "preproc_ifdef" || !file.text_of(node).starts_with("#ifndef") {
        return false;
    }
    let name = node.child_by_field_name("name").map(|n| file.text_of(n));
    let define = named_children(node).into_iter().find(|c| c.kind() == "preproc_def");
    define.and_then(|d| d.child_by_field_name("name")).map(|n| file.text_of(n)) == name
}

// the end of the line that includes a library, and the number of the line after it
fn after_include(file: &Source, needle: &str) -> Option<(usize, usize)> {
    let mut found = None;
    collect(file.root(), &mut |n| {
        if n.kind() == "preproc_include" && n.child_by_field_name("path").is_some_and(|p| file.text_of(p).contains(needle)) {
            found = Some(n);
        }
    });
    let node = found?;
    let end = file.text[node.start_byte()..].find('\n').map_or(file.text.len(), |i| node.start_byte() + i + 1);
    Some((end, file.text[..end].matches('\n').count() + 1))
}

#[cfg(test)]
mod tests {
    use super::super::{Cx, Prober, Source, Target};
    use super::*;
    use crate::changes::is_test_path;
    use crate::extract;
    use crate::languages::Language;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn rewrite(rel: &str, src: &str) -> String {
        let cx = Cx { target: Target::Cpp, out: PathBuf::new(), prefix: String::new(), commit: None, files: BTreeSet::new(), rules: Vec::new() };
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&Language::Cpp.ts_language()).unwrap();
        let ex = extract::extract(Language::Cpp, src).unwrap();
        let file = Source {
            rel: rel.into(),
            path: rel.into(),
            lang: Language::Cpp,
            text: src.into(),
            tree: parser.parse(src, None).unwrap(),
            funcs: ex.funcs,
            imports: ex.imports,
            test: is_test_path(rel),
            kind: cx.kind(rel),
        };
        Cpp::default().rewrite(&cx, &file).unwrap().edits.apply(src)
    }

    #[test]
    fn every_function_counts_its_runs_and_the_include_keeps_line_numbers() {
        let src = "#define _GNU_SOURCE\n#include <vector>\nnamespace shop {\nint Store::put(int x) const {\n  return x;\n}\nconstexpr int twice(int x) { return 2 * x; }\n}\n";
        let out = rewrite("src/store.cpp", src);
        assert!(out.starts_with("#define _GNU_SOURCE\n#include \"../.ccc-probe/cpp/ccc_probe.hpp\"\n#line 2\n#include <vector>\n"), "{out}");
        assert!(out.contains("int Store::put(int x) const { ::ccc_probe::hit(\"src/store.cpp\", \"Store::put\", 4);\n"), "{out}");
        assert!(out.contains("constexpr int twice(int x) { return 2 * x; }"), "a constexpr function cannot call the runtime: {out}");
    }

    #[test]
    fn a_header_takes_the_include_inside_its_guard() {
        let src = "#ifndef SHOP_H\n#define SHOP_H\ninline int price(int n) { return n; }\n#endif\n";
        let out = rewrite("include/shop.hpp", src);
        assert!(out.starts_with("#ifndef SHOP_H\n#define SHOP_H\n#include \"../.ccc-probe/cpp/ccc_probe.hpp\"\n#line 3\ninline int price"), "{out}");
    }

    #[test]
    fn every_test_macro_is_a_fixture_named_the_way_its_runner_selects_it() {
        let src = "#include <gtest/gtest.h>\nTEST(Billing, Charges) {\n  EXPECT_EQ(1, 1);\n}\nTEST_CASE(\"prices\", \"[billing]\") {\n  REQUIRE(true);\n}\nBOOST_AUTO_TEST_CASE(refunds) {}\nvoid helper() {}\n";
        let out = rewrite("tests/billing_test.cpp", src);
        assert!(out.contains("TEST(Billing, Charges) { auto __ccc_test = ::ccc_probe::fixture(\"Billing.Charges\", \"tests/billing_test.cpp\", \"unit\");\n"), "{out}");
        assert!(out.contains("TEST_CASE(\"prices\", \"[billing]\") { auto __ccc_test = ::ccc_probe::fixture(\"prices\""), "{out}");
        assert!(out.contains("BOOST_AUTO_TEST_CASE(refunds) { auto __ccc_test = ::ccc_probe::fixture(\"refunds\""), "{out}");
        assert!(out.contains("void helper() {}"), "{out}");
    }

    #[test]
    fn threads_servers_clients_and_channels_carry_the_fixture() {
        let src = "#include \"httplib.h\"\n#include <grpcpp/grpcpp.h>\nvoid run() {\n  httplib::Server svr;\n  svr.Get(\"/\", [](const httplib::Request& req, httplib::Response& res) {});\n  httplib::Client cli(\"x\");\n  cli.Get(\"/\", [](const char*, size_t) { return true; });\n  std::thread t(work, 1);\n  std::thread u(work, n);\n  auto f = std::async(std::launch::async, work);\n  auto ch = grpc::CreateChannel(\"x\", creds);\n  auto server = builder.BuildAndStart();\n}\n";
        let out = rewrite("src/server.cpp", src);
        assert!(out.starts_with("#include \"../.ccc-probe/cpp/ccc_probe.hpp\"\n#line 1\n#include \"httplib.h\"\n#include \"../.ccc-probe/cpp/ccc_probe_httplib.hpp\"\n#line 2\n#include <grpcpp/grpcpp.h>\n#include \"../.ccc-probe/cpp/ccc_probe_grpc.hpp\"\n#line 3\n"), "{out}");
        assert!(out.contains("svr.Get(\"/\", ::ccc_probe::httplib_serve([](const httplib::Request& req, httplib::Response& res) {}));"), "{out}");
        assert!(out.contains("httplib::Client cli(\"x\"); ::ccc_probe::httplib_client(cli);"), "{out}");
        assert!(out.contains("cli.Get(\"/\", [](const char*, size_t) { return true; });"), "a client's own callback stays as it was: {out}");
        assert!(out.contains("std::thread t(::ccc_probe::bind_current(work), 1);"), "{out}");
        assert!(out.contains("std::thread u(::ccc_probe::bind_current(work), n);"), "{out}");
        assert!(out.contains("std::async(std::launch::async, ::ccc_probe::bind_current(work));"), "{out}");
        assert!(out.contains("::ccc_probe::grpc_channel(\"x\", creds);"), "{out}");
        assert!(out.contains("::ccc_probe::grpc_server(builder).BuildAndStart();"), "{out}");
    }
}
