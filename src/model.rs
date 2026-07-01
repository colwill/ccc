//! data model for extracted symbols and a whole-file cache entry.

use crate::languages::Language;
use std::path::PathBuf;

/// module/file-level constant or static binding.
#[derive(Debug, Clone)]
pub struct Const {
    pub line: usize, // 1-based
    pub name: String,
    pub ty: Option<String>,
}

/// function / method definition.
#[derive(Debug, Clone)]
pub struct Func {
    pub line: usize, // 1-based (position of the name token)
    pub col: usize,  // 1-based
    pub name: String,
    pub ret: Option<String>,
    pub comment: Option<String>, // preceding doc / inline comment, one line
}

/// resolved call: `caller` (at `call_line`) invokes a function defined in the
/// same file at (`target_line`, `target_col`).
#[derive(Debug, Clone)]
pub struct Ref {
    pub caller: String,
    pub call_line: usize,
    pub target_line: usize,
    pub target_col: usize,
    pub target_name: String,
    pub target_ret: Option<String>,
}

/// A free-form note (TODO/FIXME/NOTE/...).
#[derive(Debug, Clone)]
pub struct Note {
    pub line: usize,
    pub text: String,
}

/// everything extracted from a single source file.
#[derive(Debug, Clone)]
pub struct FileCache {
    /// Path relative to the project root, e.g. `src/extract.rs`.
    pub rel_path: PathBuf,
    pub cache_name: String,
    pub display_name: String,
    pub language: Language,
    pub consts: Vec<Const>,
    pub funcs: Vec<Func>,
    pub refs: Vec<Ref>,
    pub notes: Vec<Note>,
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
