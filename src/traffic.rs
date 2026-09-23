//! Per-second request counts for the last hour, and the silences in them.
//!
//! HPR stops calling a custom multibuy service entirely while it is backing off
//! after a failed request (1s doubling to 5 minutes), and with
//! `fail_on_unavailable` set it drops every packet for the route meanwhile. None
//! of that reaches this service, so it shows up here only as an absence: a
//! stretch of seconds with no requests in otherwise steady traffic. Keeping the
//! history server-side means the gaps are visible after the fact, without a
//! Prometheus server or a dashboard tab left open.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many seconds of history are kept.
pub const WINDOW_SECS: u64 = 3600;

/// A ring of one-second slots, indexed by `unix_second % WINDOW_SECS`.
///
/// Each slot packs the second it describes into the high 32 bits and that
/// second's request count into the low 32, so a slot left over from a previous
/// lap of the ring is recognisable (its second is stale) and reads as zero.
/// Recording is one CAS on the request path; there is no lock and no
/// background task.
pub struct Traffic {
    slots: Box<[AtomicU64]>,
    /// Unix seconds when recording began. Earlier seconds are unknown, not
    /// silent, so they are never reported.
    started_at: u64,
}

/// Counts for a contiguous run of seconds: `counts[i]` is for second `start + i`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Snapshot {
    pub start: u64,
    pub counts: Vec<u32>,
}

/// A run of seconds with no requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Silence {
    /// Unix second the silence began (the first second with no requests).
    pub start: u64,
    pub seconds: u64,
    /// Still silent as of the latest second in the snapshot.
    pub ongoing: bool,
}

fn pack(second: u64, count: u32) -> u64 {
    (second << 32) | u64::from(count)
}

