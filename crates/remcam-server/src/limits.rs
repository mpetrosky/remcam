//! Limits on connections that haven't proven who they are yet, so handshakes
//! and pairing attempts can't be used to tie up the server.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// New connections allowed per source address per [`WINDOW`]. A client
/// reconnecting with backoff makes fewer than 10 a minute.
const PER_ADDRESS: u32 = 30;
const WINDOW: Duration = Duration::from_secs(60);
/// Connections allowed between accepting and authorizing at any one time.
const UNAUTHENTICATED: usize = 32;
/// Source addresses tracked at once. Beyond this (many addresses within one
/// window), connections from new addresses are refused until a sweep frees
/// room, so memory stays bounded.
const MAX_TRACKED: usize = 4096;
/// While the table is full, expired windows are swept at most this often, so
/// stale entries free room promptly without a scan on every connection.
const FULL_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// Why [`Limits::admit`] refused a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// This address has made too many connections in the current window.
    PerAddress,
    /// Too many distinct addresses are tracked; this affects every new
    /// address, not just one client.
    TableFull,
    /// Too many connections are waiting to be authorized.
    Unauthenticated,
}

pub struct Limits {
    recent: Mutex<Recent>,
    unauthenticated: Arc<Semaphore>,
}

struct Recent {
    /// Per address: when its window started, and connections in it.
    windows: HashMap<IpAddr, (Instant, u32)>,
    /// Expired windows are swept once per [`WINDOW`], or once per
    /// [`FULL_SWEEP_INTERVAL`] while the table is full, so a full table
    /// doesn't mean a full scan on every connection.
    last_sweep: Instant,
}

impl Limits {
    pub fn new() -> Self {
        Self {
            recent: Mutex::new(Recent {
                windows: HashMap::new(),
                last_sweep: Instant::now(),
            }),
            unauthenticated: Arc::new(Semaphore::new(UNAUTHENTICATED)),
        }
    }

    /// Admit a new connection from `ip`, returning a permit to hold until it
    /// is authorized (or rejected).
    pub fn admit(&self, ip: IpAddr, now: Instant) -> Result<OwnedSemaphorePermit, Refused> {
        {
            let mut recent = self.recent.lock().unwrap();
            let since_sweep = now.duration_since(recent.last_sweep);
            let full = |r: &Recent| r.windows.len() >= MAX_TRACKED && !r.windows.contains_key(&ip);
            if since_sweep >= WINDOW || (since_sweep >= FULL_SWEEP_INTERVAL && full(&recent)) {
                recent
                    .windows
                    .retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
                recent.last_sweep = now;
            }
            if full(&recent) {
                return Err(Refused::TableFull);
            }
            let (start, count) = recent.windows.entry(ip).or_insert((now, 0));
            if now.duration_since(*start) >= WINDOW {
                *start = now;
                *count = 0;
            }
            *count += 1;
            if *count > PER_ADDRESS {
                return Err(Refused::PerAddress);
            }
        }
        self.unauthenticated
            .clone()
            .try_acquire_owned()
            .map_err(|_| Refused::Unauthenticated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_address_window() {
        let limits = Limits::new();
        let a: IpAddr = [10, 0, 0, 1].into();
        let b: IpAddr = [10, 0, 0, 2].into();
        let t0 = Instant::now();
        for _ in 0..PER_ADDRESS {
            assert!(limits.admit(a, t0).is_ok());
        }
        assert_eq!(limits.admit(a, t0).unwrap_err(), Refused::PerAddress);
        assert!(limits.admit(b, t0).is_ok());
        assert!(limits.admit(a, t0 + WINDOW).is_ok());
    }

    #[test]
    fn tracked_addresses_bounded() {
        let limits = Limits::new();
        let t0 = Instant::now();
        let ip = |i: usize| IpAddr::from([10, 1, (i >> 8) as u8, i as u8]);
        for i in 0..MAX_TRACKED {
            assert!(limits.admit(ip(i), t0).is_ok());
        }
        // Full: new addresses are refused, known ones still admitted.
        let new: IpAddr = [10, 2, 0, 1].into();
        assert_eq!(limits.admit(new, t0).unwrap_err(), Refused::TableFull);
        assert!(limits.admit(ip(0), t0).is_ok());
        assert_eq!(limits.recent.lock().unwrap().windows.len(), MAX_TRACKED);
        // Once windows expire, a sweep makes room.
        assert!(limits.admit(new, t0 + WINDOW).is_ok());
        assert_eq!(limits.recent.lock().unwrap().windows.len(), 1);
    }

    #[test]
    fn full_table_sweeps_early() {
        let limits = Limits::new();
        let ip = |i: usize| IpAddr::from([10, 1, (i >> 8) as u8, i as u8]);
        // Fill the table just after the scheduled sweep would have run, so
        // the next scheduled sweep is nearly a full window away.
        let t0 = limits.recent.lock().unwrap().last_sweep;
        let filled = t0 + Duration::from_secs(1);
        for i in 0..MAX_TRACKED {
            assert!(limits.admit(ip(i), filled).is_ok());
        }
        let new: IpAddr = [10, 2, 0, 1].into();
        // Before those windows expire, the early sweep frees nothing.
        let early = filled + WINDOW / 2;
        assert_eq!(limits.admit(new, early).unwrap_err(), Refused::TableFull);
        // Once they have, a new address gets in without waiting for the
        // scheduled sweep at t0 + 2 * WINDOW.
        let expired = filled + WINDOW;
        assert!(expired < t0 + 2 * WINDOW);
        assert!(limits.admit(new, expired).is_ok());
    }

    #[test]
    fn unauthenticated_cap() {
        let limits = Limits::new();
        let t0 = Instant::now();
        let held: Vec<_> = (0..UNAUTHENTICATED)
            .map(|i| limits.admit([10, 0, 1, i as u8].into(), t0).unwrap())
            .collect();
        assert_eq!(
            limits.admit([10, 0, 2, 1].into(), t0).unwrap_err(),
            Refused::Unauthenticated
        );
        drop(held);
        assert!(limits.admit([10, 0, 2, 1].into(), t0).is_ok());
    }
}
