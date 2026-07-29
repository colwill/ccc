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

// function / method definition.
#[derive(Debug, Clone)]
pub struct Func {
    pub line: usize, // 1-based (position of the name token)
    pub col: usize,  // 1-based
    pub name: String,
    pub ret: Option<String>,
    pub comment: Option<String>, // preceding doc / inline comment, one line
    // full definition span (1-based, inclusive) - used by `surf` to map diff
    // hunks onto functions; not rendered into `.ccc` entries
    pub start_line: usize,
    pub end_line: usize,
    // true when defined inside a test scope (e.g. a Rust `mod tests`)
    pub test_ctx: bool,
}

// a call site kept in "loose" form: every call in the file, resolved or not.
// `surf` matches these against other services' definitions; they are not
// rendered into `.ccc` entries.
#[derive(Debug, Clone)]
pub struct CallSite {
    // nearest enclosing function (`<top>` at file level)
    pub caller: String,
    pub line: usize,
    // rightmost identifier of the callee (`billing::charge` -> `charge`)
    pub name: String,
    // qualifier text left of the name, if any (`billing::charge` -> `billing`,
    // `client.charge()` -> `client`)
    pub qualifier: Option<String>,
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

// one import/use/include statement, in loose textual form:
// `use crate::model::{CallSite, Const}` -> module "crate::model",
// names ["CallSite", "Const"]; `from a.b import c as d` -> module "a.b",
// names ["c", "d"]. Used by `dependencies` to resolve file-level edges
// (including type-only imports the call map cannot see); not rendered.
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
    pub consts: Vec<Const>,
    pub funcs: Vec<Func>,
    pub refs: Vec<Ref>,
    pub notes: Vec<Note>,
    // all call sites (superset of `refs`), used by `surf`; not rendered
    pub calls: Vec<CallSite>,
    // qualified constant-like value usages that are not calls (enum variants,
    // module consts, scoped types: `Encoding::O200kBase`, `http.StatusOK`),
    // served by `references`/`find`; not rendered
    pub uses: Vec<CallSite>,
    // import/use/include statements, used by `dependencies`; not rendered
    pub imports: Vec<Import>,
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