fn unpack(slot: u64) -> (u64, u32) {
    (slot >> 32, slot as u32)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Default for Traffic {
    fn default() -> Self {
        Self::starting_at(now_unix())
    }
}

impl Traffic {
    pub fn new() -> Self {
        Self::default()
    }

    /// A recorder whose history begins at `started_at` (unix seconds).
    pub fn starting_at(started_at: u64) -> Self {
        Self {
            slots: (0..WINDOW_SECS).map(|_| AtomicU64::new(0)).collect(),
            started_at,
        }
    }

    /// Count one request now.
    pub fn record(&self) {
        self.record_at(now_unix());
    }

    /// Count one request in the given unix second.
    pub fn record_at(&self, second: u64) {
        let slot = &self.slots[(second % WINDOW_SECS) as usize];
        let mut current = slot.load(Ordering::Relaxed);
        loop {
            let (held, count) = unpack(current);
            let next = if held == second {
                pack(second, count.saturating_add(1))
            } else if held < second {
                // A slot from a previous lap of the ring: this second starts fresh.
                pack(second, 1)
            } else {
                // The slot already holds a later second, so the clock stepped
                // back. Dropping one count beats corrupting newer history.
                return;
            };
            match slot.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// Counts for the window ending at the current second.
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_at(now_unix())
    }

    /// Counts for the window ending at `now` (inclusive), clipped to when
    /// recording began.
    pub fn snapshot_at(&self, now: u64) -> Snapshot {
        let start = (now + 1).saturating_sub(WINDOW_SECS).max(self.started_at);
        let counts = (start..=now)
            .map(|second| {
                let (held, count) =
                    unpack(self.slots[(second % WINDOW_SECS) as usize].load(Ordering::Relaxed));
                if held == second {
                    count
                } else {
                    0
                }
            })
            .collect();
        Snapshot { start, counts }
    }
}

impl Snapshot {
    /// The last second in the snapshot, or `None` if it is empty.
    pub fn end(&self) -> Option<u64> {
        (self.counts.len() as u64)
            .checked_sub(1)
            .map(|last| self.start + last)
    }

    pub fn total(&self) -> u64 {
        self.counts.iter().map(|&c| u64::from(c)).sum()
    }

    /// The most recent second with at least one request.
    pub fn last_request(&self) -> Option<u64> {
        self.counts
            .iter()
            .rposition(|&c| c > 0)
            .map(|i| self.start + i as u64)
    }

    /// Runs of at least `min_seconds` with no requests, oldest first.
    ///
    /// Only silences that follow a request are reported: a quiet stretch at the
    /// start of the window may just be the tail of an earlier one whose start we
    /// can't see, and before any traffic at all there is nothing to have gone
    /// missing.
    pub fn silences(&self, min_seconds: u64) -> Vec<Silence> {
        let min_seconds = min_seconds.max(1);
        let mut out = Vec::new();
        let mut run_start: Option<u64> = None;
        let mut seen_request = false;

        for (i, &count) in self.counts.iter().enumerate() {
            let second = self.start + i as u64;
            if count > 0 {
                if let Some(start) = run_start.take() {
                    if second - start >= min_seconds {
                        out.push(Silence {
                            start,
                            seconds: second - start,
                            ongoing: false,
                        });
                    }
                }
                seen_request = true;
            } else if seen_request && run_start.is_none() {
                run_start = Some(second);
            }
        }

        if let (Some(start), Some(end)) = (run_start, self.end()) {
            let seconds = end + 1 - start;
            if seconds >= min_seconds {
                out.push(Silence {
                    start,
                    seconds,
                    ongoing: true,
                });
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_800_000_000;

    fn recorded(seconds: &[u64]) -> Traffic {
        let traffic = Traffic::starting_at(T0);
        for &s in seconds {
            traffic.record_at(s);
        }
        traffic
    }

    #[test]
    fn counts_requests_per_second() {
        let traffic = recorded(&[T0, T0, T0 + 2]);
        let snap = traffic.snapshot_at(T0 + 3);
        assert_eq!(snap.start, T0);
        assert_eq!(snap.counts, vec![2, 0, 1, 0]);
        assert_eq!(snap.total(), 3);
        assert_eq!(snap.last_request(), Some(T0 + 2));
    }

    #[test]
    fn window_is_clipped_to_start_of_recording() {
        let traffic = recorded(&[T0]);
        assert_eq!(traffic.snapshot_at(T0 + 9).counts.len(), 10);
    }

    #[test]
    fn slots_from_a_previous_lap_read_as_zero_and_restart() {
        let traffic = recorded(&[T0, T0]);
        let later = T0 + WINDOW_SECS; // same slot, one lap on
        let snap = traffic.snapshot_at(later);
        assert_eq!(snap.start, T0 + 1);
        assert_eq!(*snap.counts.last().unwrap(), 0);

        traffic.record_at(later);
        assert_eq!(*traffic.snapshot_at(later).counts.last().unwrap(), 1);
    }

    #[test]
    fn clock_stepping_back_does_not_clobber_newer_seconds() {
        let traffic = recorded(&[T0 + WINDOW_SECS]);
        traffic.record_at(T0); // same slot, but older
        let snap = traffic.snapshot_at(T0 + WINDOW_SECS);
        assert_eq!(*snap.counts.last().unwrap(), 1);
    }

    #[test]
    fn silences_between_requests() {
        // Traffic, 4 quiet seconds, traffic, 1 quiet second, traffic.
        let traffic = recorded(&[T0, T0 + 5, T0 + 7]);
        let snap = traffic.snapshot_at(T0 + 7);
        assert_eq!(
            snap.silences(2),
            vec![Silence {
                start: T0 + 1,
                seconds: 4,
                ongoing: false
            }]
        );
        assert_eq!(snap.silences(1).len(), 2);
    }

    #[test]
    fn leading_quiet_is_not_a_silence() {
        let traffic = recorded(&[T0 + 30]);
        assert!(traffic.snapshot_at(T0 + 30).silences(1).is_empty());
    }

    #[test]
    fn trailing_silence_is_ongoing() {
        let traffic = recorded(&[T0]);
        assert_eq!(
            traffic.snapshot_at(T0 + 10).silences(5),
            vec![Silence {
                start: T0 + 1,
                seconds: 10,
                ongoing: true
            }]
        );
    }

    #[test]
    fn concurrent_records_are_all_counted() {
        let traffic = std::sync::Arc::new(Traffic::starting_at(T0));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let traffic = traffic.clone();
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        traffic.record_at(T0);
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(traffic.snapshot_at(T0).counts, vec![8000]);
    }
}
