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
/// window, likely spoofed), connections from new addresses are refused until
/// the next sweep frees room, so memory stays bounded.
const MAX_TRACKED: usize = 4096;

pub struct Limits {
    recent: Mutex<Recent>,
    unauthenticated: Arc<Semaphore>,
}

struct Recent {
    /// Per address: when its window started, and connections in it.
    windows: HashMap<IpAddr, (Instant, u32)>,
    /// Expired windows are swept at most once per [`WINDOW`], so a full
    /// table doesn't mean a full scan on every connection.
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
    /// is authorized (or rejected); `None` means refuse it.
    pub fn admit(&self, ip: IpAddr, now: Instant) -> Option<OwnedSemaphorePermit> {
        {
            let mut recent = self.recent.lock().unwrap();
            if now.duration_since(recent.last_sweep) >= WINDOW {
                recent
                    .windows
                    .retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
                recent.last_sweep = now;
            }
            if recent.windows.len() >= MAX_TRACKED && !recent.windows.contains_key(&ip) {
                return None;
            }
            let (start, count) = recent.windows.entry(ip).or_insert((now, 0));
            if now.duration_since(*start) >= WINDOW {
                *start = now;
                *count = 0;
            }
            *count += 1;
            if *count > PER_ADDRESS {
                return None;
            }
        }
        self.unauthenticated.clone().try_acquire_owned().ok()
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
            assert!(limits.admit(a, t0).is_some());
        }
        assert!(limits.admit(a, t0).is_none());
        assert!(limits.admit(b, t0).is_some());
        assert!(limits.admit(a, t0 + WINDOW).is_some());
    }

    #[test]
    fn tracked_addresses_bounded() {
        let limits = Limits::new();
        let t0 = Instant::now();
        let ip = |i: usize| IpAddr::from([10, 1, (i >> 8) as u8, i as u8]);
        for i in 0..MAX_TRACKED {
            assert!(limits.admit(ip(i), t0).is_some());
        }
        // Full: new addresses are refused, known ones still admitted.
        assert!(limits.admit([10, 2, 0, 1].into(), t0).is_none());
        assert!(limits.admit(ip(0), t0).is_some());
        assert_eq!(limits.recent.lock().unwrap().windows.len(), MAX_TRACKED);
        // The next sweep drops expired windows and makes room.
        assert!(limits.admit([10, 2, 0, 1].into(), t0 + WINDOW).is_some());
        assert_eq!(limits.recent.lock().unwrap().windows.len(), 1);
    }

    #[test]
    fn unauthenticated_cap() {
        let limits = Limits::new();
        let t0 = Instant::now();
        let held: Vec<_> = (0..UNAUTHENTICATED)
            .map(|i| limits.admit([10, 0, 1, i as u8].into(), t0).unwrap())
            .collect();
        assert!(limits.admit([10, 0, 2, 1].into(), t0).is_none());
        drop(held);
        assert!(limits.admit([10, 0, 2, 1].into(), t0).is_some());
    }
}
