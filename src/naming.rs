//! cache-file naming: map a project-relative source path to its `.ccc` entry

use std::path::{Component, Path};

// name of the cache file inside `.ccc`, e.g. `src/extract.rs` -> `src-extract.rs.md`
pub fn cache_name(rel_path: &Path) -> String {
    cache_name_inner(rel_path, None)
}

pub fn cache_name_disambiguated(rel_path: &Path) -> String {
    cache_name_inner(rel_path, Some(&short_hash(rel_path)))
}

fn cache_name_inner(rel_path: &Path, disamb: Option<&str>) -> String {
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
    let base = parts.join("-");
    match disamb {
        Some(h) => format!("{}-{}.{}.md", base, h, sanitize(ext)),
        None => format!("{}.{}.md", base, sanitize(ext)),
    }
}

// fnv-1a of the full relative path as 8 lowercase hex digits
fn short_hash(rel_path: &Path) -> String {
    let s = rel_path.to_string_lossy();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", h & 0xffff_ffff)
}

// header display name `src/extract.rs` -> `extract.rs.md`
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

    #[test]
    fn colliding_paths_disambiguate() {
        let a = Path::new("foo-bar/baz.py");
        let b = Path::new("foo_bar/baz.py");
        assert_eq!(cache_name(a), cache_name(b));
        let da = cache_name_disambiguated(a);
        let db = cache_name_disambiguated(b);
        assert_ne!(da, db);
        assert_eq!(da, cache_name_disambiguated(a));
    }
}
