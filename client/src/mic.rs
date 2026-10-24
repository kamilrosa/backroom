//! Microphone chain independent of the sound card: 10 ms of 48 kHz samples
//! in, ready Opus packets (with utterance start/end flags) out. That makes
//! it testable on WAV files.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;

use backroom_proto::{FLAG_END, FLAG_START, FRAME_SAMPLES};
use tracing::{debug, info, warn};

use crate::dsp::{self, Agc, Denoiser, EnergySnr, Gate, GateEvent, HOP};
use crate::state::{MicTest, Shared};

/// Pre-roll: 40 ms from before the gate decision, so word onsets are not clipped.
const PREROLL_FRAMES: usize = 2;
/// The gate already holds 400 ms after speech; reopening sooner than this
/// after closing means it most likely cut a sentence in half — logged.
const SHORT_CLOSE_FRAMES: u32 = 15;
/// Hysteresis: once the gate is open, the speech threshold drops by this much.
/// Measured with mic_eval on speech under loud barking (together with the
/// 400 ms hangover): 99% of speech sent instead of 93%.
const HOLD_MARGIN_DB: f32 = 5.0;

pub struct Packet {
    pub flags: u8,
    pub data: Vec<u8>,
}

pub struct MicChain {
    denoiser: Option<Denoiser>,
    energy: EnergySnr,
    agc: Agc,
    gate: Gate,
    opus: opus::Encoder,
    buf: [u8; 1500],
    preroll: VecDeque<Vec<u8>>,
    frame: [f32; FRAME_SAMPLES],
    half: usize,
    frame_snr: f32,
    /// Frames since the gate opened / closed (for the log).
    open_frames: u32,
    closed_frames: u32,
    /// Frames inside the utterance that were not speech (bridged by the hangover).
    dips: u32,
}

impl Default for MicChain {
    fn default() -> Self {
        Self::new()
    }
}

impl MicChain {
    pub fn new() -> Self {
        let denoiser = Denoiser::new().map_err(|e| warn!("Noise suppression unavailable: {e:#}")).ok();
        let mut opus = opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).expect("Opus encoder");
        let _ = opus.set_bitrate(opus::Bitrate::Bits(32_000));
        let _ = opus.set_inband_fec(true);
        let _ = opus.set_packet_loss_perc(10);
        let _ = opus.set_signal(opus::Signal::Voice);
        Self {
            denoiser,
            energy: EnergySnr::default(),
            agc: Agc::default(),
            gate: Gate::default(),
            opus,
            buf: [0; 1500],
            preroll: VecDeque::new(),
            frame: [0.0; FRAME_SAMPLES],
            half: 0,
            frame_snr: f32::MIN,
            open_frames: 0,
            closed_frames: u32::MAX,
            dips: 0,
        }
    }

    pub fn denoise_available(&self) -> bool {
        self.denoiser.is_some()
    }

    /// After the microphone is reopened, start the frame from scratch.
    pub fn reset_framing(&mut self) {
        self.half = 0;
        self.frame_snr = f32::MIN;
    }

    /// Processes 10 ms (48 kHz, in place). Every second hop completes a 20 ms
    /// frame, appends the packets to send to `out` (0, 1 or several) and
    /// returns `true`.
    pub fn push_hop(&mut self, shared: &Shared, hop: &mut [f32; HOP], out: &mut Vec<Packet>) -> bool {
        let snr = match self.denoiser.as_mut() {
            Some(d) if shared.denoise.load(Ordering::Relaxed) => d.process(hop),
            _ => self.energy.process(hop),
        };
        self.frame_snr = self.frame_snr.max(snr);
        self.frame[self.half * HOP..(self.half + 1) * HOP].copy_from_slice(hop);
        self.half += 1;
        if self.half == 2 {
            self.half = 0;
            let snr = std::mem::replace(&mut self.frame_snr, f32::MIN);
            self.process_frame(shared, snr, out);
            return true;
        }
        false
    }

    fn process_frame(&mut self, shared: &Shared, snr: f32, out: &mut Vec<Packet>) {
        let level = dsp::rms_db(&self.frame);
        // Hysteresis: opening takes the full threshold, staying open a lower one,
        // so quieter syllables inside a sentence do not close the gate.
        let mut threshold = shared.vad_threshold.get();
        if self.gate.is_open() {
            threshold -= HOLD_MARGIN_DB;
        }
        let speech = dsp::is_speech(snr, level, threshold);
        self.agc.process(&mut self.frame, speech);
        shared.mic_level_db.set(dsp::rms_db(&self.frame));

        // The mic test works while muted too — nothing goes to others anyway.
        let testing = matches!(*shared.mic_test.lock(), MicTest::Recording { .. });
        let muted = !testing && (shared.muted.load(Ordering::Relaxed) || shared.deafened.load(Ordering::Relaxed));
        let active = if shared.push_to_talk.load(Ordering::Relaxed) {
            shared.ptt_down.load(Ordering::Relaxed) && !shared.typing.load(Ordering::Relaxed)
        } else {
            speech
        };
        let event = if muted { self.gate.force_close() } else { self.gate.update(active) };
        shared.transmitting.store(self.gate.is_open(), Ordering::Relaxed);

        // The encoder runs all the time so the pre-roll has packets ready.
        let Ok(n) = self.opus.encode_float(&self.frame, &mut self.buf) else { return };
        let pkt = self.buf[..n].to_vec();
        self.log_gate(event, active, snr);
        match event {
            GateEvent::Closed => {
                self.preroll.push_back(pkt);
                while self.preroll.len() > PREROLL_FRAMES {
                    self.preroll.pop_front();
                }
            }
            GateEvent::Opened => {
                let mut flags = FLAG_START;
                for data in self.preroll.drain(..).chain(std::iter::once(pkt)) {
                    out.push(Packet { flags, data });
                    flags = 0;
                }
            }
            GateEvent::Open => out.push(Packet { flags: 0, data: pkt }),
            GateEvent::Closing => out.push(Packet { flags: FLAG_END, data: pkt }),
        }
    }

    fn log_gate(&mut self, event: GateEvent, active: bool, snr: f32) {
        match event {
            GateEvent::Opened => {
                let closed_ms = self.closed_frames.saturating_mul(20);
                if self.closed_frames < SHORT_CLOSE_FRAMES {
                    info!("Microphone: gate reopened after only {closed_ms} ms (SNR {snr:.0} dB)");
                } else {
                    debug!("Microphone: gate opened (SNR {snr:.0} dB)");
                }
                self.open_frames = 1;
                self.dips = 0;
            }
            GateEvent::Open => {
                self.open_frames += 1;
                self.dips += !active as u32;
            }
            GateEvent::Closing => {
                // The last HANG frames are the hangover after speech, not dips.
                let dips = self.dips.saturating_sub(dsp::HANG - 1);
                debug!("Microphone: gate closed after {} ms, {dips} non-speech frames inside", self.open_frames * 20);
                self.closed_frames = 0;
            }
            GateEvent::Closed => self.closed_frames = self.closed_frames.saturating_add(1),
        }
    }

    /// An interrupted utterance (e.g. microphone restart) — close it with an END packet.
    pub fn force_close(&mut self, out: &mut Vec<Packet>) {
        if self.gate.force_close() == GateEvent::Closing {
            if let Ok(n) = self.opus.encode_float(&[0.0; FRAME_SAMPLES], &mut self.buf) {
                out.push(Packet { flags: FLAG_END, data: self.buf[..n].to_vec() });
            }
        }
    }
}
