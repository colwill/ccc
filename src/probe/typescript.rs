// typescript - a `__ccc.hit` atop every function, every `it`/`test` callback wrapped as a fixture, and the runtime installed
// into the copy's node_modules, where it patches http, https, fetch and grpc-js as it loads

use super::{child_of_kind, collect, named_children, quote, Cx, Def, Prober, Report, Rewrite, Source};
use anyhow::Result;
use tree_sitter::Node;

const RUNTIME: &str = include_str!("../../probes/typescript/index.js");
const RUNTIME_ESM: &str = include_str!("../../probes/typescript/index.mjs");
const RUNTIME_TYPES: &str = include_str!("../../probes/typescript/index.d.ts");
// the package the probes import - a bare name resolves from source, from built output and inside every test runner alike
const PACKAGE: &str = "ccc-probe";
// the registrars whose callbacks are tests - `describe` and its kin only group them
const TEST_REGISTRARS: &[&str] = &["it", "test", "specify"];
// a file importing one of these registers tests wherever it lives
const TEST_MODULES: &[&str] = &["vitest", "@jest/globals", "node:test", "mocha", "bun:test", "@playwright/test"];

#[derive(Default)]
pub(crate) struct TypeScript;

impl Prober for TypeScript {
    fn prepare(&mut self, _cx: &Cx) -> Result<()> {
        Ok(())
    }

    fn rewrite(&mut self, _cx: &Cx, file: &Source) -> Result<Rewrite> {
        let mut rw = Rewrite::default();
        if file.rel.ends_with(".d.ts") || file.rel.ends_with(".d.mts") || file.rel.ends_with(".d.cts") {
            return Ok(rw);
        }
        let tests = file.test || file.imports.iter().any(|i| TEST_MODULES.contains(&i.module.as_str()));
        if !tests {
            for def in file.defs() {
                if def.is_test() {
                    continue;
                }
                if hit(file, &def, &mut rw) {
                    rw.functions += 1;
                }
            }
        }
        if tests {
            collect(file.root(), &mut |n| {
                if n.kind() == "call_expression" && wrap_test(file, n, &mut rw) {
                    rw.fixtures += 1;
                }
            });
        }
        if !rw.edits.is_empty() {
            rw.edits.insert(import_at(file), format!("import * as __ccc from {}; ", quote(PACKAGE)));
        }
        Ok(rw)
    }

    fn finish(&mut self, cx: &Cx, report: &mut Report) -> Result<()> {
        let dir = format!("node_modules/{PACKAGE}");
        let manifest = serde_json::json!({
            "name": PACKAGE,
            "version": "0.0.0",
            "private": true,
            "description": "ccc's runtime probe - written by `ccc probe typescript`",
            "main": "index.js",
            "types": "index.d.ts",
            "exports": { ".": { "types": "./index.d.ts", "import": "./index.mjs", "require": "./index.js" } },
        });
        cx.write(&format!("{dir}/package.json"), &format!("{}\n", serde_json::to_string_pretty(&manifest)?))?;
        cx.write(&format!("{dir}/index.js"), RUNTIME)?;
        cx.write(&format!("{dir}/index.mjs"), RUNTIME_ESM)?;
        cx.write(&format!("{dir}/index.d.ts"), RUNTIME_TYPES)?;
        cx.write(&format!("{dir}/commit.js"), &format!("module.exports = {};\n", quote(cx.commit.as_deref().unwrap_or_default())))?;
        report.runtime.push(format!("{dir}/ - imported as \"{PACKAGE}\""));
        report.run.push("npm test   # or however the suites run - jest, vitest, mocha and node --test all work".to_string());
        Ok(())
    }
}

// the count at the top of one function's body - `false` for a signature with no body
fn hit(file: &Source, def: &Def, rw: &mut Rewrite) -> bool {
    let Some(body) = def.node.child_by_field_name("body") else { return false };
    let call = format!("__ccc.hit({}, {}, {})", quote(&file.path), quote(&def.qualified(".")), def.func.start_line);
    if body.kind() != "statement_block" {
        // an arrow's expression body becomes a comma expression that counts first
        rw.edits.wrap(body, format!("({call}, "), ")");
        return true;
    }
    let mut at = body.start_byte() + 1;
    // a derived constructor's `super()` stays first, for the compilers that insist on it
    if def.func.name == "constructor" {
        if let Some(first) = named_children(body).first() {
            if first.kind() == "expression_statement" && super_call(*first) {
                at = first.end_byte();
            }
        }
    }
    rw.edits.insert(at, format!(" {call};"));
    true
}

fn super_call(stmt: Node) -> bool {
    stmt.named_child(0)
        .filter(|c| c.kind() == "call_expression")
        .and_then(|c| c.child_by_field_name("function"))
        .is_some_and(|f| f.kind() == "super")
}

