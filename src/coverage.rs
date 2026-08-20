//! Which tests exercise which functions.
//!
//! One relation, built once and read by everything that reports coverage:
//! `changes` (`tested` / `tested_by`), `insights::test_targets` (`covered` /
//! `covered_by`) and the trigger walk. They used to compute it separately, each
//! keyed on the callee's bare name, which made a test a "cover" of every
//! same-named function in the repository - a Rust test calling `std::fs::write`
//! was reported as covering a TypeScript method called `write`.
//!
//! A test reference is tied to a *definition*, addressed by (file, index into
//! that file's `funcs`), and only when something beyond the name agrees:
//!
//!   1. calls whose receiver or qualifier resolves to nothing in the project
//!      are external - `std::fs::write` covers nothing, it leaves the project;
//!   2. a candidate must be in the caller's runtime family, so a name shared by
//!      two ecosystems is never one definition;
//!   3. what is left needs evidence - the receiver's declared type, the same
//!      file, an import, or a qualifier naming the defining file - and where
//!      the evidence fits more than one definition it produces nothing, the
//!      same discipline `insights::build_graph` applies to call edges.
//!
//! Name-only matching survives in one corner: an untyped language calling a
//! name with exactly one definition in the project. It is labelled as such,
//! since it is the weakest thing here that is still worth reporting.

use crate::changes::{is_test_fn_name, is_test_path, module_segments, names_project, path_str};
use crate::extract::TOP_LEVEL;
use crate::model::FileCache;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

// `tested_by` is evidence, not an exhaustive index - a helper called by
// hundreds of tests would otherwise dominate the report
pub const MAX_TESTED_BY: usize = 25;

// file stems that stand for their directory rather than for themselves
const FACADE_STEMS: &[&str] = &["__init__", "index", "mod"];
// how many facades an import is chased through (`__main__` -> package
// `__init__` -> the module that defines the name)
const MAX_FACADE_HOPS: usize = 3;
// Path segments that name a position relative to the caller rather than
// another module: `self.x()`, `super::x()`, `crate::a::x()`. They are stripped
// before the external test, which is what keeps a Rust `mod tests` calling
// `super::parse` matched against the `parse` above it instead of read as a
// call leaving the project.
const RELATIVE_BASES: &[&str] = &["self", "Self", "this", "cls", "super", "crate"];
// Generic path segments, excluded so `src::foo` cannot name every file in the
// tree. Kept in step with the same list in `insights::build_graph`.
const GENERIC_DIRS: &[&str] = &[
    "src", "pkg", "internal", "cmd", "app", "lib", "test", "tests", "include",
];

// How a test reference was tied to the definition it covers. Declared
// strongest first, which is the order `tested_by` lists them in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Evidence {
    // the receiver's declared type addresses the method exactly
    ReceiverType,
    // the definition is in the file the test calls from
    SameFile,
    // the definition shares the caller's package scope: a go package or a c#
    // namespace spread over the files of one directory
    SamePackage,
    // the test's file imports the name from the defining file
    Import,
    // a qualifier segment names the defining file's module, type, stem or dir
    Qualifier,
    // untyped language, and the name has exactly one definition in the project
    NameOnly,
}

impl Evidence {
    pub fn label(self) -> &'static str {
        match self {
            Evidence::ReceiverType => "receiver-type",
            Evidence::SameFile => "same-file",
            Evidence::SamePackage => "same-package",
            Evidence::Import => "import",
            Evidence::Qualifier => "qualifier",
            Evidence::NameOnly => "name-only",
        }
    }
}

// A test function, addressed well enough for a runner to select it and for an
// editor to open it.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct TestSite {
    pub name: String,
    pub file: String,
    pub line: usize,
    pub language: &'static str,
}

// One test, and how it was tied to the definition it covers.
#[derive(Clone, Debug)]
pub struct TestRef {
    pub site: TestSite,
    pub evidence: Evidence,
}

// A definition: (index into `caches`, index into that file's `funcs`).
type DefId = (usize, usize);

pub struct CoverageIndex {
    covering: BTreeMap<DefId, Vec<TestRef>>,
    // definitions reached only from a test file's top level. Not a selectable
    // test, so it names nothing, but it is still a test reference - which is
    // what the `tested` flag has always keyed off.
    touched: BTreeSet<DefId>,
    // every distinct test function in the map, whether or not its calls
    // resolved: the denominator for "just run everything"
    total_tests: usize,
    // test calls dropped as leaving the project, reported so the gating is
    // auditable rather than silent
    external_calls: usize,
}

