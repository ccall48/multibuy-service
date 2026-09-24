//! Per-second request counts for the last hour, the silences in them, and
//! copies of a packet that arrived late.
//!
//! HPR stops calling a custom multibuy service entirely while it is backing off
//! after a failed request (1s doubling to 5 minutes), and with
//! `fail_on_unavailable` set it drops every packet for the route meanwhile. None
//! of that reaches this service, so it shows up here only as an absence: a
//! stretch of seconds with no requests in otherwise steady traffic. Keeping the
//! history server-side means the gaps are visible after the fact, without a
//! Prometheus server or a dashboard tab left open.
//!
//! HPR also waits for this service's answer before forwarding each copy of an
//! uplink to the LNS. Copies that reach us well after the first of the same
//! packet will reach the LNS late too; past its dedup window they show up there
//! as a second uplink with the same frame count. Those are counted here as
//! "late" (after the dedup window) or "repeats" (seconds later: a device
//! resending an unacknowledged frame, or a copy stalled behind a failed call).

use crate::cache::Seen;
use dashmap::DashMap;
use serde::Serialize;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How many seconds of history are kept.
pub const WINDOW_SECS: u64 = 3600;

/// A ring of time slots, indexed by `unit % len`. The unit is the caller's:
/// traffic records unix seconds over an hour ([`WINDOW_SECS`] slots), hotspots
/// record unix minutes over an hour (60 slots).
///
/// Each slot packs the second it describes into the high 32 bits and that
/// second's request count into the low 32, so a slot left over from a previous
/// lap of the ring is recognisable (its second is stale) and reads as zero.
/// Recording is one CAS on the request path; there is no lock and no
/// background task.
pub struct Ring {
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

/// Copies arriving this long or longer after the first are counted as repeats,
/// not late copies. A LoRaWAN device resends an unacknowledged confirmed uplink
/// only after its receive windows and ACK timeout, at least ~3s later; HPR's own
/// copies normally land well inside that.
pub const REPEAT_AFTER: Duration = Duration::from_secs(3);

/// Most HPR client addresses tracked for per-HPR traffic.
const MAX_PEERS: usize = 32;

/// How one request relates to earlier requests for the same packet key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arrival {
    /// The first copy of this packet.
    First,
    /// A later copy, inside the LNS dedup window.
    OnTime,
    /// A later copy, after the dedup window: the LNS will likely see it as a
    /// separate uplink with a repeated frame count.
    Late,
    /// The same packet seconds later, from a hotspot that already sent it: the
    /// device transmitted the frame again (an unacknowledged confirmed uplink).
    Resend,
    /// The same packet seconds later, from a hotspot that hadn't sent it yet:
    /// that hotspot delivered its copy late (slow backhaul, or buffered).
    SlowCopy,
}

impl Arrival {
    /// Classify a request by how long after the first copy it arrived, and
    /// whether its hotspot had already sent this packet.
    pub fn classify(seen: &Seen, dedup_window: Duration) -> Self {
        match seen.since_first {
            None => Self::First,
            Some(d) if d >= REPEAT_AFTER && seen.same_hotspot => Self::Resend,
            Some(d) if d >= REPEAT_AFTER => Self::SlowCopy,
            Some(d) if d > dedup_window => Self::Late,
            Some(_) => Self::OnTime,
        }
    }
}

/// Per-second history of all requests, the late arrivals among them, and which
/// HPR instances they came from.
pub struct Traffic {
    pub requests: Ring,
    pub late: Ring,
    pub resends: Ring,
    pub slow_copies: Ring,
    pub dedup_window: Duration,
    /// Requests per HPR client IP. Keyed by IP rather than connection so an HPR
    /// keeps one history across reconnects.
    pub peers: DashMap<IpAddr, Ring>,
}

impl Traffic {
    pub fn new(dedup_window: Duration) -> Self {
        Self {
            requests: Ring::new(),
            late: Ring::new(),
            resends: Ring::new(),
            slow_copies: Ring::new(),
            dedup_window,
            peers: DashMap::new(),
        }
    }

    /// Record one request, given what the cache knew about its packet and the
    /// HPR that sent it. Returns how it was classified.
    pub fn record(&self, seen: &Seen, peer: Option<IpAddr>) -> Arrival {
        let second = now_unix();
        self.requests.record_at(second);
        if let Some(ip) = peer {
            self.record_peer(ip, second);
        }

        let arrival = Arrival::classify(seen, self.dedup_window);
        let series = match arrival {
            Arrival::Late => Some(&self.late),
            Arrival::Resend => Some(&self.resends),
            Arrival::SlowCopy => Some(&self.slow_copies),
            Arrival::First | Arrival::OnTime => None,
        };
        if let Some(series) = series {
            series.record_at(second);
        }
        if let (Some(delay), Arrival::OnTime | Arrival::Late) = (seen.since_first, arrival) {
            crate::metrics::record_copy_delay(delay);
        }
        arrival
    }

