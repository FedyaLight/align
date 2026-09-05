//! Bounded parallel map with deterministic indexed results.
//! Mirrors Swift's `withTaskGroup` + indexed-buffer pattern (e.g. the
//! 4-reader fingerprint gate): at most `limit` jobs run at once, results
//! return in INPUT order regardless of completion order, so matching and
//! solves stay bit-identical between sequential and parallel runs.
//!
//! A panicking job aborts the whole map after joining its siblings (Swift
//! task-group semantics). Jobs must be panic-free on reachable inputs —
//! decode/match code paths are audited for that; an unreachable panic
//! propagates loudly instead of silently corrupting order.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

pub fn par_map<T, R, F>(items: &[T], limit: usize, f: F) -> Vec<R>
where
    T: Send + Sync,
    R: Send,
    F: Fn(&T) -> R + Send + Sync,
{
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let workers = limit.max(1).min(n);
    let next = AtomicUsize::new(0);
    // Each slot is written exactly once, by whichever worker claims its
    // index; the lock guards only the instant of the store, so it cannot
    // contend and cannot poison (no panicking code runs under it).
    let slots: Vec<Mutex<Option<R>>> = (0..n).map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let produced = f(&items[i]);
                    if let Ok(mut slot) = slots[i].lock() {
                        *slot = Some(produced);
                    }
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|m| {
            m.into_inner()
                .expect("par_map: every slot is filled exactly once")
                .expect("par_map: worker always stores a value")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn preserves_input_order_under_race() {
        let items: Vec<usize> = (0..500).collect();
        let out = par_map(&items, 8, |&i| {
            // Invert timing so later items finish first.
            std::thread::sleep(std::time::Duration::from_micros((500 - i) as u64 % 7));
            i * i
        });
        assert_eq!(out, items.iter().map(|i| i * i).collect::<Vec<_>>());
    }

    #[test]
    fn respects_worker_limit() {
        let live = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let items = vec![0usize; 32];
        par_map(&items, 3, |_| {
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(2));
            live.fetch_sub(1, Ordering::SeqCst);
        });
        assert!(
            peak.load(Ordering::SeqCst) <= 3,
            "peak={}",
            peak.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn empty_and_singleton() {
        let empty: Vec<usize> = Vec::new();
        assert!(par_map(&empty, 4, |&i| i).is_empty());
        assert_eq!(par_map(&[42], 4, |&i| i + 1), vec![43]);
    }
}