impl CoverageIndex {
    // The tests covering one definition, strongest evidence first, capped.
    pub fn covering(&self, def: DefId) -> &[TestRef] {
        self.covering.get(&def).map(|v| &v[..]).unwrap_or_default()
    }

    // Does any test reference this definition at all? True for a definition
    // reached only from file-level test setup, which names no test.
    pub fn is_covered(&self, def: DefId) -> bool {
        self.covering.contains_key(&def) || self.touched.contains(&def)
    }

    pub fn total_tests(&self) -> usize {
        self.total_tests
    }

    pub fn external_calls(&self) -> usize {
        self.external_calls
    }
}

// Every name a qualifier could use to reach a file: its stem, the modules it
// declares (go `package`, c++ `namespace`, rust `mod`), the types it defines,
// and its own directory names.
//
// Shared with `insights::build_graph` so a qualifier means the same thing to
// the call graph and to coverage.
pub(crate) fn file_aliases(caches: &[FileCache]) -> Vec<BTreeSet<String>> {
    caches
        .iter()
        .map(|c| {
            let mut set: BTreeSet<String> = BTreeSet::new();
            set.insert(stem_of(c).to_string());
            set.extend(c.modules.iter().cloned());
            set.extend(c.types.iter().map(|t| t.name.clone()));
            if let Some(parent) = c.rel_path.parent() {
                for comp in parent.components() {
                    if let Some(d) = comp.as_os_str().to_str() {
                        if !GENERIC_DIRS.contains(&d) {
                            set.insert(d.to_string());
                        }
                    }
                }
            }
            set.remove("");
            set
        })
        .collect()
}

// Per file: which name was imported from which files. Shared with
// `insights::build_graph` for the same reason as `file_aliases`.
pub(crate) fn imported_names(caches: &[FileCache]) -> Vec<BTreeMap<String, BTreeSet<usize>>> {
    let stems: Vec<&str> = caches.iter().map(stem_of).collect();
    let mut imported: Vec<BTreeMap<String, BTreeSet<usize>>> = vec![BTreeMap::new(); caches.len()];
    let mut stem_files: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, s) in stems.iter().enumerate() {
        stem_files.entry(s).or_default().push(i);
    }
    // a facade is imported under its directory
    for (i, c) in caches.iter().enumerate() {
        if !FACADE_STEMS.contains(&stems[i]) {
            continue;
        }
        if let Some(dir) = c
            .rel_path
            .parent()
            .and_then(Path::file_name)
            .and_then(|d| d.to_str())
        {
            stem_files.entry(dir).or_default().push(i);
        }
    }
    for (a, c) in caches.iter().enumerate() {
        let files_named = |s: &str| {
            stem_files
                .get(s)
                .into_iter()
                .flatten()
                .copied()
                .filter(|&b| b != a)
                .collect::<Vec<usize>>()
        };
        for imp in &c.imports {
            let segs: Vec<&str> = module_segments(&imp.module).collect();
            // The last segment names the module the statement actually reaches;
            // the ones before it are the packages on the way there. Preferring
            // it keeps `from mypkg.cli import main` pointed at `cli.py` even
            // when the package root defines a `main` of its own. Earlier
            // segments still get their say when the last matches nothing, which
            // is what carries a C++ `#include "foo/bar.h"` to `bar`.
            let mut targets: BTreeSet<usize> = segs
                .last()
                .map(|s| files_named(s))
                .unwrap_or_default()
                .into_iter()
                .collect();
            if targets.is_empty() {
                targets.extend(segs.iter().flat_map(|s| files_named(s)));
            }
            // `from pkg import cli` binds a name that is itself a module
            targets.extend(imp.names.iter().flat_map(|n| files_named(n)));
            for name in &imp.names {
                imported[a].entry(name.clone()).or_default().extend(&targets);
            }
            // An import that binds no names is not empty of meaning - it makes
            // a whole file's surface available instead of picking from it. A C
            // or C++ `#include` is the case that matters most, since the
            // language has no other import form. The single-candidate rule
            // still applies, so widening what is available cannot invent an
            // ambiguous match.
            if imp.names.is_empty() {
                for &b in &targets {
                    for f in &caches[b].funcs {
                        imported[a].entry(f.name.clone()).or_default().insert(b);
                    }
                }
            }
        }
    }
    // Chase each binding through the facades it passes: the name `__main__.py`
    // imported from `mypkg` is one `mypkg/__init__.py` imported from
    // `mypkg/cli.py`, and the definition is in the latter.
    for _ in 0..MAX_FACADE_HOPS {
        let mut grew = false;
        for a in 0..caches.len() {
            for name in imported[a].keys().cloned().collect::<Vec<String>>() {
                let hops: BTreeSet<usize> = imported[a][&name]
                    .iter()
                    .filter_map(|&b| imported[b].get(&name))
                    .flatten()
                    .copied()
                    .filter(|&b| b != a)
                    .collect();
                let reached = imported[a].entry(name).or_default();
                let before = reached.len();
                reached.extend(hops);
                grew |= reached.len() != before;
            }
        }
        if !grew {
            break;
        }
    }
    imported
}

