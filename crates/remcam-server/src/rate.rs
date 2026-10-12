//! Adaptive bitrate for H.264: back off quickly when the link shows strain,
//! creep back up when it doesn't (AIMD-style, but multiplicative both ways
//! so it scales from a few hundred kbit/s to tens of Mbit/s).
//!
//! Signals, checked once per [`INTERVAL`]:
//! - frames dropped before encoding because earlier ones were still in
//!   flight: the link can't carry what we produce,
//! - QUIC congestion events with noticeable packet loss,
//! - RTT well above the lowest seen: packets are queueing somewhere.

use std::time::{Duration, Instant};

pub const INTERVAL: Duration = Duration::from_secs(1);

/// Lowest bitrate we go to; below this, a camera picture isn't worth much.
const MIN_BITRATE: u32 = 250_000;
const DECREASE: f64 = 0.7;
const MILD_DECREASE: f64 = 0.85;
const INCREASE: f64 = 1.08;
/// After backing off, wait this long before increasing again.
const HOLD: Duration = Duration::from_secs(3);
/// Loss below this is treated as random (e.g. Wi-Fi), not congestion.
const LOSS_THRESHOLD: f64 = 0.02;

/// Counters sampled from the connection; totals since it started.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sample {
    pub rtt: Duration,
    pub sent_packets: u64,
    pub lost_packets: u64,
    pub congestion_events: u64,
    pub dropped_frames: u64,
}

impl Sample {
    pub fn from_connection(conn: &quinn::Connection, dropped_frames: u64) -> Self {
        let path = conn.stats().path;
        Self {
            rtt: path.rtt,
            sent_packets: path.sent_packets,
            lost_packets: path.lost_packets,
            congestion_events: path.congestion_events,
            dropped_frames,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Change {
    Down(&'static str),
    Up,
}

pub struct RateController {
    max: u32,
    target: u32,
    last: Option<Sample>,
    min_rtt: Option<Duration>,
    hold_until: Option<Instant>,
}

impl RateController {
    pub fn new(max: u32) -> Self {
        let max = max.max(MIN_BITRATE);
        Self {
            max,
            target: max,
            last: None,
            min_rtt: None,
            hold_until: None,
        }
    }

    pub fn target(&self) -> u32 {
        self.target
    }

    /// Feed a new sample; returns what changed, if anything.
    pub fn update(&mut self, now: Instant, s: Sample) -> Option<Change> {
        let Some(last) = self.last.replace(s) else {
            self.min_rtt = Some(s.rtt);
            return None;
        };
        let min_rtt = *self
            .min_rtt
            .insert(self.min_rtt.map_or(s.rtt, |m| m.min(s.rtt)));

        let sent = s.sent_packets.saturating_sub(last.sent_packets);
        let lost = s.lost_packets.saturating_sub(last.lost_packets);
        let loss = lost as f64 / sent.max(1) as f64;
        // Queueing: RTT doubled and up by at least 30 ms.
        let queueing = s.rtt > min_rtt * 2 && s.rtt > min_rtt + Duration::from_millis(30);

        let change = if s.dropped_frames > last.dropped_frames {
            Some((DECREASE, Change::Down("frames backed up")))
        } else if s.congestion_events > last.congestion_events && loss > LOSS_THRESHOLD {
            Some((MILD_DECREASE, Change::Down("packet loss")))
        } else if queueing {
            Some((MILD_DECREASE, Change::Down("rising delay")))
        } else {
            None
        };

        let old = self.target;
        let change = match change {
            Some((factor, change)) => {
                self.target = ((self.target as f64 * factor) as u32).max(MIN_BITRATE);
                self.hold_until = Some(now + HOLD);
                change
            }
            None if self.hold_until.is_some_and(|t| now < t) => return None,
            None => {
                self.target = ((self.target as f64 * INCREASE) as u32).min(self.max);
                Change::Up
            }
        };
        (self.target != old).then_some(change)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t: u64, rtt_ms: u64, lost: u64, events: u64, dropped: u64) -> Sample {
        Sample {
            rtt: Duration::from_millis(rtt_ms),
            sent_packets: t * 1000,
            lost_packets: lost,
            congestion_events: events,
            dropped_frames: dropped,
        }
    }

    #[test]
    fn backs_off_and_recovers() {
        let start = Instant::now();
        let at = |s: u64| start + Duration::from_secs(s);
        let mut rc = RateController::new(4_000_000);
        assert_eq!(rc.update(at(0), sample(0, 10, 0, 0, 0)), None);
        // Healthy and already at the maximum: nothing to do.
        assert_eq!(rc.update(at(1), sample(1, 10, 0, 0, 0)), None);

        // Dropped frames: big step down.
        assert!(matches!(
            rc.update(at(2), sample(2, 12, 0, 0, 3)),
            Some(Change::Down(_))
        ));
        assert_eq!(rc.target(), 2_800_000);
        // Random loss without a congestion event is ignored, and the hold
        // period stops an immediate increase.
        assert_eq!(rc.update(at(3), sample(3, 10, 50, 0, 3)), None);
        // Queueing delay: mild step down.
        assert!(matches!(
            rc.update(at(4), sample(4, 80, 50, 0, 3)),
            Some(Change::Down("rising delay"))
        ));
        assert_eq!(rc.target(), 2_380_000);

        // Once the hold expires, it climbs back, but never past the maximum.
        let mut t = 8;
        while rc.target() < 4_000_000 {
            assert_eq!(rc.update(at(t), sample(t, 10, 50, 0, 3)), Some(Change::Up));
            t += 1;
        }
        assert_eq!(rc.target(), 4_000_000);
        assert_eq!(rc.update(at(t), sample(t, 10, 50, 0, 3)), None);
    }

    #[test]
    fn never_below_minimum() {
        let start = Instant::now();
        let mut rc = RateController::new(300_000);
        rc.update(start, sample(0, 10, 0, 0, 0));
        for t in 1..20 {
            rc.update(start + Duration::from_secs(t), sample(t, 10, 0, 0, t));
        }
        assert_eq!(rc.target(), MIN_BITRATE);
    }
}
