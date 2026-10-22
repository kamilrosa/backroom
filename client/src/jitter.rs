//! Adaptive jitter buffer for a single speaker + Opus decoder.
//!
//! Packets arrive unevenly and sometimes get lost. The buffer keeps a few
//! frames of headroom (`target`), conceals losses (Opus FEC from the next
//! packet, or PLC when there is none) and tunes its headroom itself: it grows
//! after network "holes" and shrinks when the network is stable. Excess
//! headroom is trimmed so latency does not creep up (e.g. from sound card
//! clock drift).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use backroom_proto::{FLAG_END, FLAG_START, FRAME_SAMPLES};

const FRAME: Duration = Duration::from_millis(20);
const MIN_TARGET: usize = 2;
const MAX_TARGET: usize = 10;
/// After this many frames (100 ms) without data the speaker is considered done.
const MAX_CONCEAL: u32 = 5;
/// How long the network must be stable before headroom shrinks by one frame.
const SHRINK_AFTER: Duration = Duration::from_secs(10);
const MAX_PACKETS: usize = 64;

pub struct JitterBuffer {
    packets: BTreeMap<u32, (u8, Vec<u8>)>,
    next: Option<u32>,
    first_arrival: Option<Instant>,
    decoder: opus::Decoder,
    target: usize,
    concealed_run: u32,
    last_change: Instant,
    pub stats: Stats,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub received: u64,
    pub late: u64,
    pub fec: u64,
    pub plc: u64,
    pub underruns: u64,
    pub dropped_for_latency: u64,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterBuffer {
    pub fn new() -> Self {
        Self {
            packets: BTreeMap::new(),
            next: None,
            first_arrival: None,
            decoder: opus::Decoder::new(48_000, opus::Channels::Mono).expect("Opus decoder"),
            target: 3,
            concealed_run: 0,
            last_change: Instant::now(),
            stats: Stats::default(),
        }
    }

    pub fn push(&mut self, seq: u32, flags: u8, data: &[u8], now: Instant) {
        self.stats.received += 1;
        if flags & FLAG_START != 0 {
            if let Some(n) = self.next {
                // A new utterance while we are still playing the old one (e.g. a
                // lost END packet) — drop the leftovers to avoid adding delay.
                if seq >= n + self.target as u32 || seq < n {
                    self.reset();
                }
            }
        }
        if let Some(n) = self.next {
            if seq < n {
                self.stats.late += 1;
                return;
            }
        }
        if self.packets.is_empty() && self.next.is_none() {
            self.first_arrival = Some(now);
        }
        self.packets.insert(seq, (flags, data.to_vec()));
        while self.packets.len() > MAX_PACKETS {
            self.packets.pop_first();
        }
    }

    fn reset(&mut self) {
        self.next = None;
        self.first_arrival = None;
        self.concealed_run = 0;
        self.packets.clear();
        let _ = self.decoder.reset_state();
    }

    /// Fills `out` with the next 20 ms frame. Returns `false` when there is
    /// nothing to play (`out` is zeroed then).
    pub fn pop(&mut self, out: &mut [f32; FRAME_SAMPLES], now: Instant) -> bool {
        let n = match self.next {
            Some(n) => n,
            None => {
                let Some(&first) = self.packets.keys().next() else {
                    out.fill(0.0);
                    return false;
                };
                let waited = self.first_arrival.map_or(Duration::ZERO, |t| now - t);
                let ends = self.packets.values().any(|(f, _)| f & FLAG_END != 0);
                // Start once there is headroom, the headroom time has passed
                // (short utterance) or the whole utterance is already buffered.
                if self.packets.len() < self.target && waited < FRAME * self.target as u32 && !ends {
                    out.fill(0.0);
                    return false;
                }
                self.next = Some(first);
                first
            }
        };

        self.trim_latency(n);
        let n = self.next.unwrap();

        let mut end = false;
        if let Some((flags, data)) = self.packets.remove(&n) {
            self.decode(&data, out, false);
            self.concealed_run = 0;
            end = flags & FLAG_END != 0;
        } else if let Some((_, data)) = self.packets.get(&(n + 1)) {
            // Single packet loss: recover from the FEC copy in the next one.
            let data = data.clone();
            self.decode(&data, out, true);
            self.stats.fec += 1;
            self.concealed_run += 1;
        } else {
            if self.packets.is_empty() {
                self.stats.underruns += 1;
                self.grow(now);
            }
            self.decode(&[], out, false);
            self.stats.plc += 1;
            self.concealed_run += 1;
        }
        self.next = Some(n + 1);

        if end || (self.concealed_run >= MAX_CONCEAL && self.packets.is_empty()) {
            self.reset();
        } else if self.concealed_run >= MAX_CONCEAL {
            // Long gap, but later packets exist — jump to the nearest one.
            self.next = self.packets.keys().next().copied();
            self.concealed_run = 0;
        }
        if now.saturating_duration_since(self.last_change) > SHRINK_AFTER && self.target > MIN_TARGET {
            self.target -= 1;
            self.last_change = now;
        }
        true
    }

    fn grow(&mut self, now: Instant) {
        if self.target < MAX_TARGET {
            self.target += 1;
        }
        self.last_change = now;
    }

    /// When the buffer holds much more than the headroom, skip ahead.
    fn trim_latency(&mut self, n: u32) {
        let Some(&last) = self.packets.keys().next_back() else { return };
        let depth = last.saturating_sub(n) as usize + 1;
        if depth > self.target + 3 {
            let new_next = last + 1 - self.target as u32;
            let before = self.packets.len();
            self.packets.retain(|&s, _| s >= new_next);
            self.stats.dropped_for_latency += (before - self.packets.len()) as u64;
            self.next = Some(new_next);
        }
    }

    fn decode(&mut self, data: &[u8], out: &mut [f32; FRAME_SAMPLES], fec: bool) {
        match self.decoder.decode_float(data, out, fec) {
            Ok(n) if n == FRAME_SAMPLES => {}
            Ok(n) => out[n.min(FRAME_SAMPLES)..].fill(0.0),
            Err(_) => out.fill(0.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoder() -> opus::Encoder {
        let mut e = opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        e.set_inband_fec(true).unwrap();
        e.set_packet_loss_perc(10).unwrap();
        e
    }

    fn tone_packets(n: usize) -> Vec<Vec<u8>> {
        let mut e = encoder();
        let mut buf = [0u8; 400];
        (0..n)
            .map(|k| {
                let frame: Vec<f32> = (0..FRAME_SAMPLES)
                    .map(|i| 0.3 * ((k * FRAME_SAMPLES + i) as f32 * 300.0 * std::f32::consts::TAU / 48_000.0).sin())
                    .collect();
                let len = e.encode_float(&frame, &mut buf).unwrap();
                buf[..len].to_vec()
            })
            .collect()
    }

    fn energy(x: &[f32]) -> f32 {
        x.iter().map(|s| s * s).sum::<f32>()
    }

    #[test]
    fn waits_for_target_then_plays_in_order() {
        let pk = tone_packets(10);
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        let mut out = [0f32; FRAME_SAMPLES];
        jb.push(0, FLAG_START, &pk[0], t0);
        assert!(!jb.pop(&mut out, t0), "does not start before headroom");
        jb.push(1, 0, &pk[1], t0);
        jb.push(2, 0, &pk[2], t0);
        assert!(jb.pop(&mut out, t0));
        assert_eq!(jb.next, Some(1));
    }

    #[test]
    fn short_utterance_plays_without_waiting() {
        let pk = tone_packets(2);
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        let mut out = [0f32; FRAME_SAMPLES];
        jb.push(0, FLAG_START, &pk[0], t0);
        jb.push(1, FLAG_END, &pk[1], t0);
        assert!(jb.pop(&mut out, t0));
        assert!(jb.pop(&mut out, t0));
        // After END the buffer goes idle — no PLC tail.
        assert!(jb.next.is_none());
        assert!(!jb.pop(&mut out, t0));
        assert_eq!(jb.stats.plc, 0);
    }

    #[test]
    fn single_loss_uses_fec() {
        let pk = tone_packets(10);
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        let mut out = [0f32; FRAME_SAMPLES];
        let mut energies = vec![];
        // Packets arrive at playback pace; packet 4 is lost.
        for i in 0..10u32 {
            if i != 4 {
                jb.push(i, if i == 0 { FLAG_START } else { 0 }, &pk[i as usize], t0);
            }
            if i >= 2 {
                jb.pop(&mut out, t0);
                energies.push(energy(&out));
            }
        }
        assert_eq!(jb.stats.fec, 1);
        assert_eq!(jb.stats.plc, 0);
        assert!(energies[4] > energies[3] * 0.3, "FEC recovered audio: {energies:?}");
    }

    #[test]
    fn late_packets_dropped() {
        let pk = tone_packets(6);
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        let mut out = [0f32; FRAME_SAMPLES];
        for i in 0..4 {
            jb.push(i, 0, &pk[i as usize], t0);
        }
        jb.pop(&mut out, t0);
        jb.pop(&mut out, t0);
        jb.push(0, 0, &pk[0], t0);
        assert_eq!(jb.stats.late, 1);
    }

    #[test]
    fn underrun_grows_target_and_silence_ends_stream() {
        let pk = tone_packets(4);
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        let mut out = [0f32; FRAME_SAMPLES];
        for i in 0..3 {
            jb.push(i, 0, &pk[i as usize], t0);
        }
        for _ in 0..3 {
            assert!(jb.pop(&mut out, t0));
        }
        let before = jb.target;
        assert!(jb.pop(&mut out, t0)); // PLC
        assert_eq!(jb.target, before + 1);
        for _ in 0..MAX_CONCEAL {
            jb.pop(&mut out, t0);
        }
        assert!(jb.next.is_none());
    }

    #[test]
    fn excess_latency_trimmed() {
        let pk = tone_packets(30);
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        let mut out = [0f32; FRAME_SAMPLES];
        for i in 0..30 {
            jb.push(i, 0, &pk[i as usize], t0);
        }
        jb.pop(&mut out, t0);
        assert!(jb.packets.len() <= jb.target, "{} packets left", jb.packets.len());
        assert!(jb.stats.dropped_for_latency > 20);
    }

    #[test]
    fn new_utterance_after_lost_end_resets() {
        let pk = tone_packets(40);
        let mut jb = JitterBuffer::new();
        let t0 = Instant::now();
        let mut out = [0f32; FRAME_SAMPLES];
        for i in 0..3 {
            jb.push(i, 0, &pk[i as usize], t0);
        }
        jb.pop(&mut out, t0);
        jb.push(30, FLAG_START, &pk[30], t0);
        assert_eq!(jb.next, None);
        assert_eq!(jb.packets.len(), 1);
    }
}
