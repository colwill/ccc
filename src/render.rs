//! rendering of `FileCache` entries and the `CCC.md` index to markdown per the
//! ContextCodeCache spec in PLAN.md

use crate::model::{Counts, FileCache};
use std::fmt::Write as _;
use std::path::Path;

// current UTC timestamp formatted as `yyyymmdd-hh-mm-ss`
pub fn now_ts() -> String {
    chrono::Utc::now().format("%Y%m%d-%H-%M-%S").to_string()
}

// render a single per-file cache entry
pub fn render_file(fc: &FileCache, ts: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# {} ({}) UTC", fc.display_name, ts);
    let _ = writeln!(out, "# source: {} [{}]", fc.rel_path.display(), fc.language.as_str());

    let _ = writeln!(out, "# const");
    for c in &fc.consts {
        match &c.ty {
            Some(ty) => {
                let _ = writeln!(out, "    - L{}@{}:{}", c.line, c.name, ty);
            }
            None => {
                let _ = writeln!(out, "    - L{}@{}", c.line, c.name);
            }
        }
    }

    let _ = writeln!(out, "# funcs");
    for f in &fc.funcs {
        let ret = match &f.ret {
            Some(r) => format!(":{}", r),
            None => String::new(),
        };
        let comment = match &f.comment {
            Some(c) => format!(" // {}", c),
            None => String::new(),
        };
        let _ = writeln!(
            out,
            "    - L{}:{}@{}{}{}",
            f.line, f.col, f.name, ret, comment
        );
    }

    let _ = writeln!(out, "# refs");
    for r in &fc.refs {
        let ret = match &r.target_ret {
            Some(t) => format!(":{}", t),
            None => String::new(),
        };
        let _ = writeln!(
            out,
            "    - {}@L{} calls L{}:{}@{}{}",
            r.caller, r.call_line, r.target_line, r.target_col, r.target_name, ret
        );
    }

    let _ = writeln!(out, "# note");
    for n in &fc.notes {
        let _ = writeln!(out, "    - @L{} {}", n.line, n.text);
    }

    out
}

// render the CCC index for the whole project
pub fn render_index(root: &Path, caches: &[FileCache], ts: &str) -> String {
    let mut out = String::new();
    let root_label = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_else(|| root.to_str().unwrap_or("."));

    let mut totals = Counts::default();
    for c in caches {
        totals.add(c.counts());
    }

    // agent guide kept at the very top in metadata
    let _ = writeln!(out, "---");
    let _ = writeln!(out, "ContextCodeCache - agent guide");
    let _ = writeln!(out);
    let _ = writeln!(out, "what:  a GENERATED map of this project's source. Each source file has a");
    let _ = writeln!(out, "        `<module>-<file>.<ext>.md` entry listing its constants, functions");
    let _ = writeln!(out, "        (L<line>:<col>@name:return), intra-file call graph (refs), and");
    let _ = writeln!(out, "        marker notes (TODO/FIXME/...). See the `# files` list below.");
    let _ = writeln!(out, "why:   lets agents orient in the codebase cheaply - skim `.ccc` first to");
    let _ = writeln!(out, "        find where things live, then open the real source for detail.");
    let _ = writeln!(out, "        `tokens.bin`/`tokens.json`, if present, hold this content pre-encoded");
    let _ = writeln!(out, "        as APPROXIMATE tiktoken (o200k) ids - for a downstream model that");
    let _ = writeln!(out, "        shares that vocabulary, NOT for Claude (different tokenizer; its API");
    let _ = writeln!(out, "        takes text, not token ids). Feed Claude the markdown above as text.");
    let _ = writeln!(out, "query: `ccc serve` exposes this map over local HTTP - REST endpoints");
    let _ = writeln!(out, "        (/find /references /dependencies /file /notes) plus an MCP");
    let _ = writeln!(out, "        endpoint at /mcp - so agents can query instead of reading files.");
    let _ = writeln!(out, "keep-fresh: whenever you change tracked source, regenerate with");
    let _ = writeln!(out, "        `ccc scan` (add `--tokens` to refresh the token stream). CI runs");
    let _ = writeln!(out, "        `ccc check`, which fails when `.ccc` is out of date.");
    let _ = writeln!(out, "do-not-edit: never hand-edit files under `.ccc` - they are overwritten on");
    let _ = writeln!(out, "        the next scan. To change the cache, change the source, then rescan.");
    let _ = writeln!(out, "---");
    let _ = writeln!(out);

    let _ = writeln!(out, "# ContextCodeCache ({}) UTC", ts);
    let _ = writeln!(out, "### project: {}", root_label);
    let _ = writeln!(
        out,
        "### totals: {} files, {} funcs, {} consts, {} refs, {} notes",
        caches.len(),
        totals.funcs,
        totals.consts,
        totals.refs,
        totals.notes
    );
    let _ = writeln!(out, "### regenerate: `ccc scan`");
    let _ = writeln!(out, "### files");
    for c in caches {
        let n = c.counts();
        let _ = writeln!(
            out,
            "    - [{}]({}) [{}] {}f/{}c/{}r/{}n",
            c.rel_path.display(),
            c.cache_name,
            c.language.as_str(),
            n.funcs,
            n.consts,
            n.refs,
            n.notes
        );
    }

    out
}

// replace embedded generation timestamps with a fixed token so freshness
// checks compare content not wall-clock time
pub fn strip_timestamps(s: &str) -> String {
    s.lines().map(strip_ts_line).collect::<Vec<_>>().join("\n")
}

fn strip_ts_line(line: &str) -> String {
    if let Some(idx) = line.rfind(") UTC") {
        if let Some(start) = line[..idx].rfind(" (") {
            let mut out = String::with_capacity(line.len());
            out.push_str(&line[..start]);
            out.push_str(" (TS) UTC");
            out.push_str(&line[idx + ") UTC".len()..]);
            return out;
        }
    }
    line.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_normalized_away() {
        let a = "# f.rs.md (20260701-10-45-37) UTC\nbody line";
        let b = "# f.rs.md (20990101-00-00-00) UTC\nbody line";
        assert_eq!(strip_timestamps(a), strip_timestamps(b));
        // non-header lines are untouched
        assert!(strip_timestamps(a).contains("body line"));
    }
}
