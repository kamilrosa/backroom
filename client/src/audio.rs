//! Audio engine: a capture thread and a playback thread, each with a watchdog.
//!
//! The key difference from Mumble: a device is not opened "once and for
//! all". Every 250 ms the watchdog checks whether the sound card is really
//! calling our callback, whether it reported an error and whether the system
//! default device changed — and reopens the device itself when needed.
//! Reconnecting to the server is not required for that.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use crossbeam_channel::{Receiver, Sender};
use backroom_proto::{self as proto, UserId, FRAME_SAMPLES};
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use crate::dsp::{self, Framer, Resampler, HOP};
use crate::jitter::{JitterBuffer, Stats};
use crate::mic::{MicChain, Packet};
use crate::state::{MicTest, Shared, MIC_TEST_LEN};

/// A voice packet to play (from the network).
pub struct Incoming {
    pub sender: UserId,
    pub seq: u32,
    pub flags: u8,
    pub data: Vec<u8>,
}

const WATCHDOG_EVERY: Duration = Duration::from_millis(250);
/// No callback for longer than this = the device has "frozen".
const STALL_AFTER: Duration = Duration::from_millis(1500);
const DEFAULT_CHECK_EVERY: Duration = Duration::from_secs(2);
const LIST_EVERY: Duration = Duration::from_secs(3);
const RETRY_OPEN_EVERY: Duration = Duration::from_secs(1);
/// This much digital silence (all zeros) = microphone muted/blocked.
const DIGITAL_SILENCE: Duration = Duration::from_secs(3);
const SILENCE_RESTART_EVERY: Duration = Duration::from_secs(15);
/// A pause this short between two utterances of one person is probably a cut
/// in the middle of a sentence (the sender's gate closed), so it is logged.
const SHORT_GAP: Duration = Duration::from_millis(300);

/// Whether something last done at `last` (`None` = never) is due again.
fn due(last: Option<Instant>, every: Duration) -> bool {
    last.is_none_or(|t| t.elapsed() >= every)
}

fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn device_name(d: &cpal::Device) -> String {
    d.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "unknown device".into())
}

#[derive(Clone, Copy, PartialEq)]
enum Dir {
    Input,
    Output,
}

fn list_devices(host: &cpal::Host, dir: Dir) -> Vec<String> {
    let devs = match dir {
        Dir::Input => host.input_devices().map(|d| d.collect::<Vec<_>>()),
        Dir::Output => host.output_devices().map(|d| d.collect::<Vec<_>>()),
    };
    let mut names: Vec<String> = devs.unwrap_or_default().iter().map(device_name).collect();
    names.dedup();
    names
}

fn default_device(host: &cpal::Host, dir: Dir) -> Option<cpal::Device> {
    match dir {
        Dir::Input => host.default_input_device(),
        Dir::Output => host.default_output_device(),
    }
}

/// The selected device or the default one. Second element: whether we follow
/// the default device; third: a warning for the user.
fn pick_device(host: &cpal::Host, dir: Dir, choice: Option<&str>) -> Option<(cpal::Device, bool, String)> {
    if let Some(name) = choice {
        let found = match dir {
            Dir::Input => host.input_devices().ok().and_then(|mut d| d.find(|d| device_name(d) == name)),
            Dir::Output => host.output_devices().ok().and_then(|mut d| d.find(|d| device_name(d) == name)),
        };
        if let Some(d) = found {
            return Some((d, false, String::new()));
        }
        return default_device(host, dir)
            .map(|d| (d, true, format!("\"{name}\" unavailable — using the default")));
    }
    default_device(host, dir).map(|d| (d, true, String::new()))
}

/// Shared watchdog state for an open stream.
struct Watch {
    last_cb: Arc<AtomicU64>,
    error: Arc<Mutex<Option<String>>>,
    /// Start of the "no callback" clock — set after `play()`, because opening
    /// alone can take seconds (e.g. macOS asking for permission).
    opened: Instant,
    name: String,
    follow_default: bool,
    gen: u64,
}

