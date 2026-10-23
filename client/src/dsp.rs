//! Microphone signal processing: resampling, noise suppression
//! (DeepFilterNet), automatic gain and the voice activation gate.

use backroom_proto::FRAME_SAMPLES;

/// DeepFilterNet hop at 48 kHz: 10 ms.
pub const HOP: usize = 480;

// ---------------------------------------------------------------------------
// Resampler
// ---------------------------------------------------------------------------

/// Streaming resampler with linear interpolation. For speech (< 8 kHz band)
/// and the usual 44.1↔48 kHz conversions it is perfectly adequate.
pub struct Resampler {
    step: f64,
    pos: f64,
    hist: Vec<f32>,
    bypass: bool,
}

impl Resampler {
    pub fn new(from_rate: u32, to_rate: u32) -> Self {
        Self {
            step: from_rate as f64 / to_rate as f64,
            pos: 0.0,
            hist: Vec::with_capacity(8192),
            bypass: from_rate == to_rate,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.bypass {
            out.extend_from_slice(input);
            return;
        }
        self.hist.extend_from_slice(input);
        while self.pos + 1.0 < self.hist.len() as f64 {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            out.push(self.hist[i] + (self.hist[i + 1] - self.hist[i]) * frac);
            self.pos += self.step;
        }
        // Keep the sample the next interpolation starts from.
        let consumed = (self.pos as usize).min(self.hist.len().saturating_sub(1));
        self.hist.drain(..consumed);
        self.pos -= consumed as f64;
    }
}

// ---------------------------------------------------------------------------
// Noise suppression
// ---------------------------------------------------------------------------

pub struct Denoiser {
    model: df::tract::DfTract,
    inp: ndarray::Array2<f32>,
    out: ndarray::Array2<f32>,
}

impl Denoiser {
    pub fn new() -> anyhow::Result<Self> {
        let rp = df::tract::RuntimeParams::default_with_ch(1);
        let model = df::tract::DfTract::new(df::tract::DfParams::default(), &rp)?;
        anyhow::ensure!(model.hop_size == HOP && model.sr == 48_000, "unexpected DeepFilterNet model");
        Ok(Self { model, inp: ndarray::Array2::zeros((1, HOP)), out: ndarray::Array2::zeros((1, HOP)) })
    }

    /// Denoises 10 ms in place. Returns the local SNR in dB — high for speech,
    /// low (≈ −15) for barking, keyboards and noise.
    pub fn process(&mut self, hop: &mut [f32]) -> f32 {
        self.inp.row_mut(0).as_slice_mut().unwrap().copy_from_slice(hop);
        match self.model.process(self.inp.view(), self.out.view_mut()) {
            Ok(lsnr) => {
                hop.copy_from_slice(self.out.row(0).as_slice().unwrap());
                lsnr
            }
            Err(_) => 0.0,
        }
    }
}

/// Fallback SNR estimator (when denoising is off): the noise floor is the
/// minimum energy over the last 1.5 s — speech always has short pauses
/// between words, so the minimum lands on the background, not the voice.
pub struct EnergySnr {
    hist: std::collections::VecDeque<f32>,
}

const FLOOR_WINDOW: usize = 150; // 150 × 10 ms

impl Default for EnergySnr {
    fn default() -> Self {
        Self { hist: std::collections::VecDeque::with_capacity(FLOOR_WINDOW) }
    }
}

impl EnergySnr {
    pub fn process(&mut self, hop: &[f32]) -> f32 {
        let db = rms_db(hop).max(-90.0);
        if self.hist.len() == FLOOR_WINDOW {
            self.hist.pop_front();
        }
        self.hist.push_back(db);
        let floor = self.hist.iter().copied().fold(f32::MAX, f32::min);
        db - floor
    }
}

// ---------------------------------------------------------------------------
// Automatic gain
// ---------------------------------------------------------------------------

/// Levels the speaker to about −20 dBFS. Learns only from speech frames,
/// so it does not boost noise during pauses.
pub struct Agc {
    gain: f32,
}

const AGC_TARGET: f32 = 0.1;
const AGC_MIN: f32 = 0.5;
const AGC_MAX: f32 = 6.0;

impl Default for Agc {
    fn default() -> Self {
        Self { gain: 1.0 }
    }
}

impl Agc {
    pub fn process(&mut self, frame: &mut [f32], speech: bool) {
        if speech {
            let rms = rms(frame);
            if rms > 1e-4 {
                let desired = (AGC_TARGET / rms).clamp(AGC_MIN, AGC_MAX);
                // Turn down fast (shouting), turn up slowly.
                let rate = if desired < self.gain { 0.25 } else { 0.03 };
                self.gain += (desired - self.gain) * rate;
            }
        }
        for s in frame.iter_mut() {
            *s = soft_clip(*s * self.gain);
        }
    }

    #[cfg(test)]
    pub fn gain(&self) -> f32 {
        self.gain
    }
}

/// Soft limiter: linear up to 0.8, then gently towards 1.0.
pub fn soft_clip(x: f32) -> f32 {
    const K: f32 = 0.8;
    let a = x.abs();
    if a <= K {
        x
    } else {
        let over = (a - K) / (1.0 - K);
        x.signum() * (K + (1.0 - K) * over.tanh())
    }
}

// ---------------------------------------------------------------------------
// Transmit gate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateEvent {
    /// Nothing is sent.
    Closed,
    /// First frame of an utterance (send the pre-roll first).
    Opened,
    /// The utterance continues.
    Open,
    /// Last frame of an utterance (send with FLAG_END).
    Closing,
}

