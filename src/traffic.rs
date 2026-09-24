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

/// A ring of one-second slots, indexed by `unix_second % WINDOW_SECS`.
///
/// Each slot packs the second it describes into the high 32 bits and that
/// second's request count into the low 32, so a slot left over from a previous
/// lap of the ring is recognisable (its second is stale) and reads as zero.
/// Recording is one CAS on the request path; there is no lock and no
/// background task.
pub struct PerSecond {
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

/// Most hotspots tracked for late-copy attribution. The hotspots that hear one
/// operator's devices number in the tens; the cap only guards memory.
const MAX_HOTSPOTS: usize = 2_000;

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

/// Late arrivals attributed to one hotspot.
#[derive(Debug, Default)]
pub struct HotspotTiming {
    /// Copies after the dedup window but under [`REPEAT_AFTER`].
    pub late: AtomicU64,
    /// Copies [`REPEAT_AFTER`] or more after the first, from this hotspot's
    /// first copy of the packet.
    pub slow: AtomicU64,
    /// The packet again from this hotspot after it had already sent it.
    pub resends: AtomicU64,
    /// Unix seconds of the most recent of any of the above.
    pub last: AtomicU64,
}

/// Per-second history of all requests, the late arrivals among them, and which
/// hotspots and HPR instances they came from.
pub struct Traffic {
    pub requests: PerSecond,
    pub late: PerSecond,
    pub resends: PerSecond,
    pub slow_copies: PerSecond,
    pub dedup_window: Duration,
    /// Keyed by the hotspot's b58 address.
    pub hotspots: DashMap<String, HotspotTiming>,
    /// Requests per HPR client IP. Keyed by IP rather than connection so an HPR
    /// keeps one history across reconnects.
    pub peers: DashMap<IpAddr, PerSecond>,
}

impl Traffic {
    pub fn new(dedup_window: Duration) -> Self {
        Self {
            requests: PerSecond::new(),
            late: PerSecond::new(),
            resends: PerSecond::new(),
            slow_copies: PerSecond::new(),
            dedup_window,
            hotspots: DashMap::new(),
            peers: DashMap::new(),
        }
    }

    /// Record one request: what the cache knew about its packet, the hotspot
    /// that heard it, and the HPR that sent it.
    pub fn record(&self, seen: &Seen, hotspot: &str, peer: Option<IpAddr>) -> Arrival {
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
            self.record_hotspot(hotspot, arrival, second);
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

    fn record_hotspot(&self, hotspot: &str, arrival: Arrival, second: u64) {
        if hotspot.is_empty() {
            return;
        }
        let bump = |timing: &HotspotTiming| {
            let counter = match arrival {
                Arrival::Late => &timing.late,
                Arrival::SlowCopy => &timing.slow,
                _ => &timing.resends,
            };
            counter.fetch_add(1, Ordering::Relaxed);
            timing.last.store(second, Ordering::Relaxed);
        };
        if let Some(timing) = self.hotspots.get(hotspot) {
            bump(&timing);
            return;
        }
        if self.hotspots.len() < MAX_HOTSPOTS {
            bump(&self.hotspots.entry(hotspot.to_string()).or_default());
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Default for PerSecond {
    fn default() -> Self {
        Self::starting_at(now_unix())
    }
}

impl PerSecond {
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

    fn recorded(seconds: &[u64]) -> PerSecond {
        let traffic = PerSecond::starting_at(T0);
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
    fn repeats_are_attributed_to_their_hotspot() {
        let traffic = Traffic::new(Duration::from_millis(200));
        let seen = |ms, same_hotspot| Seen {
            count: 2,
            since_first: Some(Duration::from_millis(ms)),
            same_hotspot,
        };
        assert_eq!(
            traffic.record(&seen(500, false), "hs-a", None),
            Arrival::Late
        );
        assert_eq!(
            traffic.record(&seen(4_000, false), "hs-b", None),
            Arrival::SlowCopy
        );
        assert_eq!(
            traffic.record(&seen(9_000, true), "hs-b", None),
            Arrival::Resend
        );
        assert_eq!(
            traffic.record(&seen(10, false), "hs-c", None),
            Arrival::OnTime
        );

        let get = |h: &str| {
            let t = traffic.hotspots.get(h).unwrap();
            (
                t.late.load(Ordering::Relaxed),
                t.slow.load(Ordering::Relaxed),
                t.resends.load(Ordering::Relaxed),
            )
        };
        assert_eq!(get("hs-a"), (1, 0, 0));
        assert_eq!(get("hs-b"), (0, 1, 1));
        // On-time copies aren't attributed to anyone.
        assert!(traffic.hotspots.get("hs-c").is_none());
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
        traffic.record(&first, "", Some(a));
        traffic.record(&first, "", Some(a));
        traffic.record(&first, "", Some(b));
        traffic.record(&first, "", None);
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
        let traffic = std::sync::Arc::new(PerSecond::starting_at(T0));
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
