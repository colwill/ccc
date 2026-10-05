//! `ccc pr-summary` - turn `prompts`'s attribution into detail a PR reviewer
//! can read start to finish

use crate::prompts::PromptsReport;

// files per page; each entry can carry two code blocks
pub const DEFAULT_LIMIT: usize = 15;

// the same "say why" shape every other analysis tool uses
pub fn unavailable(reason: &str) -> String {
    format!(
        "# pr summary\nunavailable: {reason}\n\n\
         The change set diffs the branch against its base. In CI, fetch history \
         (actions/checkout with fetch-depth: 0); locally, make sure the branch \
         has an upstream such as origin/main.\n"
    )
}

// a section per changed file, the request(s) behind it as detail rather than
// an evidence citation
pub fn markdown(report: &PromptsReport, limit: Option<usize>, offset: usize) -> String {
    let mut out = format!(
        "# pr summary\n{} request(s) behind this branch's changes against {} - {} file(s) explained, {} not\n",
        report.turns.len(),
        report.base,
        report.counts.attributed_files,
        report.counts.unattributed_files,
    );
    let secrets: Vec<serde_json::Value> = report.secrets.iter().filter_map(|f| serde_json::to_value(f).ok()).collect();
    out.push_str(&crate::serve::md_secrets(&secrets));

    if report.turns.is_empty() {
        out.push_str(
            "\nno local claude or copilot records name this project. \
             The agent may not have run here, or its history has rotated.\n",
        );
        return out;
    }

    let total = report.attributed.len();
    let start = offset.min(total);
    let end = limit.map_or(total, |n| (start + n).min(total));
    let window = report.attributed.iter().skip(start).take(end - start);

    for (path, refs) in window {
        out.push_str(&format!("\n## {path}\n"));
        for r in refs {
            let span = r.lines.map(|[s, e]| format!(" L{s}-{e}")).unwrap_or_default();
            out.push_str(&format!("\n- **{}**{span} ({}): {}\n", r.agent, r.evidence, r.prompt));
            match (&r.proposed, &r.agreed) {
                (Some(proposed), Some(agreed)) if proposed == agreed => {
                    out.push_str("\n  kept as proposed:\n");
                    out.push_str(&fence(path, proposed));
                }
                (Some(proposed), Some(agreed)) => {
                    out.push_str("\n  proposed:\n");
                    out.push_str(&fence(path, proposed));
                    out.push_str("\n  agreed (current):\n");
                    out.push_str(&fence(path, agreed));
                }
                _ => out.push_str(&format!(
                    "\n  _{}_\n",
                    match r.evidence.as_str() {
                        "tool-edit" => "the request named this file, but ccc could not pin the exact span it wrote - nothing to compare",
                        "temporal" => "only the request's timing places it here - a guess, not a citation",
                        _ => "no span pinned - nothing to compare",
                    }
                )),
            }
        }
    }

    if end < total {
        out.push_str(&format!(
            "\nshowing files {}-{} of {total}; pass offset={end} for the rest\n",
            start + 1,
            end
        ));
    }

    if !report.unattributed.is_empty() {
        out.push_str("\n## not explained by any request\n");
        out.push_str(
            "written by hand, or by an agent whose record ccc cannot read:\n",
        );
        for path in &report.unattributed {
            out.push_str(&format!("- {path}\n"));
        }
    }

    out
}

fn fence(path: &str, text: &str) -> String {
    format!("  ```{}\n  {}\n  ```\n", lang_of(path), text.replace('\n', "\n  "))
}

