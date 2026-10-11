// python - a `_ccc.hit` atop every function, the runtime imported into every probed file, and every test file and conftest
// registering it with the pytest that loads them

use super::{dir_of, named_children, quote, relative, Cx, Def, Prober, Report, Rewrite, Source, RUNTIME_DIR};
use anyhow::Result;
use std::collections::BTreeMap;
use tree_sitter::Node;

const RUNTIME: &str = include_str!("../../probes/python/ccc_probe.py");
// statements that open a block - nothing can share their line ahead of them
const COMPOUND: &[&str] = &[
    "function_definition",
    "class_definition",
    "decorated_definition",
    "if_statement",
    "for_statement",
    "while_statement",
    "try_statement",
    "with_statement",
    "match_statement",
];

#[derive(Default)]
pub(crate) struct Python {
    // each test file's kind, by its path inside the copy - the pytest plugin reads it back
    kinds: BTreeMap<String, &'static str>,
}

impl Prober for Python {
    fn prepare(&mut self, _cx: &Cx) -> Result<()> {
        Ok(())
    }

    fn rewrite(&mut self, _cx: &Cx, file: &Source) -> Result<Rewrite> {
        let mut rw = Rewrite::default();
        if file.rel.ends_with(".pyi") {
            return Ok(rw);
        }
        let tests = file.test || file.rel.rsplit('/').next() == Some("conftest.py");
        if tests {
            self.kinds.insert(file.rel.clone(), file.kind);
            rw.fixtures += file.defs().iter().filter(|d| d.is_test()).count();
        } else {
            for def in file.defs() {
                if !def.is_test() && hit(file, &def, &mut rw) {
                    rw.functions += 1;
                }
            }
            if rw.edits.is_empty() {
                return Ok(rw);
            }
        }
        let to = relative(dir_of(&file.rel), &format!("{RUNTIME_DIR}/python"));
        let mut import = format!(
            "import sys as _ccc_sys, os as _ccc_os; _ccc_sys.path.append(_ccc_os.path.join(_ccc_os.path.dirname(_ccc_os.path.abspath(__file__)), {})) if \"ccc_probe\" not in _ccc_sys.modules else None; import ccc_probe as _ccc",
            quote(&to)
        );
        // a test file registers the pytest plugin as pytest collects it
        if tests {
            import.push_str("; _ccc.attach()");
        }
        place_import(file, &import, &mut rw);
        Ok(rw)
    }

    fn finish(&mut self, cx: &Cx, report: &mut Report) -> Result<()> {
        let path = format!("{RUNTIME_DIR}/python/ccc_probe.py");
        let runtime = RUNTIME
            .replacen("COMMIT = \"\"", &format!("COMMIT = {}", quote(cx.commit.as_deref().unwrap_or_default())), 1)
            .replacen("PREFIX = \"\"", &format!("PREFIX = {}", quote(&cx.prefix)), 1)
            .replacen("KINDS = {}", &format!("KINDS = {}", serde_json::to_string(&self.kinds)?), 1);
        cx.write(&path, &runtime)?;
        report.runtime.push(format!("{path} - imported by every probed file, so no install step"));
        report.run.push("pytest   # or python -m unittest - each test is a fixture either way".to_string());
        Ok(())
    }
}

fn docstring(stmt: Node) -> bool {
    stmt.kind() == "expression_statement" && stmt.named_child_count() == 1 && stmt.named_child(0).is_some_and(|s| matches!(s.kind(), "string" | "concatenated_string"))
}

// the count at the top of one function's body - beside its first statement, so its lines keep their numbers where python allows
fn hit(file: &Source, def: &Def, rw: &mut Rewrite) -> bool {
    let Some(body) = def.node.child_by_field_name("body") else { return false };
    let Some(first) = named_children(body).first().copied() else { return false };
    let call = format!("_ccc.hit({}, {}, {})", quote(&file.path), quote(&def.qualified(".")), def.func.start_line);
    if docstring(first) {
        rw.edits.insert(first.end_byte(), format!("; {call}"));
    } else if COMPOUND.contains(&first.kind()) {
        // a block opener keeps its line to itself, so the count takes a line of its own above it
        let line = file.text[..first.start_byte()].rfind('\n').map_or(0, |i| i + 1);
        let indent = &file.text[line..first.start_byte()];
        if !indent.trim().is_empty() {
            return false;
        }
        rw.edits.insert(first.start_byte(), format!("{call}\n{indent}"));
    } else {
        rw.edits.insert(first.start_byte(), format!("{call}; "));
    }
    true
}