fn stem_of(c: &FileCache) -> &str {
    c.rel_path.file_stem().and_then(|s| s.to_str()).unwrap_or("")
}

// Build the coverage relation. `project_ids` are the identities declared by
// manifests (crate name, go module path, npm name), so an integration test
// calling `mycrate::parse` is understood as staying inside the project.
pub fn build(caches: &[FileCache], project_ids: &BTreeSet<String>) -> CoverageIndex {
    let aliases = file_aliases(caches);
    let imported = imported_names(caches);
    let alias_any: BTreeSet<&str> = aliases.iter().flatten().map(String::as_str).collect();

    let mut by_name: BTreeMap<&str, Vec<DefId>> = BTreeMap::new();
    let mut by_file_name: BTreeMap<(usize, &str), Vec<DefId>> = BTreeMap::new();
    let mut by_owner: BTreeMap<(&str, &str), Vec<DefId>> = BTreeMap::new();
    // every type the project defines, so a receiver can be told apart from one
    // that belongs to the standard library or a dependency
    let mut project_types: BTreeSet<&str> = BTreeSet::new();
    for (fi, c) in caches.iter().enumerate() {
        for t in &c.types {
            project_types.insert(t.name.as_str());
        }
        for (ki, f) in c.funcs.iter().enumerate() {
            by_name.entry(f.name.as_str()).or_default().push((fi, ki));
            by_file_name
                .entry((fi, f.name.as_str()))
                .or_default()
                .push((fi, ki));
            if let Some(owner) = f.owner.as_deref() {
                project_types.insert(owner);
                by_owner
                    .entry((owner, f.name.as_str()))
                    .or_default()
                    .push((fi, ki));
            }
        }
    }

    let families: Vec<&'static str> = caches.iter().map(|c| c.language.family()).collect();
    let dirs: Vec<String> = caches
        .iter()
        .map(|c| {
            c.rel_path
                .parent()
                .map(path_str)
                .unwrap_or_default()
        })
        .collect();
    let mut hits: BTreeMap<DefId, BTreeMap<TestSite, Evidence>> = BTreeMap::new();
    let mut touched: BTreeSet<DefId> = BTreeSet::new();
    let mut tests: BTreeSet<(usize, &str)> = BTreeSet::new();
    let mut external_calls = 0usize;

    for (a, c) in caches.iter().enumerate() {
        let path = path_str(&c.rel_path);
        let file_is_test = is_test_path(&path);
        for call in &c.calls {
            if !(file_is_test || call.test_ctx || is_test_fn_name(&call.caller)) {
                continue;
            }
            let selectable = call.caller != TOP_LEVEL;
            if selectable {
                tests.insert((a, call.caller.as_str()));
            }
            let matched = resolve(
                Site {
                    file: a,
                    name: call.name.as_str(),
                    qualifier: call.qualifier.as_deref(),
                    recv_type: call.recv_type.as_deref(),
                    typed: c.language.is_typed(),
                    package_scoped: c.language.package_scoped(),
                },
                &Tables {
                    aliases: &aliases,
                    alias_any: &alias_any,
                    imported: &imported,
                    by_name: &by_name,
                    by_file_name: &by_file_name,
                    by_owner: &by_owner,
                    project_types: &project_types,
                    project_ids,
                    families: &families,
                    dirs: &dirs,
                },
            );
            let Some((defs, evidence)) = matched else {
                external_calls += 1;
                continue;
            };
            if defs.is_empty() {
                continue;
            }
            if !selectable {
                touched.extend(defs);
                continue;
            }
            // the line the test is defined on, not the line of the call
            let line = c
                .funcs
                .iter()
                .find(|f| f.name == call.caller)
                .map(|f| f.line)
                .unwrap_or(call.line);
            let site = TestSite {
                name: call.caller.clone(),
                file: path.clone(),
                line,
                language: c.language.as_str(),
            };
            for def in defs {
                // one test can reach a definition several ways; keep the
                // strongest, so a listed row never understates its own evidence
                let slot = hits.entry(def).or_default().entry(site.clone());
                slot.and_modify(|e| *e = (*e).min(evidence)).or_insert(evidence);
            }
        }
    }

    let covering = hits
        .into_iter()
        .map(|(def, sites)| {
            let mut refs: Vec<TestRef> = sites
                .into_iter()
                .map(|(site, evidence)| TestRef { site, evidence })
                .collect();
            refs.sort_by(|x, y| {
                (x.evidence, &x.site.file, &x.site.name).cmp(&(y.evidence, &y.site.file, &y.site.name))
            });
            refs.truncate(MAX_TESTED_BY);
            (def, refs)
        })
        .collect();

    CoverageIndex {
        covering,
        touched,
        total_tests: tests.len(),
        external_calls,
    }
}