impl Watch {
    fn new(name: String, follow_default: bool, gen: u64) -> Self {
        Self {
            last_cb: Arc::new(AtomicU64::new(now_ms())),
            error: Arc::new(Mutex::new(None)),
            opened: Instant::now(),
            name,
            follow_default,
            gen,
        }
    }

    fn arm(&mut self) {
        self.opened = Instant::now();
        self.last_cb.store(now_ms(), Ordering::Relaxed);
    }

    fn error_callback(&self, what: &'static str) -> impl FnMut(cpal::Error) + Send + 'static {
        let error = self.error.clone();
        move |e: cpal::Error| match e.kind() {
            // The stream switched to the new default device by itself.
            cpal::ErrorKind::DeviceChanged => info!("{what}: the system switched devices"),
            cpal::ErrorKind::Xrun => {}
            _ => {
                warn!("{what}: stream error: {e}");
                *error.lock() = Some(e.to_string());
            }
        }
    }

    /// The restart reason, if the stream has to be reopened.
    fn check(&self, shared: &Shared, host: &cpal::Host, dir: Dir, check_default: bool) -> Option<String> {
        if let Some(e) = self.error.lock().take() {
            return Some(format!("device error ({e})"));
        }
        if shared.device_gen.load(Ordering::Relaxed) != self.gen {
            return Some("audio settings changed".into());
        }
        let since_cb = now_ms().saturating_sub(self.last_cb.load(Ordering::Relaxed));
        if self.opened.elapsed() > STALL_AFTER && since_cb > STALL_AFTER.as_millis() as u64 {
            return Some(format!("device not responding for {:.1} s", since_cb as f64 / 1000.0));
        }
        if check_default && self.follow_default {
            if let Some(d) = default_device(host, dir) {
                let n = device_name(&d);
                if n != self.name {
                    return Some(format!("new default device: {n}"));
                }
            }
        }
        None
    }
}

fn stream_config(dev: &cpal::Device, dir: Dir) -> Result<(cpal::StreamConfig, SampleFormat), String> {
    let sc = match dir {
        Dir::Input => dev.default_input_config(),
        Dir::Output => dev.default_output_config(),
    }
    .map_err(|e| e.to_string())?;
    let mut cfg = sc.config();
    cfg.buffer_size = cpal::BufferSize::Default;
    Ok((cfg, sc.sample_format()))
}

macro_rules! dispatch_format {
    ($fmt:expr, $f:ident, $($arg:expr),*) => {
        match $fmt {
            SampleFormat::F32 => $f::<f32>($($arg),*),
            SampleFormat::I16 => $f::<i16>($($arg),*),
            SampleFormat::U16 => $f::<u16>($($arg),*),
            SampleFormat::I32 => $f::<i32>($($arg),*),
            SampleFormat::U8 => $f::<u8>($($arg),*),
            SampleFormat::I8 => $f::<i8>($($arg),*),
            SampleFormat::F64 => $f::<f64>($($arg),*),
            other => Err(format!("unsupported sample format {other:?}")),
        }
    };
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

struct OpenInput {
    stream: cpal::Stream,
    cons: rtrb::Consumer<f32>,
    rate: u32,
    watch: Watch,
}

fn build_input<T>(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    mut prod: rtrb::Producer<f32>,
    watch: &Watch,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = cfg.channels.max(1) as usize;
    let last_cb = watch.last_cb.clone();
    dev.build_input_stream::<T, _, _>(
        *cfg,
        move |data: &[T], _| {
            last_cb.store(now_ms(), Ordering::Relaxed);
            // Mix down to mono; when the buffer is full (the processing thread
            // is falling behind) samples are dropped — better than blocking audio.
            for frame in data.chunks(ch) {
                let s = frame.iter().map(|&x| f32::from_sample_(x)).sum::<f32>() / ch as f32;
                let _ = prod.push(s);
            }
        },
        watch.error_callback("microphone"),
        Some(Duration::from_secs(3)),
    )
    .map_err(|e| e.to_string())
}

fn open_input(host: &cpal::Host, shared: &Shared) -> Result<OpenInput, String> {
    let gen = shared.device_gen.load(Ordering::Relaxed);
    let choice = shared.input_choice.lock().clone();
    let (dev, follow, note) = pick_device(host, Dir::Input, choice.as_deref()).ok_or("no microphone in the system")?;
    let (cfg, fmt) = stream_config(&dev, Dir::Input)?;
    let (prod, cons) = rtrb::RingBuffer::new(cfg.sample_rate as usize);
    let mut watch = Watch::new(device_name(&dev), follow, gen);
    let stream = dispatch_format!(fmt, build_input, &dev, &cfg, prod, &watch)?;
    stream.play().map_err(|e| e.to_string())?;
    watch.arm();
    info!("Microphone: {} ({} Hz, {} ch, {fmt:?})", watch.name, cfg.sample_rate, cfg.channels);
    shared.update(|s| {
        s.input.name = watch.name.clone();
        s.input.ok = true;
        s.input.message = note;
    });
    Ok(OpenInput { stream, cons, rate: cfg.sample_rate, watch })
}

pub fn spawn_capture(
    shared: Arc<Shared>,
    voice_tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    clip_tx: Sender<Vec<f32>>,
) {
    std::thread::Builder::new()
        .name("backroom-mic".into())
        .spawn(move || capture_thread(shared, voice_tx, clip_tx))
        .expect("microphone thread");
}

fn capture_thread(shared: Arc<Shared>, voice_tx: tokio::sync::mpsc::Sender<Vec<u8>>, clip_tx: Sender<Vec<f32>>) {
    let host = cpal::default_host();
    let mut input: Option<OpenInput> = None;
    let mut last_try = None;
    let mut last_watch = Instant::now();
    let mut last_default_check = Instant::now();
    let mut last_list = None;
    let mut last_silence_restart = None;

    let mut chain = MicChain::new();
    if !chain.denoise_available() {
        shared.update(|s| s.denoise_available = false);
    }
    let mut resampler = Resampler::new(48_000, 48_000);
    let mut framer = Framer::new(HOP);
    let mut raw = Vec::with_capacity(8192);
    let mut res = Vec::with_capacity(8192);
    let mut hop = [0f32; HOP];
    let mut packets = Vec::new();
    let mut seq = 0u32;
    let mut zero_run = Duration::ZERO;
    let mut rec: Option<Recorder> = None;

    loop {
        if input.is_none() && due(last_try, RETRY_OPEN_EVERY) {
            last_try = Some(Instant::now());
            match open_input(&host, &shared) {
                Ok(i) => {
                    resampler = Resampler::new(i.rate, 48_000);
                    framer.clear();
                    chain.reset_framing();
                    zero_run = Duration::ZERO;
                    input = Some(i);
                }
                Err(e) => shared.update(|s| {
                    if s.input.ok || s.input.message != e {
                        warn!("Microphone: {e}");
                    }
                    s.input.ok = false;
                    s.input.message = e;
                }),
            }
        }

        if let Some(inp) = input.as_mut() {
            raw.clear();
            while let Ok(s) = inp.cons.pop() {
                raw.push(s);
            }
            res.clear();
            resampler.process(&raw, &mut res);
            framer.push(&res);
        }

        while framer.pop(&mut hop) {
            if hop.iter().all(|&s| s == 0.0) {
                zero_run += Duration::from_millis(10);
            } else {
                zero_run = Duration::ZERO;
            }
            if chain.push_hop(&shared, &mut hop, &mut packets) {
                route_frame(&shared, &mut packets, &mut rec, &mut seq, &voice_tx, &clip_tx);
            }
        }

        if last_watch.elapsed() >= WATCHDOG_EVERY {
            last_watch = Instant::now();
            let check_default = last_default_check.elapsed() >= DEFAULT_CHECK_EVERY;
            if check_default {
                last_default_check = Instant::now();
            }
            let mut reason = input.as_ref().and_then(|i| i.watch.check(&shared, &host, Dir::Input, check_default));
            if reason.is_none() && input.is_some() && zero_run >= DIGITAL_SILENCE {
                shared.update(|s| {
                    s.input.message =
                        "The microphone is sending pure silence — muted in the system or missing permission?".into()
                });
                if due(last_silence_restart, SILENCE_RESTART_EVERY) {
                    last_silence_restart = Some(Instant::now());
                    reason = Some("microphone is sending pure silence".into());
                }
            } else if zero_run.is_zero() && input.is_some() {
                shared.update(|s| {
                    if s.input.message.starts_with("The microphone is sending pure silence") {
                        s.input.message.clear();
                    }
                });
            }
            if let Some(reason) = reason {
                restart_note(&shared, Dir::Input, &reason);
                if let Some(old) = input.take() {
                    drop(old.stream);
                }
                last_try = None; // reopen right away
                // Any ongoing utterance gets cut — close it properly.
                chain.force_close(&mut packets);
            }
            if due(last_list, LIST_EVERY) {
                last_list = Some(Instant::now());
                let list = list_devices(&host, Dir::Input);
                shared.status.lock().input_devices = list;
            }
        }

        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Where packets from a just-completed 20 ms frame go: to the server, or —
/// during the mic test — into the recording.
fn route_frame(
    shared: &Shared,
    packets: &mut Vec<Packet>,
    rec: &mut Option<Recorder>,
    seq: &mut u32,
    voice_tx: &tokio::sync::mpsc::Sender<Vec<u8>>,
    clip_tx: &Sender<Vec<f32>>,
) {
    let state = *shared.mic_test.lock();
    match state {
        MicTest::Recording { started } => {
            let r = rec.get_or_insert_with(Recorder::new);
            r.add_frame(packets);
            packets.clear();
            if started.elapsed() >= MIC_TEST_LEN {
                let (pcm, sent_pct) = rec.take().unwrap().finish();
                let until = Instant::now() + Duration::from_secs_f32(pcm.len() as f32 / 48_000.0 + 0.3);
                *shared.mic_test.lock() = MicTest::Playing { until, sent_pct };
                let _ = clip_tx.try_send(pcm);
                shared.repaint();
            }
        }
        // Playing the recording back: do not transmit, so the speakers do not leak to others.
        MicTest::Playing { .. } => packets.clear(),
        MicTest::Idle | MicTest::Done { .. } => {
            *rec = None;
            for p in packets.drain(..) {
                *seq = seq.wrapping_add(1);
                // Full queue = no connection; the packet would be stale anyway.
                let _ = voice_tx.try_send(proto::encode_voice_up(p.flags, *seq, &p.data));
            }
        }
    }
}

/// Test recording: decodes exactly the packets that would go to others and
/// inserts silence for frames that were not sent (gate closed).
struct Recorder {
    dec: opus::Decoder,
    pcm: Vec<f32>,
    frames: u32,
    sent: u32,
}

impl Recorder {
    fn new() -> Self {
        Self {
            dec: opus::Decoder::new(48_000, opus::Channels::Mono).expect("Opus decoder"),
            pcm: Vec::with_capacity(48_000 * 6),
            frames: 0,
            sent: 0,
        }
    }

    fn add_frame(&mut self, packets: &[Packet]) {
        self.frames += 1;
        if packets.is_empty() {
            self.pcm.extend_from_slice(&[0.0; FRAME_SAMPLES]);
            return;
        }
        self.sent += 1;
        let mut buf = [0f32; FRAME_SAMPLES];
        for p in packets {
            let n = self.dec.decode_float(&p.data, &mut buf, false).unwrap_or(0);
            self.pcm.extend_from_slice(&buf[..n]);
        }
    }

    fn finish(self) -> (Vec<f32>, u8) {
        let pct = (100 * self.sent / self.frames.max(1)) as u8;
        (self.pcm, pct)
    }
}

fn restart_note(shared: &Shared, dir: Dir, reason: &str) {
    let what = if dir == Dir::Input { "Microphone" } else { "Speakers" };
    warn!("{what}: reopening — {reason}");
    shared.update(|s| {
        let d = if dir == Dir::Input { &mut s.input } else { &mut s.output };
        d.ok = false;
        d.restarts += 1;
        d.last_restart = Some((Instant::now(), reason.to_string()));
    });
}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

struct OpenOutput {
    stream: cpal::Stream,
    prod: rtrb::Producer<f32>,
    capacity: usize,
    rate: u32,
    callback_frames: Arc<AtomicUsize>,
    watch: Watch,
}

fn build_output<T>(
    dev: &cpal::Device,
    cfg: &cpal::StreamConfig,
    mut cons: rtrb::Consumer<f32>,
    callback_frames: Arc<AtomicUsize>,
    watch: &Watch,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = cfg.channels.max(1) as usize;
    let last_cb = watch.last_cb.clone();
    dev.build_output_stream::<T, _, _>(
        *cfg,
        move |data: &mut [T], _| {
            last_cb.store(now_ms(), Ordering::Relaxed);
            callback_frames.store(data.len() / ch, Ordering::Relaxed);
            for frame in data.chunks_mut(ch) {
                let s = cons.pop().unwrap_or(0.0);
                let v = T::from_sample_(s);
                frame.fill(v);
            }
        },
        watch.error_callback("speakers"),
        Some(Duration::from_secs(3)),
    )
    .map_err(|e| e.to_string())
}

fn open_output(host: &cpal::Host, shared: &Shared) -> Result<OpenOutput, String> {
    let gen = shared.device_gen.load(Ordering::Relaxed);
    let choice = shared.output_choice.lock().clone();
    let (dev, follow, note) = pick_device(host, Dir::Output, choice.as_deref()).ok_or("no speakers in the system")?;
    let (cfg, fmt) = stream_config(&dev, Dir::Output)?;
    let capacity = cfg.sample_rate as usize / 2; // 500 ms
    let (prod, cons) = rtrb::RingBuffer::new(capacity);
    let callback_frames = Arc::new(AtomicUsize::new(0));
    let mut watch = Watch::new(device_name(&dev), follow, gen);
    let stream = dispatch_format!(fmt, build_output, &dev, &cfg, cons, callback_frames.clone(), &watch)?;
    stream.play().map_err(|e| e.to_string())?;
    watch.arm();
    info!("Speakers: {} ({} Hz, {} ch, {fmt:?})", watch.name, cfg.sample_rate, cfg.channels);
    shared.update(|s| {
        s.output.name = watch.name.clone();
        s.output.ok = true;
        s.output.message = note;
    });
    Ok(OpenOutput { stream, prod, capacity, rate: cfg.sample_rate, callback_frames, watch })
}

pub fn spawn_playback(shared: Arc<Shared>, rx: Receiver<Incoming>, clip_rx: Receiver<Vec<f32>>) {
    std::thread::Builder::new()
        .name("backroom-out".into())
        .spawn(move || playback_thread(shared, rx, clip_rx))
        .expect("speaker thread");
}

struct Peer {
    jb: JitterBuffer,
    last_packet: Instant,
    /// Start of the utterance being played and the stats at that moment.
    talk: Option<(Instant, Stats)>,
    idle_since: Option<Instant>,
}

impl Peer {
    fn new(now: Instant) -> Self {
        Self { jb: JitterBuffer::new(), last_packet: now, talk: None, idle_since: None }
    }

    /// Logs utterance boundaries, short gaps and losses — the data needed
    /// when someone reports that a voice cuts out.
    fn track(&mut self, id: UserId, shared: &Shared, now: Instant) {
        let playing = self.jb.is_playing();
        let name = || shared.status.lock().users.get(&id).map_or_else(|| format!("#{id}"), |u| u.info.name.clone());
        match (self.talk, playing) {
            (None, true) => {
                if let Some(gap) = self.idle_since.take().map(|t| now - t) {
                    if gap < SHORT_GAP {
                        info!("Voice from {}: resumed after only {} ms", name(), gap.as_millis());
                    }
                }
                self.talk = Some((now, self.jb.stats));
            }
            (Some((start, s0)), false) => {
                let s = self.jb.stats;
                let (plc, fec, late, skipped) =
                    (s.plc - s0.plc, s.fec - s0.fec, s.late - s0.late, s.dropped_for_latency - s0.dropped_for_latency);
                let msg = format!(
                    "Voice from {}: {:.1} s, concealed {plc} (FEC {fec}), late {late}, skipped {skipped}, underruns {}",
                    name(),
                    (now - start).as_secs_f32(),
                    s.underruns - s0.underruns
                );
                if plc + fec + late + skipped > 0 {
                    info!("{msg}");
                } else {
                    debug!("{msg}");
                }
                self.talk = None;
                self.idle_since = Some(now);
            }
            _ => {}
        }
    }
}

/// Adds the next frame of a prerecorded sound to `mix`; false once it has ended.
fn mix_next(mix: &mut [f32], pcm: &[f32], pos: &mut usize, gain: f32) -> bool {
    let end = (*pos + mix.len()).min(pcm.len());
    for (m, s) in mix.iter_mut().zip(&pcm[*pos..end]) {
        *m += s * gain;
    }
    *pos = end;
    end < pcm.len()
}

/// The new chat message sound: two short bell-like tones, 48 kHz.
fn chime() -> Vec<f32> {
    const RATE: f32 = 48_000.0;
    let tone = |f: f32, t: f32| {
        if t < 0.0 {
            return 0.0;
        }
        // 2.5 ms fade-in against a click, then an exponential decay.
        (std::f32::consts::TAU * f * t).sin() * (t * 400.0).min(1.0) * (-t * 14.0).exp()
    };
    (0..(0.45 * RATE) as usize)
        .map(|i| {
            let t = i as f32 / RATE;
            0.18 * (tone(880.0, t) + tone(1318.5, t - 0.09))
        })
        .collect()
}

fn playback_thread(shared: Arc<Shared>, rx: Receiver<Incoming>, clip_rx: Receiver<Vec<f32>>) {
    let host = cpal::default_host();
    let mut output: Option<OpenOutput> = None;
    let mut last_try = None;
    let mut last_watch = Instant::now();
    let mut last_default_check = Instant::now();
    let mut last_list = None;
    let mut resampler = Resampler::new(48_000, 48_000);
    let mut peers: std::collections::HashMap<UserId, Peer> = Default::default();
    let mut mix = [0f32; FRAME_SAMPLES];
    let mut one = [0f32; FRAME_SAMPLES];
    let mut out = Vec::with_capacity(4096);
    // Mic test recording and its playback position.
    let mut clip: Option<(Vec<f32>, usize)> = None;
    let chime_pcm = chime();
    let mut chime_pos: Option<usize> = None;

    loop {
        if output.is_none() && due(last_try, RETRY_OPEN_EVERY) {
            last_try = Some(Instant::now());
            match open_output(&host, &shared) {
                Ok(o) => {
                    resampler = Resampler::new(48_000, o.rate);
                    output = Some(o);
                }
                Err(e) => shared.update(|s| {
                    if s.output.ok || s.output.message != e {
                        warn!("Speakers: {e}");
                    }
                    s.output.ok = false;
                    s.output.message = e;
                }),
            }
        }

        // Receive packets — wait at most 5 ms, which also paces the loop.
        let deafened = shared.deafened.load(Ordering::Relaxed);
        let now = Instant::now();
        let first = rx.recv_timeout(Duration::from_millis(5)).ok();
        for p in first.into_iter().chain(rx.try_iter()) {
            if deafened {
                continue;
            }
            let peer = peers.entry(p.sender).or_insert_with(|| Peer::new(now));
            peer.last_packet = now;
            peer.jb.push(p.seq, p.flags, &p.data, now);
        }
        if deafened {
            peers.clear();
        }
        if let Ok(pcm) = clip_rx.try_recv() {
            clip = Some((pcm, 0));
        }
        if clip.is_some() && !matches!(*shared.mic_test.lock(), MicTest::Playing { .. }) {
            clip = None; // cancelled
        }
        if shared.chime.swap(false, Ordering::Relaxed) {
            chime_pos = Some(0);
        }

        if let Some(o) = output.as_mut() {
            // Keep about 40 ms in the card's buffer (or 2 callbacks if the
            // system calls less often) — less = crackles, more = latency.
            let cb = o.callback_frames.load(Ordering::Relaxed);
            let target = (o.rate as usize * 40 / 1000).max(cb * 2).min(o.capacity / 2);
            let gains = shared.gains.lock().clone();
            let master = shared.master_volume.get();
            while o.capacity - o.prod.slots() < target {
                mix.fill(0.0);
                let now = Instant::now();
                if let Some((pcm, pos)) = clip.as_mut() {
                    mix_next(&mut mix, pcm, pos, master);
                }
                if let Some(pos) = chime_pos.as_mut() {
                    if !mix_next(&mut mix, &chime_pcm, pos, master) {
                        chime_pos = None;
                    }
                }
                for (id, peer) in peers.iter_mut() {
                    if peer.jb.pop(&mut one, now) {
                        let g = gains.get(id).copied().unwrap_or(1.0) * master;
                        for (m, s) in mix.iter_mut().zip(one.iter()) {
                            *m += s * g;
                        }
                    }
                    peer.track(*id, &shared, now);
                }
                for m in mix.iter_mut() {
                    *m = dsp::soft_clip(*m);
                }
                out.clear();
                resampler.process(&mix, &mut out);
                for &s in &out {
                    if o.prod.push(s).is_err() {
                        break;
                    }
                }
            }
        }
        peers.retain(|_, p| p.last_packet.elapsed() < Duration::from_secs(30));

        if last_watch.elapsed() >= WATCHDOG_EVERY {
            last_watch = Instant::now();
            let check_default = last_default_check.elapsed() >= DEFAULT_CHECK_EVERY;
            if check_default {
                last_default_check = Instant::now();
            }
            let reason = output.as_ref().and_then(|o| o.watch.check(&shared, &host, Dir::Output, check_default));
            if let Some(reason) = reason {
                restart_note(&shared, Dir::Output, &reason);
                if let Some(old) = output.take() {
                    drop(old.stream);
                }
                last_try = None; // reopen right away
            }
            if due(last_list, LIST_EVERY) {
                last_list = Some(Instant::now());
                let list = list_devices(&host, Dir::Output);
                shared.status.lock().output_devices = list;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn packet() -> Packet {
        let mut e = opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let mut buf = [0u8; 400];
        let frame: Vec<f32> = (0..FRAME_SAMPLES).map(|i| (i as f32 * 0.05).sin() * 0.3).collect();
        let n = e.encode_float(&frame, &mut buf).unwrap();
        Packet { flags: 0, data: buf[..n].to_vec() }
    }

    #[test]
    fn mic_test_records_locally_then_plays() {
        let shared = Shared::new(&Config::default());
        let (voice_tx, mut voice_rx) = tokio::sync::mpsc::channel(64);
        let (clip_tx, clip_rx) = crossbeam_channel::bounded(2);
        let mut rec = None;
        let mut seq = 0;

        // Normal operation: the packet goes to the server.
        let mut p = vec![packet()];
        route_frame(&shared, &mut p, &mut rec, &mut seq, &voice_tx, &clip_tx);
        assert!(voice_rx.try_recv().is_ok());

        // Recording: 3 voice frames, 1 silent, then time runs out.
        *shared.mic_test.lock() = MicTest::Recording { started: Instant::now() };
        for frame in [vec![packet()], vec![], vec![packet()], vec![packet()]] {
            let mut p = frame;
            route_frame(&shared, &mut p, &mut rec, &mut seq, &voice_tx, &clip_tx);
        }
        assert!(voice_rx.try_recv().is_err(), "nothing goes to others during the test");
        *shared.mic_test.lock() = MicTest::Recording { started: Instant::now() - MIC_TEST_LEN };
        route_frame(&shared, &mut vec![], &mut rec, &mut seq, &voice_tx, &clip_tx);

        let pcm = clip_rx.try_recv().expect("recording handed over for playback");
        assert_eq!(pcm.len(), 5 * FRAME_SAMPLES);
        assert!(matches!(*shared.mic_test.lock(), MicTest::Playing { sent_pct: 60, .. }));

        // No transmitting during playback either (speakers → mic → friends).
        let mut p = vec![packet()];
        route_frame(&shared, &mut p, &mut rec, &mut seq, &voice_tx, &clip_tx);
        assert!(voice_rx.try_recv().is_err());
    }
}