// enough to make the common languages readable in a PR description; an
// unrecognised extension just renders as a plain fenced block
fn lang_of(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "jsx",
        "ts" => "typescript",
        "tsx" => "tsx",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "rb" => "ruby",
        "php" => "php",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "swift" => "swift",
        "sh" | "bash" => "bash",
        "sql" => "sql",
        "yaml" | "yml" => "yaml",
        "json" => "json",
        "toml" => "toml",
        "md" => "markdown",
        "html" => "html",
        "css" | "scss" => "css",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompts::{PromptRef, PromptsCounts, Turn};
    use std::collections::BTreeMap;

    fn report(attributed: BTreeMap<String, Vec<PromptRef>>, unattributed: Vec<String>) -> PromptsReport {
        PromptsReport {
            schema: crate::prompts::SCHEMA,
            root: ".".into(),
            base: "origin/main".into(),
            turns: vec![Turn {
                id: "claude:s:1".into(),
                agent: "claude".into(),
                session: "s".into(),
                model: Some("claude-sonnet-5".into()),
                ts: "2026-09-27T00:00:00Z".into(),
                branch: None,
                prompt: "add a retry to the charge call".into(),
                edits: vec![],
                changesets: vec![],
                epoch: 0,
                beats: vec![],
            }],
            counts: PromptsCounts {
                turns: 1,
                edits: 1,
                attributed_files: attributed.len(),
                unattributed_files: unattributed.len(),
                claude_turns: 1,
                copilot_turns: 0,
            },
            attributed,
            unattributed,
            sources: vec![],
            secrets: vec![],
        }
    }

    fn pinned_ref(proposed: &str, agreed: &str) -> PromptRef {
        PromptRef {
            turn: "claude:s:1".into(),
            agent: "claude".into(),
            ts: "2026-09-27T00:00:00Z".into(),
            prompt: "add a retry to the charge call".into(),
            evidence: "content-match".into(),
            lines: Some([42, 44]),
            proposed: Some(proposed.into()),
            agreed: Some(agreed.into()),
        }
    }

    #[test]
    fn a_pinned_reference_shows_proposed_next_to_agreed_when_they_differ() {
        let refs = vec![pinned_ref("let mut attempts = 0;", "let mut attempts = 0u32;")];
        let out = markdown(&report(BTreeMap::from([("src/pay.rs".to_string(), refs)]), vec![]), None, 0);
        assert!(out.contains("## src/pay.rs"), "{out}");
        assert!(out.contains("proposed:"), "{out}");
        assert!(out.contains("agreed (current):"), "{out}");
        assert!(out.contains("let mut attempts = 0;"), "{out}");
        assert!(out.contains("let mut attempts = 0u32;"), "{out}");
    }

    #[test]
    fn an_unchanged_proposal_is_labelled_kept_rather_than_repeated_twice() {
        let refs = vec![pinned_ref("same text", "same text")];
        let out = markdown(&report(BTreeMap::from([("src/pay.rs".to_string(), refs)]), vec![]), None, 0);
        assert!(out.contains("kept as proposed:"), "{out}");
        assert!(!out.contains("agreed (current):"), "{out}");
    }

    #[test]
    fn weaker_evidence_explains_why_there_is_nothing_to_compare() {
        let weak = PromptRef {
            turn: "claude:s:1".into(),
            agent: "claude".into(),
            ts: "2026-09-27T00:00:00Z".into(),
            prompt: "add a retry to the charge call".into(),
            evidence: "temporal".into(),
            lines: None,
            proposed: None,
            agreed: None,
        };
        let out = markdown(&report(BTreeMap::from([("src/pay.rs".to_string(), vec![weak])]), vec![]), None, 0);
        assert!(out.contains("only the request's timing places it here"), "{out}");
    }

    #[test]
    fn unattributed_files_are_listed_separately() {
        let out = markdown(&report(BTreeMap::new(), vec!["src/hand.rs".to_string()]), None, 0);
        assert!(out.contains("## not explained by any request"), "{out}");
        assert!(out.contains("src/hand.rs"), "{out}");
    }

    #[test]
    fn no_local_records_says_so_rather_than_rendering_an_empty_report() {
        let mut r = report(BTreeMap::new(), vec![]);
        r.turns.clear();
        let out = markdown(&r, None, 0);
        assert!(out.contains("no local claude or copilot records"), "{out}");
    }

    #[test]
    fn paging_windows_the_file_section_and_says_how_to_reach_the_rest() {
        let mut attributed = BTreeMap::new();
        for i in 0..3 {
            attributed.insert(format!("src/f{i}.rs"), vec![pinned_ref("a", "a")]);
        }
        let out = markdown(&report(attributed, vec![]), Some(2), 0);
        assert!(out.contains("src/f0.rs") && out.contains("src/f1.rs"), "{out}");
        assert!(!out.contains("src/f2.rs"), "{out}");
        assert!(out.contains("pass offset=2 for the rest"), "{out}");
    }
}
