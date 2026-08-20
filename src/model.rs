//! data model for extracted symbols and a whole-file cache entry.

use crate::languages::Language;
use std::path::PathBuf;

// module/file-level constant or static binding.
#[derive(Debug, Clone)]
pub struct Const {
    pub line: usize, // 1-based
    pub name: String,
    pub ty: Option<String>,
}

// named type definition `struct`/`enum`/`class`/`interface`/`trait`/alias.
#[derive(Debug, Clone)]
pub struct TypeDef {
    pub line: usize,
    pub name: String,
    pub kind: String, // struct | enum | class | interface | trait | alias | union | protocol
}

#[derive(Debug, Clone)]
pub struct LoopInfo {
    pub line: usize,
    pub kind: String, // for | while | do | loop | comprehension
    pub depth: usize,
    pub trip: Option<usize>,
}

// a call that acquires or releases a resource, for the leak heuristic.
#[derive(Debug, Clone)]
pub struct ResourceOp {
    pub line: usize,
    pub name: String,
    pub pair: &'static str,
    pub acquire: bool,
    // inside a `with`/`defer`, so the release is automatic
    pub guarded: bool,
}

// structural measurements of one function body.
#[derive(Debug, Clone, Default)]
pub struct FuncMetrics {
    pub body_lines: usize,
    pub params: usize,
    pub branches: usize,
    pub nodes: usize,
    pub loops: Vec<LoopInfo>,
    pub resources: Vec<ResourceOp>,
    // the function calls itself by name
    pub recursive: bool,
}

impl FuncMetrics {
    // cyclomatic-style score: one path, plus one per decision point and loop
    pub fn complexity(&self) -> usize {
        1 + self.branches + self.loops.len()
    }

    pub fn max_loop_depth(&self) -> usize {
        self.loops.iter().map(|l| l.depth).max().unwrap_or(0)
    }

    // `complexity` on a 1-10 scale, for anything that has to *show* it rather
    // than rank by it, the usual cyclomatic risk advice
    pub fn complexity_score(&self) -> u8 {
        match self.complexity() {
            0..=1 => 1,
            2 => 2,
            3 => 3,
            4..=5 => 4,
            6..=7 => 5,
            8..=10 => 6,
            11..=15 => 7,
            16..=20 => 8,
            21..=30 => 9,
            _ => 10,
        }
    }
}

// function / method definition.
#[derive(Debug, Clone)]
pub struct Func {
    pub line: usize, // 1-based (position of the name token)
    pub col: usize,  // 1-based
    pub name: String,
    pub ret: Option<String>,
    pub comment: Option<String>, // preceding doc / inline comment, one line
    pub start_line: usize,
    pub end_line: usize,
    // true when defined inside a test scope (e.g. a Rust `mod tests`)
    pub test_ctx: bool,
    // the type this is a method of, when it is one (Rust `impl T`, a Go
    // receiver, a C++/TS class). Together with `name` this addresses a method
    // precisely enough to resolve a call through its receiver's type.
    pub owner: Option<String>,
    pub param_types: Vec<String>,
    pub metrics: FuncMetrics,
}

// a call site kept in "loose" form: every call in the file, resolved or not.
#[derive(Debug, Clone)]
pub struct CallSite {
    // nearest enclosing function (`<top>` at file level)
    pub caller: String,
    pub line: usize,
    // rightmost identifier of the callee (`billing::charge` -> `charge`)
    pub name: String,
    pub qualifier: Option<String>,
    pub recv_type: Option<String>,
    // true when the call sits inside a test scope (e.g. a Rust `mod tests`)
    pub test_ctx: bool,
}

// resolved call: `caller` (at `call_line`) invokes a function defined in the
// same file at (`target_line`, `target_col`).
#[derive(Debug, Clone)]
pub struct Ref {
    pub caller: String,
    pub call_line: usize,
    pub target_line: usize,
    pub target_col: usize,
    pub target_name: String,
    pub target_ret: Option<String>,
}

// A free-form "note" (TODO/FIXME/NOTE/...).
#[derive(Debug, Clone)]
pub struct Note {
    pub line: usize,
    pub text: String,
}

// Which side of a boundary an annotation describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Boundary {
    // this function handles the key: `ccc:serves grpc billing.v1.Charge`
    Serves,
    // this function reaches out to the key: `ccc:calls grpc billing.v1.Charge`
    Calls,
}

impl Boundary {
    pub fn label(self) -> &'static str {
        match self {
            Boundary::Serves => "serves",
            Boundary::Calls => "calls",
        }
    }
}

