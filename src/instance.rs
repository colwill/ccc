//! One ccc serves a project at a time. An instance that starts serving a
//! project stops any other still serving it - an orphan an old session left
//! behind, a second copy started by hand - so every agent's edits land on the
//! one instance the visualiser watches.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// how long an instance told to stop is given before it is made to
#[cfg(unix)]
const GRACE: Duration = Duration::from_secs(2);

// a running process, as far as telling whether it serves a project needs
struct Proc {
    pid: u32,
    args: Vec<String>,
    cwd: Option<PathBuf>,
}

// Stop every other ccc serving `root`, and say which went.
pub fn take_over(root: &Path) -> Vec<u32> {
    let me = std::process::id();
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut others: Vec<u32> = processes()
        .into_iter()
        .filter(|p| p.pid != me && serves(&p.args, p.cwd.as_deref(), &root))
        .map(|p| p.pid)
        .collect();
    others.sort_unstable();
    others.dedup();
    for &pid in &others {
        stop(pid);
    }
    others
}

// the first argument that is neither a flag nor a flag's value
fn positional(args: &[String]) -> Option<&str> {
    const VALUED: &[&str] = &["--addr", "--port", "--watch-interval"];
    let mut value = false;
    for a in args {
        if value {
            value = false;
        } else if VALUED.contains(&a.as_str()) {
            value = true;
        } else if !a.starts_with('-') {
            return Some(a);
        }
    }
    None
}

// Does a process with these arguments serve `root`: a ccc binary - `ccc`,
// `ccc-mcp`, whatever it was built as - running `serve` or `run` on that
// directory, named outright or as the directory it was started in.
fn serves(args: &[String], cwd: Option<&Path>, root: &Path) -> bool {
    let Some((exe, rest)) = args.split_first() else {
        return false;
    };
    let name = Path::new(exe).file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if !name.starts_with("ccc") {
        return false;
    }
    let Some(at) = rest.iter().position(|a| !a.starts_with('-')) else {
        return false;
    };
    if !matches!(rest[at].as_str(), "serve" | "run") {
        return false;
    }
    let path = Path::new(positional(&rest[at + 1..]).unwrap_or("."));
    let full = match (path.is_absolute(), cwd) {
        (true, _) => path.to_path_buf(),
        (false, Some(dir)) => dir.join(path),
        (false, None) => return false,
    };
    std::fs::canonicalize(&full).unwrap_or(full) == root
}

#[cfg(target_os = "linux")]
fn processes() -> Vec<Proc> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    dir.flatten()
        .filter_map(|e| {
            let pid = e.file_name().to_str()?.parse().ok()?;
            let raw = std::fs::read(e.path().join("cmdline")).ok()?;
            let args = raw.split(|&b| b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned()).collect();
            Some(Proc { pid, args, cwd: std::fs::read_link(e.path().join("cwd")).ok() })
        })
        .collect()
}

// without /proc, `ps` - its arguments split on spaces, and where each started unknown
#[cfg(all(unix, not(target_os = "linux")))]
fn processes() -> Vec<Proc> {
    let Ok(out) = std::process::Command::new("ps").args(["-axo", "pid=,command="]).output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let pid = parts.next()?.parse().ok()?;
            Some(Proc { pid, args: parts.map(str::to_string).collect(), cwd: None })
        })
        .collect()
}

#[cfg(not(unix))]
fn processes() -> Vec<Proc> {
    Vec::new()
}

// ask a process to go, then make it
#[cfg(unix)]
fn stop(pid: u32) {
    use std::process::{Command, Stdio};
    let pid = pid.to_string();
    let signal = |sig: &str| Command::new("kill").args([sig, pid.as_str()]).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    signal("-TERM");
    let until = Instant::now() + GRACE;
    while signal("-0") && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    if signal("-0") {
        signal("-KILL");
    }
}

#[cfg(not(unix))]
fn stop(_pid: u32) {
    let _ = Instant::now() + Duration::ZERO;
}

#[cfg(test)]
mod tests {
    use super::*;

    // An instance is a ccc serving this project - however it was started -
    // and no other project's, no other command, and no other program.
    #[test]
    fn an_instance_is_a_ccc_serving_this_project() {
        let dir = std::env::temp_dir().join(format!("ccc-instance-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let root = std::fs::canonicalize(&dir).unwrap();
        let r = root.to_str().unwrap();
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // the editor's, an old session's, one started in the project, and one naming it last
        assert!(serves(&args(&["/x/target/release/ccc", "serve", r, "--html", "--port", "0"]), None, &root));
        assert!(serves(&args(&["/tmp/s/mcp/ccc-mcp", "run", r, "--port", "6767"]), None, &root));
        assert!(serves(&args(&["ccc", "--ignore-skip", "run", "--port", "6767"]), Some(&root), &root));
        assert!(serves(&args(&["ccc", "run", "--port", "6767", r]), None, &root));
        assert!(!serves(&args(&["ccc", "run", "/elsewhere"]), None, &root));
        assert!(!serves(&args(&["ccc", "replay", "save", r]), None, &root));
        assert!(!serves(&args(&["ccc", "changes", "run"]), None, &root));
        assert!(!serves(&args(&["python3", "-m", "http.server", "run", r]), None, &root));
        assert!(!serves(&args(&["ccc", "run"]), None, &root), "a relative path from nowhere known");
        std::fs::remove_dir_all(&dir).ok();
    }
}
