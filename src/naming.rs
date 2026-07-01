//! cache-file naming: map a project-relative source path to its `.ccc` entry

use std::path::{Component, Path};

/// name of the cache file inside `.ccc`, e.g. `src/extract.rs` -> `src-extract.rs.md`
pub fn cache_name(rel_path: &Path) -> String {
    let ext = rel_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt");
    let stem = rel_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");

    let mut parts: Vec<String> = Vec::new();
    if let Some(parent) = rel_path.parent() {
        for comp in parent.components() {
            if let Component::Normal(c) = comp {
                if let Some(s) = c.to_str() {
                    parts.push(sanitize(s));
                }
            }
        }
    }
    parts.push(sanitize(stem));
    format!("{}.{}.md", parts.join("-"), sanitize(ext))
}

/// header display name, e.g. `src/extract.rs` -> `extract.rs.md`
pub fn display_name(rel_path: &Path) -> String {
    let ext = rel_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt");
    let stem = rel_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    format!("{}.{}.md", stem, ext)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn nested_and_root_paths() {
        assert_eq!(cache_name(Path::new("src/extract.rs")), "src-extract.rs.md");
        assert_eq!(
            cache_name(Path::new("src/languages/go.rs")),
            "src-languages-go.rs.md"
        );
        assert_eq!(cache_name(Path::new("main.rs")), "main.rs.md");
        assert_eq!(display_name(Path::new("src/extract.rs")), "extract.rs.md");
    }
}