    fn record_peer(&self, ip: IpAddr, second: u64) {
        if let Some(history) = self.peers.get(&ip) {
            history.record_at(second);
            return;
        }
        if self.peers.len() < MAX_PEERS {
            self.peers.entry(ip).or_default().record_at(second);
        }
    }
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Default for Ring {
    fn default() -> Self {
        Self::starting_at(now_unix())
    }
}

impl Ring {
    pub fn new() -> Self {
        Self::default()
    }

    /// A per-second, one-hour recorder whose history begins at `started_at`
    /// (unix seconds).
    pub fn starting_at(started_at: u64) -> Self {
        Self::with_len(WINDOW_SECS, started_at)
    }

    /// A recorder of `len` slots whose history begins at unit `started_at`.
    pub fn with_len(len: u64, started_at: u64) -> Self {
        Self {
            slots: (0..len).map(|_| AtomicU64::new(0)).collect(),
            started_at,
        }
    }

    fn len(&self) -> u64 {
        self.slots.len() as u64
    }

    /// Count one request now.
    pub fn record(&self) {
        self.record_at(now_unix());
    }

    /// Count one request in the given unix second.
    pub fn record_at(&self, second: u64) {
        let slot = &self.slots[(second % self.len()) as usize];
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
        let start = (now + 1).saturating_sub(self.len()).max(self.started_at);
        let counts = (start..=now)
            .map(|second| {
                let (held, count) =
                    unpack(self.slots[(second % self.len()) as usize].load(Ordering::Relaxed));
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

    /// Only the seconds with a non-zero count, as `(second, count)` — compact
    /// for sparse series like late copies.
    pub fn nonzero(&self) -> Vec<(u64, u32)> {
        self.counts
            .iter()
            .enumerate()
            .filter(|(_, &c)| c > 0)
            .map(|(i, &c)| (self.start + i as u64, c))
            .collect()
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

    fn recorded(seconds: &[u64]) -> Ring {
        let traffic = Ring::starting_at(T0);
        for &s in seconds {
            traffic.record_at(s);
        }
        traffic
    }

    #[test]
    fn arrivals_are_classified_by_delay_and_hotspot() {
        let window = Duration::from_millis(200);
        let at = |ms, same_hotspot| {
            Arrival::classify(
                &Seen {
                    count: 2,
                    since_first: Some(Duration::from_millis(ms)),
                    same_hotspot,
                },
                window,
            )
        };
        let first = Seen {
            count: 1,
            since_first: None,
            same_hotspot: false,
        };
        assert_eq!(Arrival::classify(&first, window), Arrival::First);
        assert_eq!(at(0, false), Arrival::OnTime);
        assert_eq!(at(200, false), Arrival::OnTime);
        assert_eq!(at(201, false), Arrival::Late);
        assert_eq!(at(2_999, true), Arrival::Late);
        assert_eq!(at(3_000, true), Arrival::Resend);
        assert_eq!(at(3_000, false), Arrival::SlowCopy);
        assert_eq!(at(600_000, true), Arrival::Resend);
    }

    #[test]
    fn late_arrivals_are_counted_in_their_series() {
        let traffic = Traffic::new(Duration::from_millis(200));
        let seen = |ms, same_hotspot| Seen {
            count: 2,
            since_first: Some(Duration::from_millis(ms)),
            same_hotspot,
        };
        assert_eq!(traffic.record(&seen(500, false), None), Arrival::Late);
        assert_eq!(traffic.record(&seen(4_000, false), None), Arrival::SlowCopy);
        assert_eq!(traffic.record(&seen(9_000, true), None), Arrival::Resend);
        assert_eq!(traffic.record(&seen(10, false), None), Arrival::OnTime);
        assert_eq!(traffic.late.snapshot().total(), 1);
        assert_eq!(traffic.slow_copies.snapshot().total(), 1);
        assert_eq!(traffic.resends.snapshot().total(), 1);
        assert_eq!(traffic.requests.snapshot().total(), 4);
    }

    #[test]
    fn a_short_ring_wraps_at_its_own_length() {
        let ring = Ring::with_len(60, 100);
        ring.record_at(100);
        ring.record_at(159);
        ring.record_at(160); // same slot as 100, one lap on
        assert_eq!(ring.snapshot_at(160).total(), 2);
    }

    #[test]
    fn requests_are_counted_per_peer() {
        let traffic = Traffic::new(Duration::from_millis(200));
        let first = Seen {
            count: 1,
            since_first: None,
            same_hotspot: false,
        };
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        traffic.record(&first, Some(a));
        traffic.record(&first, Some(a));
        traffic.record(&first, Some(b));
        traffic.record(&first, None);
        let total = |ip| traffic.peers.get(&ip).unwrap().snapshot().total();
        assert_eq!(total(a), 2);
        assert_eq!(total(b), 1);
        assert_eq!(traffic.requests.snapshot().total(), 4);
    }

    #[test]
    fn nonzero_lists_only_busy_seconds() {
        let traffic = recorded(&[T0, T0 + 2, T0 + 2]);
        assert_eq!(
            traffic.snapshot_at(T0 + 3).nonzero(),
            vec![(T0, 1), (T0 + 2, 2)]
        );
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
        let traffic = std::sync::Arc::new(Ring::starting_at(T0));
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