// the import after the docstring and `from __future__` lines, or ahead of the first statement - on a line it shares where it can
fn place_import(file: &Source, import: &str, rw: &mut Rewrite) {
    let statements = named_children(file.root());
    let mut header = None;
    for (i, s) in statements.iter().enumerate() {
        if (i == 0 && docstring(*s)) || s.kind() == "future_import_statement" {
            header = Some(s.end_byte());
            continue;
        }
        break;
    }
    match (header, statements.first()) {
        (Some(end), _) => rw.edits.insert(end, format!("; {import}")),
        (None, Some(first)) if COMPOUND.contains(&first.kind()) => rw.edits.insert(first.start_byte(), format!("{import}\n")),
        (None, Some(first)) => rw.edits.insert(first.start_byte(), format!("{import}; ")),
        (None, None) => rw.edits.insert(0, format!("{import}\n")),
    }
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

    fn rewrite(python: &mut Python, rel: &str, src: &str) -> String {
        let cx = Cx { target: Target::Python, out: PathBuf::new(), prefix: String::new(), commit: None, files: BTreeSet::new(), rules: Vec::new() };
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&Language::Python.ts_language()).unwrap();
        let ex = extract::extract(Language::Python, src).unwrap();
        let file = Source {
            rel: rel.into(),
            path: rel.into(),
            lang: Language::Python,
            text: src.into(),
            tree: parser.parse(src, None).unwrap(),
            funcs: ex.funcs,
            imports: ex.imports,
            test: is_test_path(rel),
            kind: cx.kind(rel),
        };
        python.rewrite(&cx, &file).unwrap().edits.apply(src)
    }

    #[test]
    fn every_function_counts_its_runs_and_lines_hold_where_python_lets_them() {
        let src = "\"\"\"billing\"\"\"\nfrom __future__ import annotations\nimport os\n\n\nclass Ledger:\n    def add(self, n):\n        \"\"\"adds\"\"\"\n        return n\n\n    def refund(self, n):\n        if n:\n            return 1\n        return 0\n\n\ndef price(n): return n * 2\n";
        let out = rewrite(&mut Python::default(), "billing/ledger.py", src);
        assert!(out.starts_with("\"\"\"billing\"\"\"\nfrom __future__ import annotations; import sys as _ccc_sys"), "{out}");
        assert!(out.contains("_ccc_os.path.abspath(__file__)), \"../.ccc-probe/python\")) if \"ccc_probe\" not in _ccc_sys.modules else None; import ccc_probe as _ccc\nimport os"), "{out}");
        assert!(out.contains("        \"\"\"adds\"\"\"; _ccc.hit(\"billing/ledger.py\", \"Ledger.add\", 7)\n"), "{out}");
        // an `if` keeps its line, so this one count takes a line of its own
        assert!(out.contains("    def refund(self, n):\n        _ccc.hit(\"billing/ledger.py\", \"Ledger.refund\", 11)\n        if n:"), "{out}");
        assert!(out.contains("def price(n): _ccc.hit(\"billing/ledger.py\", \"price\", 17); return n * 2\n"), "{out}");
    }

    #[test]
    fn a_test_file_counts_nothing_but_registers_the_plugin_and_its_kind() {
        let mut python = Python::default();
        let src = "import requests\n\n\ndef test_charges():\n    assert requests.get('x')\n";
        let out = rewrite(&mut python, "qa/contract/test_shapes.py", src);
        assert!(out.starts_with("import sys as _ccc_sys"), "{out}");
        assert!(out.contains("import ccc_probe as _ccc; _ccc.attach(); import requests\n"), "{out}");
        assert!(!out.contains("_ccc.hit"), "{out}");
        assert_eq!(python.kinds.get("qa/contract/test_shapes.py"), Some(&"contract"));
        // a module with nothing to count is left as it was
        assert_eq!(rewrite(&mut python, "consts.py", "RATE = 2\n"), "RATE = 2\n");
    }

    #[test]
    fn the_runtime_has_the_lines_the_rewrite_fills_in() {
        for line in ["COMMIT = \"\"", "PREFIX = \"\"", "KINDS = {}"] {
            assert_eq!(RUNTIME.matches(line).count(), 1, "{line}");
        }
    }
}
