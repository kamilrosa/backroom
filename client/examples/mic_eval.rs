//! Evaluates the microphone chain on WAV files (48 kHz mono 16-bit):
//!
//!   cargo run --release --example mic_eval -- speech.wav noise.wav [noise_gain_dB] [out.wav]
//!
//! Mixes speech with noise, runs it through denoising + gate + Opus and
//! reports what share of noise-only / speech frames would be transmitted.
//! Writes what others would hear to `out.wav`.

use std::time::{Duration, Instant};

use backroom_client::config::Config;
use backroom_client::dsp::{self, HOP};
use backroom_client::jitter::JitterBuffer;
use backroom_client::mic::MicChain;
use backroom_client::state::Shared;
use backroom_proto::FRAME_SAMPLES;
use std::sync::atomic::Ordering;

fn read(p: &str) -> Vec<f32> {
    let r = hound::WavReader::open(p).unwrap_or_else(|e| panic!("{p}: {e}"));
    assert_eq!(r.spec().sample_rate, 48_000, "{p}: 48 kHz required");
    r.into_samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect()
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let speech = read(&a[1]);
    let noise_raw = read(&a[2]);
    let gain_db: f32 = a.get(3).map_or(0.0, |s| s.parse().unwrap());
    let out_path = a.get(4).cloned();
    let n = speech.len();
    let g = 10f32.powf(gain_db / 20.0);
    let noise: Vec<f32> = (0..n).map(|i| noise_raw[i % noise_raw.len()] * g).collect();
    let mix: Vec<f32> = speech.iter().zip(&noise).map(|(s, b)| s + b).collect();

    for (label, denoise, sens) in [
        ("denoise, normal sensitivity", true, backroom_client::config::Sensitivity::Normal),
        ("denoise, high sensitivity  ", true, backroom_client::config::Sensitivity::High),
        ("no denoise (level threshold)", false, backroom_client::config::Sensitivity::Normal),
    ] {
        let shared = Shared::new(&Config::default());
        shared.denoise.store(denoise, Ordering::Relaxed);
        shared.vad_threshold.set(sens.threshold_db());
        let mut chain = MicChain::new();
        let mut jb = JitterBuffer::new();
        let mut heard = Vec::with_capacity(n);
        let mut packets = Vec::new();
        let mut sent_frames = vec![false; n / FRAME_SAMPLES + 1];
        let mut seq = 0u32;
        let t0 = Instant::now();
        let mut fake_now = t0;
        let mut out = [0f32; FRAME_SAMPLES];
        let mut cpu = Duration::ZERO;
        for (i, c) in mix.as_chunks::<HOP>().0.iter().enumerate() {
            let mut hop = *c;
            let t = Instant::now();
            chain.push_hop(&shared, &mut hop, &mut packets);
            cpu += t.elapsed();
            if i % 2 == 1 {
                let f = i / 2;
                for p in packets.drain(..) {
                    seq += 1;
                    sent_frames[f] = true;
                    jb.push(seq, p.flags, &p.data, fake_now);
                }
                fake_now += Duration::from_millis(20);
                jb.pop(&mut out, fake_now);
                heard.extend_from_slice(&out);
            }
        }
        // Classify frames by the clean tracks (with margin for the gate hangover).
        let frames = n / FRAME_SAMPLES;
        let near = |v: &[f32], f: usize, thr: f32| {
            let a = f.saturating_sub(20) * FRAME_SAMPLES;
            let b = ((f + 3) * FRAME_SAMPLES).min(n);
            v[a..b].chunks(FRAME_SAMPLES).any(|c| dsp::rms(c) > thr)
        };
        let (mut sp, mut sp_sent, mut nz, mut nz_sent, mut sil, mut sil_sent) = (0, 0, 0, 0, 0, 0);
        for (f, &sent) in sent_frames.iter().enumerate().take(frames) {
            let r = f * FRAME_SAMPLES..(f + 1) * FRAME_SAMPLES;
            let s = dsp::rms(&speech[r.clone()]) > 0.02;
            let b = dsp::rms(&noise[r.clone()]) > 0.02;
            if s {
                sp += 1;
                sp_sent += sent as u32;
            } else if b && !near(&speech, f, 0.005) {
                nz += 1;
                nz_sent += sent as u32;
            } else if !b && !near(&speech, f, 0.005) {
                sil += 1;
                sil_sent += sent as u32;
            }
        }
        // Cuts: gaps in transmission inside an utterance (sent before and after, within 1 s).
        let (mut cuts, mut cut_ms, mut run) = (0u32, 0u32, 0u32);
        let mut seen_sent = false;
        for &sent in &sent_frames[..frames] {
            if sent {
                if seen_sent && run > 0 && run < 50 {
                    cuts += 1;
                    cut_ms += run * 20;
                }
                seen_sent = true;
                run = 0;
            } else {
                run += 1;
            }
        }
        println!("  gaps under 1 s inside speech: {cuts} ({cut_ms} ms in total)");
        let pct = |a: u32, b: u32| if b == 0 { 0.0 } else { 100.0 * a as f64 / b as f64 };
        println!(
            "{label} | speech sent: {:5.1}% ({sp}) | noise-only sent: {:5.1}% ({nz}) | silence sent: {:5.1}% ({sil}) | CPU {:.1}% of a core",
            pct(sp_sent, sp),
            pct(nz_sent, nz),
            pct(sil_sent, sil),
            100.0 * cpu.as_secs_f64() / (n as f64 / 48_000.0)
        );
        if let (Some(p), true) = (&out_path, denoise && sens == backroom_client::config::Sensitivity::Normal) {
            let spec = hound::WavSpec { channels: 1, sample_rate: 48_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
            let mut w = hound::WavWriter::create(p, spec).unwrap();
            for s in &heard {
                w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16).unwrap();
            }
            let mut w2 = hound::WavWriter::create(p.replace(".wav", "_input.wav"), spec).unwrap();
            for s in &mix {
                w2.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16).unwrap();
            }
        }
    }
}
