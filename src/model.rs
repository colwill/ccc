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

// A free-form note (TODO/FIXME/NOTE/...).
#[derive(Debug, Clone)]
pub struct Note {
    pub line: usize,
    pub text: String,
}

// one import/use/include statement, in loose form
#[derive(Debug, Clone)]
pub struct Import {
    pub line: usize,
    pub module: String,
    pub names: Vec<String>,
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
}

impl FileCache {
    pub fn counts(&self) -> Counts {
        Counts {
            funcs: self.funcs.len(),
            consts: self.consts.len(),
            refs: self.refs.len(),
            notes: self.notes.len(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Counts {
    pub funcs: usize,
    pub consts: usize,
    pub refs: usize,
    pub notes: usize,
}

impl Counts {
    pub fn add(&mut self, other: Counts) {
        self.funcs += other.funcs;
        self.consts += other.consts;
        self.refs += other.refs;
        self.notes += other.notes;
    }
}
