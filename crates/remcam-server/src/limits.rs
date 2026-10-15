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

pub struct Limits {
    recent: Mutex<HashMap<IpAddr, (Instant, u32)>>,
    unauthenticated: Arc<Semaphore>,
}

impl Limits {
    pub fn new() -> Self {
        Self {
            recent: Mutex::new(HashMap::new()),
            unauthenticated: Arc::new(Semaphore::new(UNAUTHENTICATED)),
        }
    }

    /// Admit a new connection from `ip`, returning a permit to hold until it
    /// is authorized (or rejected); `None` means refuse it.
    pub fn admit(&self, ip: IpAddr, now: Instant) -> Option<OwnedSemaphorePermit> {
        {
            let mut recent = self.recent.lock().unwrap();
            if recent.len() > 4096 {
                recent.retain(|_, (start, _)| now.duration_since(*start) < WINDOW);
            }
            let (start, count) = recent.entry(ip).or_insert((now, 0));
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
