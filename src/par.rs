//! Parallelise independent pieces of work - over every core

use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

// indices a worker takes at a time - small enough to keep every core busy, large enough not to contend
const CHUNK: usize = 16;

// `f(i)` for every `i` in `0..n` in order, chunks pulled off a shared counter by every core and the caller
pub(crate) fn map<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let workers = thread::available_parallelism().map_or(1, |p| p.get()).min(n.div_ceil(CHUNK));
    if workers <= 1 {
        return (0..n).map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let work = || {
        let mut done = Vec::new();
        loop {
            let start = next.fetch_add(CHUNK, Ordering::Relaxed);
            if start >= n {
                return done;
            }
            done.push((start, (start..(start + CHUNK).min(n)).map(&f).collect::<Vec<T>>()));
        }
    };
    let mut parts = thread::scope(|s| {
        let helpers: Vec<_> = (1..workers).filter_map(|_| thread::Builder::new().spawn_scoped(s, work).ok()).collect();
        let mut parts = work();
        for h in helpers {
            parts.extend(h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)));
        }
        parts
    });
    parts.sort_unstable_by_key(|(start, _)| *start);
    parts.into_iter().flat_map(|(_, part)| part).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // the answers come back in index order however the chunks were shared out
    #[test]
    fn results_keep_their_order() {
        for n in [0, 1, CHUNK - 1, CHUNK, CHUNK * 7 + 3, 10_000] {
            let squares = map(n, |i| i * i);
            assert_eq!(squares, (0..n).map(|i| i * i).collect::<Vec<_>>());
        }
    }
}
