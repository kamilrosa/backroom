//! Microphone chain independent of the sound card: 10 ms of 48 kHz samples
//! in, ready Opus packets (with utterance start/end flags) out. That makes
//! it testable on WAV files.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;

use backroom_proto::{FLAG_END, FLAG_START, FRAME_SAMPLES};
use tracing::warn;

use crate::dsp::{self, Agc, Denoiser, EnergySnr, Gate, GateEvent, HOP};
use crate::state::{MicTest, Shared};

/// Pre-roll: 40 ms from before the gate decision, so word onsets are not clipped.
const PREROLL_FRAMES: usize = 2;

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
        let speech = dsp::is_speech(snr, level, shared.vad_threshold.get());
        self.agc.process(&mut self.frame, speech);
        shared.mic_level_db.set(dsp::rms_db(&self.frame));

        // The mic test works while muted too — nothing goes to others anyway.
        let testing = matches!(*shared.mic_test.lock(), MicTest::Recording { .. });
        let muted = !testing && (shared.muted.load(Ordering::Relaxed) || shared.deafened.load(Ordering::Relaxed));
        let active = if shared.push_to_talk.load(Ordering::Relaxed) {
            shared.ptt_down.load(Ordering::Relaxed)
        } else {
            speech
        };
        let event = if muted { self.gate.force_close() } else { self.gate.update(active) };
        shared.transmitting.store(self.gate.is_open(), Ordering::Relaxed);

        // The encoder runs all the time so the pre-roll has packets ready.
        let Ok(n) = self.opus.encode_float(&self.frame, &mut self.buf) else { return };
        let pkt = self.buf[..n].to_vec();
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

    /// An interrupted utterance (e.g. microphone restart) — close it with an END packet.
    pub fn force_close(&mut self, out: &mut Vec<Packet>) {
        if self.gate.force_close() == GateEvent::Closing {
            if let Ok(n) = self.opus.encode_float(&[0.0; FRAME_SAMPLES], &mut self.buf) {
                out.push(Packet { flags: FLAG_END, data: self.buf[..n].to_vec() });
            }
        }
    }
}