// `it("charges", cb)` with cb wrapped as a fixture named the way the map labels it - `it("charges")`
fn wrap_test(file: &Source, call: Node, rw: &mut Rewrite) -> bool {
    let (Some(callee), Some(args)) = (call.child_by_field_name("function"), call.child_by_field_name("arguments")) else {
        return false;
    };
    let head = file
        .text_of(callee)
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .find(|s| !s.is_empty())
        .unwrap_or_default()
        .to_string();
    if !TEST_REGISTRARS.contains(&head.as_str()) {
        return false;
    }
    let list = named_children(args);
    let Some(title) = list.iter().find(|a| matches!(a.kind(), "string" | "template_string")) else { return false };
    let title = file.text_of(*title).split_whitespace().collect::<Vec<_>>().join(" ");
    let title = title.trim_matches(['"', '\'', '`']);
    let Some(callback) = list.iter().find(|a| matches!(a.kind(), "arrow_function" | "function_expression")) else {
        return false;
    };
    if title.is_empty() {
        return false;
    }
    let label = format!("{head}(\"{title}\")");
    rw.edits.wrap(*callback, format!("__ccc.fixture({}, {}, {}, ", quote(&label), quote(&file.path), quote(file.kind)), ")");
    true
}

// where the import goes - after a shebang and any directive prologue, on a line that already holds code so no line moves
fn import_at(file: &Source) -> usize {
    let root = file.root();
    let mut at = 0;
    if let Some(bang) = child_of_kind(root, "hash_bang_line") {
        at = file.text[bang.end_byte()..].find('\n').map_or(file.text.len(), |n| bang.end_byte() + n + 1);
    }
    for stmt in named_children(root) {
        if stmt.kind() == "hash_bang_line" || stmt.kind() == "comment" {
            continue;
        }
        let directive = stmt.kind() == "expression_statement" && stmt.named_child(0).is_some_and(|s| s.kind() == "string");
        if !directive {
            break;
        }
        at = stmt.end_byte();
    }
    at
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
        let cx = Cx { target: Target::TypeScript, out: PathBuf::new(), prefix: String::new(), commit: None, files: BTreeSet::new(), rules: Vec::new() };
        let lang = if rel.ends_with(".tsx") { Language::Tsx } else { Language::TypeScript };
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.ts_language()).unwrap();
        let ex = extract::extract(lang, src).unwrap();
        let file = Source {
            rel: rel.into(),
            path: rel.into(),
            lang,
            text: src.into(),
            tree: parser.parse(src, None).unwrap(),
            funcs: ex.funcs,
            imports: ex.imports,
            test: is_test_path(rel),
            kind: cx.kind(rel),
        };
        TypeScript.rewrite(&cx, &file).unwrap().edits.apply(src)
    }

    #[test]
    fn every_function_counts_its_runs_and_no_line_moves() {
        let src = "#!/usr/bin/env node\n\"use strict\";\nimport { a } from \"./a\";\nexport function charge(n: number): number {\n  return n;\n}\nconst add = (n: number) => n + 1;\nclass Billing extends Base {\n  constructor() {\n    super();\n  }\n  refund(): void {}\n}\nexport function sig(): void;\n";
        let out = rewrite("src/billing.ts", src);
        assert!(out.starts_with("#!/usr/bin/env node\n\"use strict\";import * as __ccc from \"ccc-probe\"; \nimport"), "{out}");
        assert!(out.contains("export function charge(n: number): number { __ccc.hit(\"src/billing.ts\", \"charge\", 4);\n"), "{out}");
        assert!(out.contains("const add = (n: number) => (__ccc.hit(\"src/billing.ts\", \"add\", 7), n + 1);"), "{out}");
        assert!(out.contains("    super(); __ccc.hit(\"src/billing.ts\", \"Billing.constructor\", 9);\n"), "{out}");
        assert!(out.contains("refund(): void { __ccc.hit(\"src/billing.ts\", \"Billing.refund\", 12);}"), "{out}");
        assert!(out.contains("export function sig(): void;\n"), "{out}");
        assert_eq!(out.lines().count(), src.lines().count());
    }

    #[test]
    fn a_test_callback_runs_as_the_fixture_the_map_names_and_its_file_counts_nothing() {
        let src = "import { it, describe } from \"vitest\";\ndescribe(\"billing\", () => {\n  it(\"charges a fee\", async () => {\n    await charge(1);\n  });\n  it.each([1, 2])(\"refunds %s\", (n) => {});\n  test(\"done style\", function (done) { done(); }, 500);\n});\nfunction helper() {}\n";
        let out = rewrite("src/billing.test.ts", src);
        assert!(out.starts_with("import * as __ccc from \"ccc-probe\"; import { it, describe }"), "{out}");
        assert!(out.contains("it(\"charges a fee\", __ccc.fixture(\"it(\\\"charges a fee\\\")\", \"src/billing.test.ts\", \"unit\", async () => {\n    await charge(1);\n  }));"), "{out}");
        assert!(out.contains("(\"refunds %s\", __ccc.fixture(\"it(\\\"refunds %s\\\")\", \"src/billing.test.ts\", \"unit\", (n) => {}));"), "{out}");
        assert!(out.contains("__ccc.fixture(\"test(\\\"done style\\\")\", \"src/billing.test.ts\", \"unit\", function (done) { done(); }), 500);"), "{out}");
        assert!(out.contains("describe(\"billing\", () => {"), "a group is not a test: {out}");
        assert!(out.contains("function helper() {}"), "{out}");
    }

    #[test]
    fn a_file_with_nothing_to_probe_is_left_alone() {
        assert_eq!(rewrite("src/types.ts", "export type A = { a: number };\n"), "export type A = { a: number };\n");
        assert_eq!(rewrite("src/api.d.ts", "export function a(): void;\n"), "export function a(): void;\n");
    }
}