// An author-written hint that a call leaves this process
#[derive(Debug, Clone)]
pub struct Annotation {
    pub line: usize,
    pub boundary: Boundary,
    // grpc | rest | http | graphql | queue | event | webhook | ffi | cli, or
    // "unspecified" when the author named only a key
    pub transport: String,
    // the rendezvous key, matched verbatim against the other end
    pub key: String,
    // the function it was attached to, or `<top>` for a file-level hint
    pub function: String,
}

// one import/use/include statement, in loose form
#[derive(Debug, Clone)]
pub struct Import {
    pub line: usize,
    pub module: String,
    pub names: Vec<String>,
    pub reexport: bool,
}

// everything extracted from a single source file.
#[derive(Debug, Clone)]
pub struct FileCache {
    // Path relative to the project root, e.g. `src/extract.rs`.
    pub rel_path: PathBuf,
    pub cache_name: String,
    pub display_name: String,
    pub language: Language,
    // total lines in the source file, for project-size reporting
    pub lines: usize,
    pub consts: Vec<Const>,
    pub funcs: Vec<Func>,
    pub refs: Vec<Ref>,
    pub notes: Vec<Note>,
    // all call sites (superset of `refs`), used by `changes`; not rendered
    pub calls: Vec<CallSite>,
    // qualified constant-like value usages that are not calls
    pub uses: Vec<CallSite>,
    pub imports: Vec<Import>,
    pub types: Vec<TypeDef>,
    pub modules: Vec<String>,
    // `ccc:serves` / `ccc:calls` hints written in comments
    pub annotations: Vec<Annotation>,
}

impl FileCache {
    pub fn counts(&self) -> Counts {
        Counts {
            funcs: self.funcs.len(),
            consts: self.consts.len(),
            refs: self.refs.len(),
            notes: self.notes.len(),
            mods: self.modules.len(),
            reexports: self
                .imports
                .iter()
                .filter(|i| i.reexport)
                .map(|i| i.names.len().max(1))
                .sum(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics(branches: usize, loops: usize) -> FuncMetrics {
        FuncMetrics {
            branches,
            loops: (0..loops)
                .map(|_| LoopInfo { line: 1, kind: "for".into(), depth: 1, trip: None })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn the_complexity_band_spans_one_to_ten_and_never_leaves_the_range() {
        // a straight-line function is the floor, not zero: there is always one
        // path through it
        assert_eq!(metrics(0, 0).complexity(), 1);
        assert_eq!(metrics(0, 0).complexity_score(), 1);
        // the ordinary range keeps its resolution rather than collapsing
        assert_eq!(metrics(1, 0).complexity_score(), 2);
        assert_eq!(metrics(2, 0).complexity_score(), 3);
        assert_eq!(metrics(4, 0).complexity_score(), 4);
        assert_eq!(metrics(6, 0).complexity_score(), 5);
        assert_eq!(metrics(9, 0).complexity_score(), 6);
        // 11-20 is the "worth a second look" band
        assert_eq!(metrics(10, 0).complexity_score(), 7);
        assert_eq!(metrics(15, 0).complexity_score(), 8);
        // and 21+ is where it stops being testable in one sitting
        assert_eq!(metrics(20, 0).complexity_score(), 9);
        assert_eq!(metrics(30, 0).complexity_score(), 10);
        assert_eq!(metrics(4_000, 0).complexity_score(), 10);
        // loops count toward the same score as branches do
        assert_eq!(metrics(2, 2).complexity(), 5);
        assert_eq!(metrics(2, 2).complexity_score(), 4);
        // monotonic, and never outside 1..=10
        let mut last = 0;
        for b in 0..200 {
            let s = metrics(b, 0).complexity_score();
            assert!((1..=10).contains(&s), "{b} branches scored {s}");
            assert!(s >= last, "score went backwards at {b} branches");
            last = s;
        }
    }
}

// The per-file tally the index reports
#[derive(Debug, Clone, Copy, Default)]
pub struct Counts {
    pub funcs: usize,
    pub consts: usize,
    pub refs: usize,
    pub notes: usize,
    // submodules/namespaces this file declares (Rust `mod x;`, a Go package
    // clause, a TS `namespace`)
    pub mods: usize,
    // bindings this file re-exports rather than consumes (Rust `pub use`)
    pub reexports: usize,
}

impl Counts {
    pub fn add(&mut self, other: Counts) {
        self.funcs += other.funcs;
        self.consts += other.consts;
        self.refs += other.refs;
        self.notes += other.notes;
        self.mods += other.mods;
        self.reexports += other.reexports;
    }
}