/// Gate driven by "is this speech" (the model's SNR), not by loudness.
/// Decides every 20 ms; after the last speech frame it stays open for `HANG`.
#[derive(Default)]
pub struct Gate {
    open: bool,
    hang_left: u32,
}

/// 400 ms hangover after speech ends: does not clip word endings and bridges
/// short pauses inside a sentence.
pub const HANG: u32 = 20;
/// Anything quieter never opens the gate, even with a high SNR.
const MIN_LEVEL_DB: f32 = -55.0;

impl Gate {
    /// `active` — whether this frame should transmit (speech or PTT held).
    pub fn update(&mut self, active: bool) -> GateEvent {
        if active {
            self.hang_left = HANG;
            if self.open {
                GateEvent::Open
            } else {
                self.open = true;
                GateEvent::Opened
            }
        } else if self.open {
            if self.hang_left > 1 {
                self.hang_left -= 1;
                GateEvent::Open
            } else {
                self.open = false;
                self.hang_left = 0;
                GateEvent::Closing
            }
        } else {
            GateEvent::Closed
        }
    }

    pub fn force_close(&mut self) -> GateEvent {
        if self.open {
            self.open = false;
            self.hang_left = 0;
            GateEvent::Closing
        } else {
            GateEvent::Closed
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }
}

/// Whether a frame is speech: SNR above the sensitivity threshold and not near-silence.
pub fn is_speech(snr_db: f32, level_db: f32, threshold_db: f32) -> bool {
    snr_db > threshold_db && level_db > MIN_LEVEL_DB
}

// ---------------------------------------------------------------------------

pub fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
}

pub fn rms_db(x: &[f32]) -> f32 {
    20.0 * rms(x).max(1e-9).log10()
}

/// Buffer that assembles arbitrary chunks into fixed-size frames.
pub struct Framer {
    buf: Vec<f32>,
    size: usize,
}

impl Framer {
    pub fn new(size: usize) -> Self {
        Self { buf: Vec::with_capacity(size * 4), size }
    }

    pub fn push(&mut self, x: &[f32]) {
        self.buf.extend_from_slice(x);
    }

    pub fn pop(&mut self, out: &mut [f32]) -> bool {
        if self.buf.len() < self.size {
            return false;
        }
        out.copy_from_slice(&self.buf[..self.size]);
        self.buf.drain(..self.size);
        true
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

pub const _ASSERT_FRAME: () = assert!(FRAME_SAMPLES == 2 * HOP);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_preserves_length_and_tone() {
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        let tone: Vec<f32> = (0..44_100).map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / 44_100.0).sin()).collect();
        for chunk in tone.chunks(441) {
            r.process(chunk, &mut out);
        }
        assert!((out.len() as i64 - 48_000).abs() <= 2, "len {}", out.len());
        // 440 Hz tone after conversion: check amplitude and continuity.
        let peak = out[1000..].iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.99 && peak <= 1.0001);
        let max_jump = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
        assert!(max_jump < 0.06, "discontinuity {max_jump}");
    }

    #[test]
    fn resampler_down() {
        let mut r = Resampler::new(48_000, 44_100);
        let mut out = Vec::new();
        for _ in 0..100 {
            r.process(&[0.5; 480], &mut out);
        }
        assert!((out.len() as i64 - 44_100).abs() <= 2);
        assert!(out.iter().all(|s| (s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn gate_hangover_and_end() {
        let mut g = Gate::default();
        assert_eq!(g.update(false), GateEvent::Closed);
        assert_eq!(g.update(true), GateEvent::Opened);
        assert_eq!(g.update(true), GateEvent::Open);
        for _ in 0..HANG - 1 {
            assert_eq!(g.update(false), GateEvent::Open);
        }
        assert_eq!(g.update(false), GateEvent::Closing);
        assert_eq!(g.update(false), GateEvent::Closed);
    }

    #[test]
    fn gate_reopens_during_hang() {
        let mut g = Gate::default();
        g.update(true);
        for _ in 0..5 {
            assert_eq!(g.update(false), GateEvent::Open);
        }
        assert_eq!(g.update(true), GateEvent::Open);
        assert_eq!(g.force_close(), GateEvent::Closing);
        assert!(!g.is_open());
    }

    #[test]
    fn agc_levels_quiet_speaker_up_and_limits() {
        let mut agc = Agc::default();
        let mut quiet = vec![0.01f32; FRAME_SAMPLES];
        for _ in 0..300 {
            quiet.fill(0.01);
            agc.process(&mut quiet, true);
        }
        assert!((agc.gain() - AGC_MAX).abs() < 0.1);
        let mut loud = vec![0.99f32; FRAME_SAMPLES];
        agc.process(&mut loud, true);
        assert!(loud.iter().all(|s| *s <= 1.0));
        // No speech = no learning.
        let g = agc.gain();
        agc.process(&mut [0.5; FRAME_SAMPLES], false);
        assert_eq!(agc.gain(), g);
    }

    #[test]
    fn energy_snr_tracks_floor() {
        let mut e = EnergySnr::default();
        let noise: Vec<f32> = (0..HOP).map(|i| if i % 2 == 0 { 0.001 } else { -0.001 }).collect();
        for _ in 0..200 {
            e.process(&noise);
        }
        assert!(e.process(&noise).abs() < 3.0);
        let speech = vec![0.1f32; HOP];
        assert!(e.process(&speech) > 30.0);
    }

    #[test]
    fn framer() {
        let mut f = Framer::new(4);
        let mut out = [0f32; 4];
        f.push(&[1.0, 2.0, 3.0]);
        assert!(!f.pop(&mut out));
        f.push(&[4.0, 5.0]);
        assert!(f.pop(&mut out));
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert!(!f.pop(&mut out));
    }
}