struct Site<'a> {
    file: usize,
    name: &'a str,
    qualifier: Option<&'a str>,
    recv_type: Option<&'a str>,
    typed: bool,
    package_scoped: bool,
}

struct Tables<'a> {
    aliases: &'a [BTreeSet<String>],
    alias_any: &'a BTreeSet<&'a str>,
    imported: &'a [BTreeMap<String, BTreeSet<usize>>],
    by_name: &'a BTreeMap<&'a str, Vec<DefId>>,
    by_file_name: &'a BTreeMap<(usize, &'a str), Vec<DefId>>,
    by_owner: &'a BTreeMap<(&'a str, &'a str), Vec<DefId>>,
    project_types: &'a BTreeSet<&'a str>,
    project_ids: &'a BTreeSet<String>,
    families: &'a [&'static str],
    // the directory each file sits in, for package-scoped languages
    dirs: &'a [String],
}

// The definitions one test call covers, and the evidence that tied them.
//
// `None` means the call leaves the project: it covers nothing here and is
// counted rather than silently ignored. `Some(empty)` means it stays in the
// project but nothing here matched it - an unresolved internal call.
fn resolve(site: Site, t: &Tables) -> Option<(Vec<DefId>, Evidence)> {
    let family = t.families[site.file];
    let same_family = |defs: &[DefId]| -> Vec<DefId> {
        defs.iter()
            .copied()
            .filter(|&(b, _)| t.families[b] == family)
            .collect()
    };

    // The receiver's declared type addresses the method exactly. A receiver
    // the project never defines is something it got from elsewhere - an
    // `RwLock`, a `String` - and a method on it is not this project's function
    // however the name reads.
    if let Some(ty) = site.recv_type {
        if !t.project_types.contains(ty) {
            return None;
        }
        if let Some(defs) = t.by_owner.get(&(ty, site.name)) {
            let hits = same_family(defs);
            if !hits.is_empty() {
                return Some((hits, Evidence::ReceiverType));
            }
        }
        // a project type whose method is not indexed under it: fall through to
        // the weaker rules rather than claiming the call left the project
    }

    // What the qualifier names, once the parts that point back at the caller
    // are stripped. Nothing left means the call is effectively unqualified.
    let segs: Vec<&str> = site
        .qualifier
        .map(|q| {
            module_segments(q)
                .filter(|s| !RELATIVE_BASES.contains(s))
                .collect()
        })
        .unwrap_or_default();
    if !segs.is_empty() {
        let internal = segs.iter().any(|s| t.alias_any.contains(*s))
            || t.project_ids.iter().any(|id| {
                segs.iter().any(|s| *s == id) || site.qualifier.is_some_and(|q| names_project(q, id))
            });
        // `std::fs::write`, `tokio::spawn`: the qualifier names nothing in this
        // project, so whatever it calls is not defined here
        if !internal {
            return None;
        }
    }

    let candidates = t.by_name.get(site.name).map(|v| same_family(v)).unwrap_or_default();
    if candidates.is_empty() {
        return Some((Vec::new(), Evidence::NameOnly));
    }

    if segs.is_empty() {
        // an unqualified call next to the definition it names
        if let Some(defs) = t.by_file_name.get(&(site.file, site.name)) {
            let hits = same_family(defs);
            if !hits.is_empty() {
                return Some((hits, Evidence::SameFile));
            }
        }
        // a language whose package scope is the directory: the rest of the
        // package is in scope without an import, which is how a go test file
        // beside the code calls into it
        if site.package_scoped {
            let hits: Vec<DefId> = candidates
                .iter()
                .copied()
                .filter(|&(b, _)| t.dirs[b] == t.dirs[site.file])
                .collect();
            if !hits.is_empty() {
                return Some((hits, Evidence::SamePackage));
            }
        }
        // the name was imported from exactly one file that defines it
        let hits: Vec<DefId> = candidates
            .iter()
            .copied()
            .filter(|&(b, _)| {
                t.imported[site.file]
                    .get(site.name)
                    .is_some_and(|s| s.contains(&b))
            })
            .collect();
        if let [one] = hits[..] {
            return Some((vec![one], Evidence::Import));
        }
        // Untyped languages have nothing better than the name, so a name with
        // exactly one definition in the project is allowed to stand for it.
        // Typed languages do not get this: it is the rule that invented edges
        // for stdlib names colliding with project functions.
        if !site.typed {
            if let [one] = candidates[..] {
                return Some((vec![one], Evidence::NameOnly));
            }
        }
        return Some((Vec::new(), Evidence::NameOnly));
    }

    // a qualifier segment naming the file the definition lives in. More than
    // one file answering to it is ambiguous, and an absent link is better than
    // a wrong one.
    let hits: Vec<DefId> = candidates
        .iter()
        .copied()
        .filter(|&(b, _)| segs.iter().any(|s| t.aliases[b].contains(*s)))
        .collect();
    if let [one] = hits[..] {
        return Some((vec![one], Evidence::Qualifier));
    }
    Some((Vec::new(), Evidence::Qualifier))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::languages::Language;
    use crate::scan;
    use std::path::PathBuf;

    // a throwaway directory that cleans up on drop
    struct Dir(PathBuf);
    impl Dir {
        fn new(tag: &str) -> Dir {
            let p = std::env::temp_dir().join(format!("ccc-cov-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn caches(label: &str, files: &[(&str, &str)]) -> (Dir, Vec<FileCache>) {
        let dir = Dir::new(label);
        for (name, body) in files {
            let to = dir.0.join(name);
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::write(&to, body).unwrap();
        }
        let found = scan::collect_files(&dir.0).unwrap();
        let caches = scan::build_caches(&dir.0, &found);
        (dir, caches)
    }

    fn index(caches: &[FileCache]) -> CoverageIndex {
        build(caches, &BTreeSet::new())
    }

    fn def(caches: &[FileCache], file: &str, name: &str) -> (usize, usize) {
        let fi = caches
            .iter()
            .position(|c| path_str(&c.rel_path) == file)
            .unwrap_or_else(|| panic!("no cache for {file}"));
        let ki = caches[fi]
            .funcs
            .iter()
            .position(|f| f.name == name)
            .unwrap_or_else(|| panic!("no fn {name} in {file}"));
        (fi, ki)
    }

    // the reported bug: a Rust test writing a fixture must not be presented as
    // covering a TypeScript method that happens to be called `write`
    #[test]
    fn a_stdlib_call_in_a_rust_test_does_not_cover_a_typescript_method() {
        let (_dir, caches) = caches("a_stdlib_call_in_a_rust_test_does_not_cover_a_typescript_method", &[
            (
                "src/insights.rs",
                "#[cfg(test)]\nmod tests {\n    #[test]\n    fn triggers_follow_the_diff() {\n        std::fs::write(&p, \"x\").unwrap();\n    }\n}\n",
            ),
            (
                "ext/log.ts",
                "export class Log {\n  private write(kind: string, message: string): void {\n    this.channel.appendLine(message);\n  }\n}\n",
            ),
        ]);
        let idx = index(&caches);
        assert!(!idx.is_covered(def(&caches, "ext/log.ts", "write")));
        assert!(idx.external_calls() >= 1, "the std::fs::write call should be counted as external");
    }

    // the same call must not attach to a same-language function either: it
    // leaves the project, so it covers nothing at all
    #[test]
    fn an_external_qualifier_covers_nothing_in_its_own_language() {
        let (_dir, caches) = caches("an_external_qualifier_covers_nothing_in_its_own_language", &[(
            "src/io.rs",
            "pub fn write(p: &str) {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn writes() {\n        std::fs::write(\"a\", \"b\").unwrap();\n    }\n}\n",
        )]);
        let idx = index(&caches);
        assert!(!idx.is_covered(def(&caches, "src/io.rs", "write")));
    }

    // a receiver the project never defines is not this project's method
    #[test]
    fn a_lock_guard_is_not_a_covered_method() {
        let (_dir, caches) = caches("a_lock_guard_is_not_a_covered_method", &[(
            "src/serve.rs",
            "pub struct Map;\nimpl Map {\n    pub fn write(&self) {}\n}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn opt_in() {\n        let state: std::sync::RwLock<u8> = Default::default();\n        state.write().unwrap();\n    }\n}\n",
        )]);
        let idx = index(&caches);
        assert!(!idx.is_covered(def(&caches, "src/serve.rs", "write")));
    }

    // the dominant genuine case: `mod tests` calling the function above it
    #[test]
    fn a_test_module_covers_the_function_beside_it() {
        let (_dir, caches) = caches("a_test_module_covers_the_function_beside_it", &[(
            "src/parse.rs",
            "pub fn parse(s: &str) -> usize { s.len() }\n#[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn parses_headers() {\n        assert_eq!(parse(\"ab\"), 2);\n    }\n}\n",
        )]);
        let idx = index(&caches);
        let refs = idx.covering(def(&caches, "src/parse.rs", "parse"));
        assert_eq!(refs.len(), 1, "expected one covering test, got {refs:?}");
        assert_eq!(refs[0].site.name, "parses_headers");
        assert_eq!(refs[0].evidence, Evidence::SameFile);
    }

    // `super::parse(..)` names the function above it, not another project
    #[test]
    fn a_relative_qualifier_is_not_external() {
        let (_dir, caches) = caches("a_relative_qualifier_is_not_external", &[(
            "src/parse.rs",
            "pub fn parse(s: &str) -> usize { s.len() }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn parses() {\n        assert_eq!(super::parse(\"ab\"), 2);\n    }\n}\n",
        )]);
        let idx = index(&caches);
        assert!(idx.is_covered(def(&caches, "src/parse.rs", "parse")));
    }

    // a qualifier naming the defining file resolves across files
    #[test]
    fn a_qualifier_naming_the_defining_file_covers_it() {
        let (_dir, caches) = caches("a_qualifier_naming_the_defining_file_covers_it", &[
            ("src/parse.rs", "pub fn parse(s: &str) -> usize { s.len() }\n"),
            (
                "tests/parse_test.rs",
                "#[test]\nfn parses() {\n    assert_eq!(parse::parse(\"ab\"), 2);\n}\n",
            ),
        ]);
        let idx = index(&caches);
        let refs = idx.covering(def(&caches, "src/parse.rs", "parse"));
        assert_eq!(refs.len(), 1, "expected one covering test, got {refs:?}");
        assert_eq!(refs[0].evidence, Evidence::Qualifier);
    }

    // two same-named functions in one family, one qualifier: no link at all
    #[test]
    fn ambiguous_evidence_covers_nothing() {
        let (_dir, caches) = caches("ambiguous_evidence_covers_nothing", &[
            ("src/a/parse.rs", "pub fn run() {}\n"),
            ("src/b/parse.rs", "pub fn run() {}\n"),
            (
                "tests/run_test.rs",
                "#[test]\nfn runs() {\n    parse::run();\n}\n",
            ),
        ]);
        let idx = index(&caches);
        assert!(!idx.is_covered(def(&caches, "src/a/parse.rs", "run")));
        assert!(!idx.is_covered(def(&caches, "src/b/parse.rs", "run")));
    }

    // untyped languages keep the single-definer fallback, labelled as such
    #[test]
    fn an_untyped_language_keeps_the_single_definer_fallback() {
        let (_dir, caches) = caches("an_untyped_language_keeps_the_single_definer_fallback", &[
            ("app/money.py", "def charge(n):\n    return n\n"),
            ("tests/test_money.py", "def test_charge():\n    assert charge(1) == 1\n"),
        ]);
        let idx = index(&caches);
        let refs = idx.covering(def(&caches, "app/money.py", "charge"));
        assert_eq!(refs.len(), 1, "expected one covering test, got {refs:?}");
        assert_eq!(refs[0].evidence, Evidence::NameOnly);
    }

    // a typed language does not: a bare name is not evidence there
    #[test]
    fn a_typed_language_gets_no_name_only_fallback() {
        let (_dir, caches) = caches("a_typed_language_gets_no_name_only_fallback", &[
            ("src/money.go", "package money\n\nfunc Charge(n int) int { return n }\n"),
            (
                "other/thing_test.go",
                "package other\n\nfunc TestCharge(t *testing.T) {\n\tCharge(1)\n}\n",
            ),
        ]);
        let idx = index(&caches);
        assert!(!idx.is_covered(def(&caches, "src/money.go", "Charge")));
    }

    // file-level setup in a test file is a reference, but names no test
    #[test]
    fn file_level_setup_marks_tested_without_naming_a_test() {
        let (_dir, caches) = caches("file_level_setup_marks_tested_without_naming_a_test", &[
            ("app/money.py", "def charge(n):\n    return n\n"),
            ("tests/test_money.py", "charge(1)\n"),
        ]);
        let idx = index(&caches);
        let d = def(&caches, "app/money.py", "charge");
        assert!(idx.is_covered(d));
        assert!(idx.covering(d).is_empty());
    }

    // Measurement, not an assertion: the old bare-name join against the new
    // one over this repository. Run with
    // `cargo test coverage_delta -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn coverage_delta() {
        let root = std::path::Path::new(".");
        let files = scan::collect_files(root).unwrap();
        let caches = scan::build_caches(root, &files);

        // the old relation: callee bare name -> test-context callers, global
        let mut test_callers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for c in &caches {
            let path = path_str(&c.rel_path);
            let file_is_test = is_test_path(&path);
            for call in &c.calls {
                if file_is_test || call.test_ctx || is_test_fn_name(&call.caller) {
                    test_callers
                        .entry(call.name.clone())
                        .or_default()
                        .insert(call.caller.clone());
                }
            }
        }
        let cov = build(&caches, &BTreeSet::new());

        let (mut before, mut after, mut total) = (0usize, 0usize, 0usize);
        let mut lost: Vec<(String, String, usize)> = Vec::new();
        let mut gained: Vec<(String, String)> = Vec::new();
        for (fi, c) in caches.iter().enumerate() {
            let path = path_str(&c.rel_path);
            for (ki, f) in c.funcs.iter().enumerate() {
                total += 1;
                let old = test_callers.contains_key(&f.name);
                let new = cov.is_covered((fi, ki));
                before += old as usize;
                after += new as usize;
                if old && !new {
                    lost.push((path.clone(), f.name.clone(), test_callers[&f.name].len()));
                }
                if new && !old {
                    gained.push((path.clone(), f.name.clone()));
                }
            }
        }
        println!("functions in map: {total}");
        println!("covered before (bare name): {before}");
        println!("covered after  (evidenced): {after}");
        println!("external test calls dropped: {}", cov.external_calls());
        println!("distinct tests in map: {}", cov.total_tests());
        println!("\nno longer covered: {}", lost.len());
        lost.sort_by(|a, b| b.2.cmp(&a.2));
        for (file, name, n) in lost.iter().take(20) {
            println!("  {file}::{name} (claimed {n} before)");
        }
        println!("\nnewly covered: {}", gained.len());
        for (file, name) in gained.iter().take(10) {
            println!("  {file}::{name}");
        }
        for (fi, c) in caches.iter().enumerate() {
            if path_str(&c.rel_path) != "extensions/vscode/src/log.ts" {
                continue;
            }
            for (ki, f) in c.funcs.iter().enumerate() {
                if f.name == "write" {
                    println!(
                        "\nlog.ts::write  before={} tests, after={} tests",
                        test_callers.get("write").map(|s| s.len()).unwrap_or(0),
                        cov.covering((fi, ki)).len()
                    );
                }
            }
        }
    }

    // C and C++ are one runtime family; Rust and TypeScript are not
    #[test]
    fn families_bridge_c_and_cpp_only() {
        assert_eq!(Language::C.family(), Language::Cpp.family());
        assert_eq!(Language::JavaScript.family(), Language::TypeScript.family());
        assert_ne!(Language::Rust.family(), Language::TypeScript.family());
        assert_ne!(Language::Go.family(), Language::Rust.family());
    }
}
