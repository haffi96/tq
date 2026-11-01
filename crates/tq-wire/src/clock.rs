//! Clock offsets between hops, and putting remote timestamps on a local
//! clock.
//!
//! Each hop measures its peer's offset with control-stream `ping` / `pong`
//! ([`crate::Control`]). The relay measures every publishing session's
//! offset and puts it in the room state; each session measures the
//! relay's. Adding the two gives any owner's clock relative to a
//! subscriber's, and adding their error bounds bounds the result.

/// NTP-style offset of the remote clock: `t1 - (t0 + t3) / 2`, in
/// microseconds, and the round trip `t3 - t0`.
pub fn clock_offset(t0_us: u64, t1_us: u64, t3_us: u64) -> (i64, u64) {
    let rtt = t3_us.saturating_sub(t0_us);
    let mid = (t0_us as i128 + t3_us as i128) / 2;
    ((t1_us as i128 - mid) as i64, rtt)
}

/// Clock offset estimate from a window of ping samples.
///
/// Each sample's offset is exact only if the request and the reply took
/// equally long; queueing on one direction (a Starlink uplink full of
/// video) biases it by up to half the round trip. The sample with the
/// smallest round trip had the least queueing, so its offset is the best
/// one in the window, and half its round trip bounds the error (the NTP
/// clock filter idea).
#[derive(Debug, Clone)]
pub struct ClockFilter {
    samples: std::collections::VecDeque<(i64, u64)>,
    window: usize,
}

impl ClockFilter {
    pub fn new(window: usize) -> Self {
        Self {
            samples: std::collections::VecDeque::with_capacity(window),
            window,
        }
    }

    pub fn add(&mut self, offset_us: i64, rtt_us: u64) {
        if self.samples.len() == self.window {
            self.samples.pop_front();
        }
        self.samples.push_back((offset_us, rtt_us));
    }

    /// `(offset_us, error_us)` of the best sample in the window.
    pub fn estimate(&self) -> Option<Offset> {
        self.samples
            .iter()
            .min_by_key(|(_, rtt)| *rtt)
            .map(|&(off, rtt)| Offset {
                offset_us: off,
                error_us: (rtt / 2) as i64,
            })
    }

    /// Round trip of the newest sample.
    pub fn last_rtt_us(&self) -> Option<u64> {
        self.samples.back().map(|(_, rtt)| *rtt)
    }
}

/// A remote clock minus a local one, with a bound on the error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Offset {
    pub offset_us: i64,
    pub error_us: i64,
}

impl Offset {
    /// The offset of a clock two hops away: `self` is B − A, `next` is
    /// C − B; the result is C − A.
    pub fn then(self, next: Offset) -> Offset {
        Offset {
            offset_us: self.offset_us + next.offset_us,
            error_us: self.error_us + next.error_us,
        }
    }

    /// A timestamp on the remote clock, on the local one.
    pub fn to_local(self, remote_us: u64) -> u64 {
        (remote_us as i64 - self.offset_us).max(0) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_filter_prefers_least_queued_sample() {
        let mut f = ClockFilter::new(3);
        assert_eq!(f.estimate(), None);
        f.add(5_000, 60_000); // queued
        f.add(100, 2_000);
        f.add(-3_000, 40_000);
        assert_eq!(f.estimate().map(|o| (o.offset_us, o.error_us)), Some((100, 1_000)));
        f.add(7_000, 50_000); // pushes the best sample out
        f.add(8_000, 50_000);
        assert_eq!(f.estimate().map(|o| (o.offset_us, o.error_us)), Some((-3_000, 20_000)));
    }

    #[test]
    fn offset() {
        // Remote is 1000 us ahead; 200 us each way.
        assert_eq!(clock_offset(10_000, 11_200, 10_400), (1000, 400));
        // Owner 5 ms ahead of the relay, relay 2 ms behind me.
        let me_to_relay = Offset {
            offset_us: -2_000,
            error_us: 100,
        };
        let relay_to_owner = Offset {
            offset_us: 5_000,
            error_us: 300,
        };
        let o = me_to_relay.then(relay_to_owner);
        assert_eq!((o.offset_us, o.error_us), (3_000, 400));
        assert_eq!(o.to_local(10_000), 7_000);
    }
}
