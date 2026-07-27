extern crate alloc;

use crate::audio_hal::{self, SpeakerAmp};
use crate::bus::{AudioCommand, AUDIO_COMMAND, EVENTS, SharedI2c};
use app_core::events::SystemEvent;
use drivers::es8311::Es8311;
use drivers::es7210::{Es7210, MicGain};
use embassy_futures::select::{select, select3, Either, Either3};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::{
    dma::{DmaChannelFor, DmaDescriptor},
    gpio::interconnect::{PeripheralInput, PeripheralOutput},
    i2s::AnyI2s,
};


// ---- Audio task: alarm tone (TX), mic capture (RX), speaker-test ----------
// ---- tone sweep (TX) and mic->speaker loopback (full duplex) --------------

/// Alert-tone pitch. A square wave near 880 Hz (A5) - bright and
/// audible over ambient noise on the small NS4150-driven speaker.
const TONE_HZ: u32 = 880;

/// Square-wave amplitude (i16 full-scale is 32767). Kept well below
/// full-scale: the codec output is already attenuated and the small
/// speaker distorts near the rails.
const TONE_AMPLITUDE: i16 = 0x1800;

/// Beep cadence: `BEEP_ON_MS` of tone then `BEEP_GAP_MS` of silence,
/// repeating. Deliberately distinct from the 200/100 ms haptic buzz it
/// sounds alongside.
const BEEP_ON_MS: u64 = 400;
const BEEP_GAP_MS: u64 = 200;

/// Speaker-test tone sweep, played once per `PlayTones` command:
/// A4, ~1 kHz, A5 - the classic factory-test sequence.
const TEST_TONES_HZ: [u32; 3] = [440, 1000, 880];
/// Duration of each tone in the sweep.
const TEST_TONE_MS: u64 = 1000;

/// Bin-side TX circular DMA ring size in bytes (**must match the
/// bins' `dma_circular_buffers!` TX size**). Pushing this many bytes
/// guarantees everything previously queued has been consumed by the
/// DMA, and overwriting it with silence stops the ring's endless
/// replay of its last contents.
const TX_RING_BYTES: usize = 4096;

/// Lead-in silence streamed before the first test tone. Covers the
/// stale-ring lap (~64 ms of whatever the previous session left in
/// the TX ring) and the ES8311's DAC unmute ramp, both of which were
/// swallowing the start of the 440 Hz tone. The amp is enabled only
/// after this has drained.
const TONE_LEAD_MS: u64 = 150;

/// LOOP-test parrot recording buffer size in bytes. The recording is
/// stored mono at 8 kHz (adjacent-frame average of the left mic -
/// telephone quality, 4x smaller than the stereo 16 kHz wire format),
/// so 16 000 bytes is 1.0 s of speech. Heap-bounded: it lives
/// alongside the 32 KB pop buffer plus baseline heap use (~13 KB
/// measured, more under load - 48 KB of audio buffers OOM'd the C6's
/// old 64 KB heap; both bins run 128 KB now). The record and playback
/// phases both drain the RX ring continuously, so unlike the buffer
/// size, the *duration* has no DMA-imposed ceiling.
const PARROT_RECORD_BYTES: usize = 16_000;

/// Mic-capture pop-buffer size (bytes).
///
/// **Must be >= the bin-side RX DMA buffer size** (currently 32 KB).
/// `RxCircularState::pop` requires the caller's buffer to hold ALL
/// accumulated data - `if avail > data.len() { return BufferTooSmall }`
/// - so a smaller pop buf than the DMA buf produces a persistent
/// `DmaError::BufferTooSmall` that never recovers (zero bytes consumed,
/// `available` stays high, every subsequent pop also fails). Both must
/// move together.
///
/// At 16 kHz / 16-bit stereo this is ~512 ms of capture per pop, which
/// is plenty - we still pop in a tight loop, this is just the upper
/// bound the API forces us to allocate.
const CAPTURE_CHUNK_BYTES: usize = 32_768;

/// Minimum change in computed level before emitting a fresh
/// `MicLevel`, so a steady-state room doesn't spam the event channel
/// (and trigger redraws) on every chunk.
const MIC_LEVEL_DELTA: u8 = 4;

/// Minimum interval between emitted `MicLevel` events. Each one drives
/// a full-screen settings redraw, so this caps the meter at ~5 Hz -
/// matching the MOTION sub-view's proven-safe redraw cadence. Emitting
/// per chunk (~15 Hz) saturates the render loop and locks out touch.
const MIC_EMIT_MS: u64 = 200;

/// ES7210 analog input gain. 30 dB is the esp-bsp / esp_codec_dev
/// reference default for this ADC and is usable now that the modulator
/// clock is correct (the earlier "30 dB clips on ambient" reading was
/// the misclocked modulator's own noise, not real signal).
const MIC_GAIN: MicGain = MicGain::Db30;

/// Resting mic noise floor (mean-abs units) gated out so a quiet room
/// reads ~0 instead of a jittery baseline. Calibrated at 30 dB gain:
/// quiet-room `smoothed` reads ~95-125 with bumps to ~145, so the
/// gate sits just above those. Raise it if the bar idles above zero,
/// lower it if quiet speech is swallowed.
const MIC_NOISE_FLOOR: u32 = 200;

/// Mean-abs amplitude (after the floor gate) that maps to a full bar.
/// Calibrated at 30 dB gain, wrist distance: normal speech `smoothed`
/// peaks ~3300-4900, louder speech ~5300-7000. Set so normal speech
/// lands mid-bar, louder reaches the top and anything above pegs.
/// Lower = more sensitive meter.
const MIC_FULL_SCALE: u32 = 7500;

/// Fill `buf` (length a multiple of 4 = stereo i16 frames) with one
/// stretch of a `tone_hz` square wave. `phase` is the running sample
/// index, carried across calls so the waveform stays continuous
/// between DMA chunks. When `audible` is false the chunk is silence
/// (the gap in the beep).
fn fill_tone(buf: &mut [u8], phase: &mut u32, audible: bool, tone_hz: u32) {
    // Samples per half-period of the square wave.
    let half_period = (audio_hal::SAMPLE_RATE_HZ / (tone_hz * 2)).max(1);
    let frames = buf.len() / 4;
    for i in 0..frames {
        let sample: i16 = if !audible {
            0
        } else if (*phase / half_period) % 2 == 0 {
            TONE_AMPLITUDE
        } else {
            -TONE_AMPLITUDE
        };
        *phase = phase.wrapping_add(1);
        let [lo, hi] = sample.to_le_bytes();
        let base = i * 4;
        // Same sample on both channels (mono tone on a stereo bus).
        buf[base] = lo;
        buf[base + 1] = hi;
        buf[base + 2] = lo;
        buf[base + 3] = hi;
    }
}

/// Stream the beeping tone into the circular DMA buffer until an
/// interrupting command arrives on [`AUDIO_COMMAND`]. `push_with`
/// blocks until the DMA frees space, so this paces itself to the
/// 16 kHz sample rate with no separate timer. Returns the command the
/// main loop should handle next (e.g. a `StartCapture` that interrupted
/// playback), or `None` if playback was simply stopped.
async fn play_until_interrupt<B>(
    tx: &mut esp_hal::i2s::master::asynch::I2sWriteDmaTransferAsync<'_, B>,
    amp: &mut SpeakerAmp<'_>,
    phase: &mut u32,
) -> Option<AudioCommand> {
    let start = Instant::now();
    loop {
        let audible =
            (start.elapsed().as_millis() % (BEEP_ON_MS + BEEP_GAP_MS)) < BEEP_ON_MS;
        let fill = tx.push_with(|buf| {
            // DMA-available length isn't guaranteed frame-aligned; fill
            // only whole frames and leave any 1-3 trailing bytes for
            // the next push.
            let frames = buf.len() / 4;
            fill_tone(&mut buf[..frames * 4], phase, audible, TONE_HZ);
            frames * 4
        });
        match select(fill, AUDIO_COMMAND.receive()).await {
            Either::First(_) => {} // chunk enqueued; keep streaming
            Either::Second(cmd) => match cmd {
                AudioCommand::StopAlarm => {
                    amp.disable();
                    return None;
                }
                // Not ours - the mic-test modes aren't running while
                // we play. The stops are mode-specific so the command
                // streams can't cancel each other: a trailing
                // StopCapture (e.g. the model's leave-screen safety
                // net firing right after an alarm interrupted the mic
                // test) must not silence the alarm.
                AudioCommand::StopCapture
                | AudioCommand::StopTones
                | AudioCommand::StopLoopback => {}
                AudioCommand::PlayAlarm => {} // already playing
                // Mic-test starts are explicit user actions; hand the
                // I2S over (speaker muted first).
                cmd @ (AudioCommand::StartCapture
                | AudioCommand::PlayTones
                | AudioCommand::StartLoopback) => {
                    amp.disable();
                    return Some(cmd);
                }
            },
        }
    }
}

/// Per-chunk signal statistics over one stereo chunk's first slot.
///
/// `mean_abs` (average rectified amplitude, 0..=32767) drives the
/// level meter - the original, hardware-tuned metric. Averaging across
/// the whole chunk tracks loudness far more stably than a
/// single-sample peak, which spikes on any transient and reads as
/// noise.
///
/// The rest are diagnostics for the `mic:` log only - nothing
/// behavioral reads them. Together they separate failure modes that
/// all look like "meter stuck" on the bar: an ADC that isn't streaming
/// (every field 0), a floating data line (huge erratic values),
/// clipping (min/max pinned at the i16 rails), and a DC pedestal
/// (`dev` measures deviation from the chunk mean, so DC inflates
/// `mean_abs` but not `dev` - mean_abs >> dev with a large stable
/// `mean` confirms DC; mean_abs ~= dev rules it out).
struct MicStats {
    mean_abs: u32,
    dev: u32,
    mean: i32,
    min: i16,
    max: i16,
}

fn mic_stats(buf: &[u8]) -> MicStats {
    let mut sum: i64 = 0;
    let mut abs_sum: u64 = 0;
    let mut frames: i64 = 0;
    let mut min: i16 = i16::MAX;
    let mut max: i16 = i16::MIN;
    for frame in buf.chunks_exact(4) {
        let sample = i16::from_le_bytes([frame[0], frame[1]]);
        sum += sample as i64;
        abs_sum += (sample as i64).unsigned_abs();
        min = min.min(sample);
        max = max.max(sample);
        frames += 1;
    }
    if frames == 0 {
        return MicStats { mean_abs: 0, dev: 0, mean: 0, min: 0, max: 0 };
    }
    let mean = sum / frames;
    let mut dev: u64 = 0;
    for frame in buf.chunks_exact(4) {
        let sample = i16::from_le_bytes([frame[0], frame[1]]) as i64;
        dev += (sample - mean).unsigned_abs();
    }
    MicStats {
        mean_abs: (abs_sum / frames as u64) as u32,
        dev: (dev / frames as u64) as u32,
        mean: mean as i32,
        min,
        max,
    }
}

/// Digital makeup gain for the PDM-mic capture path, as a left-shift
/// (3 = x8 = +18 dB). The codec boards' capture runs through the
/// ADC's +30 dB analog PGA before quantization; a raw PDM mic has no
/// gain stage anywhere (and this silicon's PDM-to-PCM block has no
/// amplify stage either), so its -26 dBFS-sensitivity stream arrives
/// ~30x smaller than the levels the meter constants and the parrot
/// playback were calibrated against. Chosen from first-hardware
/// readings: normal-volume ambient sound measured dev ~400-800 raw,
/// so x8 maps it to ~3200-6400 - the mid-to-high bar zone the meter
/// constants define for normal speech, leaving headroom above for
/// genuinely loud input. Raise/lower by 1 if speech undershoots/pegs
/// the meter after the DC removal below.
const PDM_RX_GAIN_SHIFT: u32 = 3;

/// RX conditioning for [`run_session_pdm_mic`], run on each popped
/// chunk before the shared consumers see it: subtract the chunk's
/// mean (the T3902 carries a documented DC pedestal of up to ~3% of
/// full scale - measured ~620 on this hardware - and unlike the
/// codec boards there is no high-pass filter anywhere in the path;
/// unremoved, the meter reads the pedestal as a permanent ~14/255
/// and the gain below would multiply it x16), then apply the
/// saturating makeup gain. Pure sample math, backend-intrinsic, no
/// chip or board knowledge.
fn pdm_condition_rx(buf: &mut [u8]) {
    let mut sum: i64 = 0;
    let mut n: i64 = 0;
    for s in buf.chunks_exact(2) {
        sum += i16::from_le_bytes([s[0], s[1]]) as i64;
        n += 1;
    }
    if n == 0 {
        return;
    }
    let mean = (sum / n) as i32;
    for s in buf.chunks_exact_mut(2) {
        let v = i16::from_le_bytes([s[0], s[1]]) as i32;
        let amplified = ((v - mean) << PDM_RX_GAIN_SHIFT)
            .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        s.copy_from_slice(&amplified.to_le_bytes());
    }
}

/// Parrot playback makeup gain for the PDM-mic session, as a
/// left-shift (2 = x4 = +12 dB). Applied to the finished recording
/// just before the playback phase - NOT on the capture side, which
/// stays at [`PDM_RX_GAIN_SHIFT`] because that value is what the
/// level meter is calibrated against. The two jobs need different
/// numbers: hardware-measured conditioned speech averages ~2000
/// mean-abs, ~10 dB under the tone sweep's 0x1800 amplitude that
/// the user hears as normal volume, and speech's crest factor makes
/// it read quieter still. +12 dB closes that gap; loud peaks
/// saturate softly, which a diagnostic parrot can live with.
const PDM_PLAY_GAIN_SHIFT: u32 = 2;

/// Playback-side conditioning for [`run_session_pdm_mic`]'s parrot:
/// the saturating [`PDM_PLAY_GAIN_SHIFT`] gain. No DC handling - the
/// recording was already conditioned chunk-by-chunk on capture.
fn pdm_condition_play(buf: &mut [u8]) {
    for s in buf.chunks_exact_mut(2) {
        let v = i16::from_le_bytes([s[0], s[1]]) as i32;
        let amplified = (v << PDM_PLAY_GAIN_SHIFT)
            .clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        s.copy_from_slice(&amplified.to_le_bytes());
    }
}

/// The codec sessions' conditioning (both capture and playback
/// side): none. Their analog PGA and codec high-pass already deliver
/// calibrated, DC-free samples at verified loudness.
fn condition_passthrough(_buf: &mut [u8]) {}

/// Detect the RX stream's byte alignment from a chunk of audio, or
/// `None` if the chunk is too quiet to judge yet.
///
/// With the RX transfer armed before TX starts the clocks (the
/// ordering `run_session` needs - see its comments), the I2S RX
/// delivers its stream displaced by one spurious leading byte on
/// BOTH boards' silicon (hardware-verified on each, 2026-07-22;
/// register-level origin unresolved). Uncorrected, every sample
/// reads as `true_sample << 8` - quiet ambient "amplified" by
/// 48 dB. This detection is therefore a load-bearing part of the
/// capture path, not a quirk workaround.
///
/// It judges alignment by the quantity we actually care about: the
/// mean absolute sample value under each candidate alignment. The
/// wrong alignment splits every sample across two frames and reads
/// real audio as large noise-like values (mean-abs ~16k regardless
/// of content), while the correct one reads the true level -
/// typically a 10-100x separation. This replaced an earlier
/// sign-byte-parity heuristic that codec-startup ramps fooled into
/// confident wrong answers on both boards. Near-clipping audio and
/// pathological 256-quantized data blur the comparison; those
/// chunks return None and the caller keeps its current offset.
fn rx_align_offset(buf: &[u8]) -> Option<usize> {
    let mean_abs = |off: usize| -> u64 {
        let mut sum = 0u64;
        let mut cnt = 0u64;
        for pair in buf[off..].chunks_exact(2) {
            sum += (i16::from_le_bytes([pair[0], pair[1]]) as i64).unsigned_abs();
            cnt += 1;
        }
        if cnt == 0 { u64::MAX } else { sum / cnt }
    };
    let e0 = mean_abs(0);
    let e1 = mean_abs(1);
    // Decisive 4x margin or no verdict. Silence yields two tiny
    // near-equal energies and correctly stays undecided.
    if e0.saturating_mul(4) < e1 {
        Some(0)
    } else if e1.saturating_mul(4) < e0 {
        Some(1)
    } else {
        None
    }
}

/// Shared level-meter state for the capture and loopback loops: EMA
/// smoothing, rate-limited [`SystemEvent::MicLevel`] emission, and the
/// 1 Hz `mic:` diagnostic log line.
struct LevelMeter {
    smoothed: u32,
    last_sent: u8,
    sent_any: bool,
    last_emit: Instant,
    last_log: Instant,
}

impl LevelMeter {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            smoothed: 0,
            last_sent: 0,
            sent_any: false,
            last_emit: now,
            last_log: now,
        }
    }

    /// Fold one chunk's stats into the meter; emits `MicLevel` and the
    /// diagnostic line as their rate limits allow.
    fn feed(&mut self, stats: &MicStats) {
        // Exponential moving average (~4-chunk time constant) so the
        // meter glides instead of twitching per chunk.
        self.smoothed = (self.smoothed * 3 + stats.mean_abs) / 4;
        let gated = self.smoothed.saturating_sub(MIC_NOISE_FLOOR);
        let level = ((gated * 255) / MIC_FULL_SCALE).min(255) as u8;

        let now = Instant::now();
        // Diagnostic telemetry, behavior-free: abs drives the meter;
        // dev/mean/min/max characterise the raw signal (see
        // `MicStats`). Calibrate the floor / full-scale constants
        // against quiet-room, normal-speech and loud-close-speech
        // readings of `smoothed`.
        if now.duration_since(self.last_log) >= Duration::from_secs(1) {
            self.last_log = now;
            log::info!(
                "mic: abs={} dev={} mean={} min={} max={} smoothed={} level={}/255",
                stats.mean_abs, stats.dev, stats.mean,
                stats.min, stats.max, self.smoothed, level,
            );
        }
        if now.duration_since(self.last_emit) >= Duration::from_millis(MIC_EMIT_MS)
            && (!self.sent_any || level.abs_diff(self.last_sent) >= MIC_LEVEL_DELTA)
        {
            self.last_sent = level;
            self.sent_any = true;
            self.last_emit = now;
            // Non-blocking: if the main loop is mid-render and the
            // channel is full, drop this frame rather than block the
            // capture loop. A meter frame is disposable; blocking here
            // would back the mic up and flood the event loop.
            let _ = EVENTS.try_send(SystemEvent::MicLevel { level });
        }
    }
}

/// Read the mic over the I2S RX, emitting [`SystemEvent::MicLevel`] as
/// the input level moves, until an interrupting command arrives. `pop`
/// blocks until the DMA has data, so this self-paces. Returns the next
/// command to handle (e.g. a `PlayAlarm` that interrupted capture), or
/// `None` if capture was simply stopped.
///
/// `condition_rx` is the session kind's per-chunk sample conditioning
/// ([`condition_passthrough`] for codec capture, [`pdm_condition_rx`] for
/// the PDM mic), applied to the aligned chunk before the meter reads
/// it. The alignment detection runs on the raw chunk, the one-shot
/// first-frames dump stays raw - both are diagnostics of the wire.
async fn capture_until_interrupt<B>(
    rx: &mut esp_hal::i2s::master::asynch::I2sReadDmaTransferAsync<'_, B>,
    buf: &mut [u8],
    condition_rx: fn(&mut [u8]),
) -> Option<AudioCommand> {
    // Drain the MCLK-startup window first so the initial level reading
    // is real audio, not codec garbage. Raced against the command
    // queue: if the codecs ever fail to stream (no RX data, `pop`
    // never resolves), an unguarded pop would wedge the audio task
    // here forever and take the alarm path down with it.
    match select(rx.pop(&mut *buf), AUDIO_COMMAND.receive()).await {
        // Drained - or a pop error, which the loop's error arm handles.
        Either::First(_) => {}
        Either::Second(cmd) => match cmd {
            AudioCommand::StopCapture => return None,
            cmd @ (AudioCommand::PlayAlarm
            | AudioCommand::PlayTones
            | AudioCommand::StartLoopback) => return Some(cmd),
            // Skipping the drain on a stray command costs at most one
            // garbage level reading; the EMA below swallows it.
            AudioCommand::StopAlarm
            | AudioCommand::StopTones
            | AudioCommand::StopLoopback
            | AudioCommand::StartCapture => {}
        },
    }

    let mut meter = LevelMeter::new();
    // One-shot raw dump of the session's first frames: distinguishes
    // real audio (small varied LE i16s at rest) from a floating /
    // clock-crosstalk data line (rail-to-rail patterns) or byte-slip
    // (audio recognizable but shifted) when a board's meter reads
    // garbage.
    let mut first_chunk = true;
    // Per-session stream alignment, detected from the first chunk
    // with enough signal (see `rx_align_offset`). Pops are always
    // word-multiples, so one detection holds for the whole session.
    let mut rx_off: Option<usize> = None;
    loop {
        match select(rx.pop(&mut *buf), AUDIO_COMMAND.receive()).await {
            // Pop drains the RX DMA every chunk (~64 ms); the EMA
            // smooths across chunks and UI emission is rate-limited
            // inside the meter.
            Either::First(Ok(n)) => {
                if first_chunk {
                    first_chunk = false;
                    log::info!("mic: first frames: {:02x?}", &buf[..16.min(n)]);
                }
                // Re-evaluated every chunk (not latched): a transient
                // misread on startup garbage then self-corrects on
                // the next real-audio chunk instead of pegging the
                // whole session. Ambiguous chunks return None and
                // keep the current offset.
                if let Some(off) = rx_align_offset(&buf[..n]) {
                    if rx_off != Some(off) {
                        log::info!("mic: rx alignment offset -> {}", off);
                        rx_off = Some(off);
                    }
                }
                let off = rx_off.unwrap_or(0);
                condition_rx(&mut buf[off..n]);
                meter.feed(&mic_stats(&buf[off..n]));
            }
            Either::First(Err(e)) => {
                // `Late` permanently wedges the circular transfer -
                // and a synchronously-erroring pop wins every select,
                // so staying in this loop would also starve the
                // command queue (observed on hw: "command queue full"
                // drops until reboot). Rebuild the session with fresh
                // rings, same recovery as the parrot.
                log::warn!("rx.pop err: {:?}, rebuilding session", e);
                return Some(AudioCommand::StartCapture);
            }
            Either::Second(cmd) => match cmd {
                AudioCommand::StopCapture => return None,
                // Not ours - the other modes aren't running while we
                // capture. Mode-specific stops keep the command
                // streams from cancelling each other (see play side).
                AudioCommand::StopAlarm
                | AudioCommand::StopTones
                | AudioCommand::StopLoopback => {}
                AudioCommand::StartCapture => {} // already capturing
                cmd @ (AudioCommand::PlayAlarm
                | AudioCommand::PlayTones
                | AudioCommand::StartLoopback) => return Some(cmd),
            },
        }
    }
}

/// Play the three-tone speaker sweep once, then return. Interruptible
/// like every session loop. Emits [`SystemEvent::TonesDone`] only when
/// the sweep completes naturally, so a cancelled sweep (sleep safety
/// net) can't trigger the mic-test view's restart-the-meter response.
async fn play_tones_until_done<B>(
    tx: &mut esp_hal::i2s::master::asynch::I2sWriteDmaTransferAsync<'_, B>,
    amp: &mut SpeakerAmp<'_>,
    phase: &mut u32,
) -> Option<AudioCommand> {
    // Step 0 is the lead-in silence (hz = 0); the amp comes up only
    // once it has drained, so neither the stale ring lap nor the DAC
    // ramp reach the speaker and tone 1 starts clean.
    let steps: [(u32, u64); 4] = [
        (0, TONE_LEAD_MS),
        (TEST_TONES_HZ[0], TEST_TONE_MS),
        (TEST_TONES_HZ[1], TEST_TONE_MS),
        (TEST_TONES_HZ[2], TEST_TONE_MS),
    ];
    // One-shot TX push error log; the 1 ms yield below keeps a
    // persistently-failing push from busy-spinning the wall-clock
    // loop without ever awaiting.
    let mut err_logged = false;
    for (i, &(tone_hz, step_ms)) in steps.iter().enumerate() {
        if i == 1 {
            amp.enable();
        }
        if tone_hz != 0 {
            log::info!("tones: {} Hz for {} ms", tone_hz, step_ms);
        }
        // Fresh phase per tone: each starts at a half-period boundary
        // instead of wherever the previous frequency left off.
        *phase = 0;
        let start = Instant::now();
        while start.elapsed().as_millis() < step_ms {
            let fill = tx.push_with(|buf| {
                let frames = buf.len() / 4;
                fill_tone(&mut buf[..frames * 4], phase, tone_hz != 0, tone_hz.max(1));
                frames * 4
            });
            match select(fill, AUDIO_COMMAND.receive()).await {
                Either::First(Ok(_)) => {}
                Either::First(Err(e)) => {
                    if !err_logged {
                        err_logged = true;
                        log::warn!("tones: tx push err: {:?}", e);
                    }
                    Timer::after(Duration::from_millis(1)).await;
                }
                Either::Second(cmd) => match cmd {
                    AudioCommand::StopTones => {
                        log::info!("tones: stopped mid-sweep");
                        amp.disable();
                        return None;
                    }
                    cmd @ (AudioCommand::PlayAlarm
                    | AudioCommand::StartCapture
                    | AudioCommand::StartLoopback) => {
                        log::info!("tones: interrupted by {:?}", cmd);
                        amp.disable();
                        return Some(cmd);
                    }
                    AudioCommand::PlayTones => {} // already playing
                    AudioCommand::StopAlarm
                    | AudioCommand::StopCapture
                    | AudioCommand::StopLoopback => {}
                },
            }
        }
    }
    amp.disable();
    log::info!("tones: sweep done");
    // Completed naturally: let the mic-test view restart its meter.
    let _ = EVENTS.try_send(SystemEvent::TonesDone);
    None
}

/// LOOP test: "parrot" record-then-playback. Records ~1.0 s of mic
/// audio (mono 8 kHz, [`PARROT_RECORD_BYTES`]) with the speaker
/// muted, replays it with the mic ignored, and repeats until an
/// interrupting command arrives. The strict phase separation means the live mic and the
/// speaker are never acoustically coupled, so unlike a live monitor it
/// cannot howl. That's not over-caution: with the mics millimetres
/// from the speaker and 30 dB of PGA, the raw acoustic loop sits far
/// above the feedback threshold at any audible volume - the board's
/// reference stack (Waveshare's esp-brookesia voice pipeline) draws
/// the same conclusion and only ever runs the mic through an AEC DSP,
/// never raw into the speaker.
async fn loopback_until_interrupt<BT, BR>(
    tx: &mut esp_hal::i2s::master::asynch::I2sWriteDmaTransferAsync<'_, BT>,
    rx: &mut esp_hal::i2s::master::asynch::I2sReadDmaTransferAsync<'_, BR>,
    amp: &mut SpeakerAmp<'_>,
    buf: &mut [u8],
    condition_rx: fn(&mut [u8]),
    condition_play: fn(&mut [u8]),
) -> Option<AudioCommand> {
    // The mono 8 kHz accumulation buffer the recording lands in (the
    // pop buffer comes from `run_session`).
    let mut rec = alloc::vec![0u8; PARROT_RECORD_BYTES];
    let mut meter = LevelMeter::new();
    // Per-session stream alignment, detected once there's enough
    // signal (see `rx_align_offset`). Without the correction a
    // displaced stream records as `sample << 8` - ambient noise
    // played back at +48 dB.
    let mut rx_off: Option<usize> = None;
    // FIRST action: drain what accumulated since `run_session`'s
    // early drain (codec-init garbage, mostly), so the session
    // starts from fresh audio and the ring stays far from capacity.
    match select(rx.pop(&mut *buf), AUDIO_COMMAND.receive()).await {
        Either::First(_) => {}
        Either::Second(cmd) => {
            if let Some(r) = loopback_command(cmd, amp) {
                return r;
            }
        }
    }
    // Flush the pre-session TX ring once, muted, so the first unmute
    // doesn't replay a stale lap. Later cycles are covered by their
    // own trailing silence.
    if let Some(r) = parrot_push_silence(tx, amp, TX_RING_BYTES).await {
        return r;
    }
    loop {
        // -- Record phase: speaker muted, pops accumulate into `rec`
        // until it's full. Popping continuously keeps the RX ring
        // drained, so the phase length has no DMA ceiling.
        amp.disable();
        // Discard everything captured so far: codec-startup garbage on
        // the first cycle, our own playback on later ones.
        match select(rx.pop(&mut *buf), AUDIO_COMMAND.receive()).await {
            Either::First(_) => {}
            Either::Second(cmd) => {
                if let Some(r) = loopback_command(cmd, amp) {
                    return r;
                }
            }
        }
        let rec_start = Instant::now();
        let mut w = 0;
        while w < rec.len() {
            match select(rx.pop(&mut *buf), AUDIO_COMMAND.receive()).await {
                Either::First(Ok(n)) => {
                    // Whole frames only; a straggling 1-3 bytes would
                    // slip the L/R alignment.
                    let n = n / 4 * 4;
                    // Re-evaluated every chunk, same reasoning as the
                    // capture loop: never latch a startup-garbage
                    // misread for the session.
                    if let Some(off) = rx_align_offset(&buf[..n]) {
                        if rx_off != Some(off) {
                            log::info!("mic: rx alignment offset -> {}", off);
                            rx_off = Some(off);
                        }
                    }
                    let off = rx_off.unwrap_or(0);
                    // Condition before both consumers: the meter and
                    // the recording (so the parrot replays at the
                    // conditioned level, not the raw one).
                    condition_rx(&mut buf[off..n]);
                    meter.feed(&mic_stats(&buf[off..n]));
                    // Decimate stereo 16 kHz -> mono 8 kHz by
                    // averaging the left (MIC1) samples of each frame
                    // pair. The average is a crude anti-alias filter;
                    // plain drop-every-2nd-frame folds 4-8 kHz speech
                    // content into the audible band, which reads as a
                    // deeper, garbled ("slowed down") voice.
                    for pair in buf[off..n].chunks_exact(8) {
                        if w + 2 > rec.len() {
                            break;
                        }
                        let a = i16::from_le_bytes([pair[0], pair[1]]) as i32;
                        let b = i16::from_le_bytes([pair[4], pair[5]]) as i32;
                        let s = ((a + b) / 2) as i16;
                        rec[w..w + 2].copy_from_slice(&s.to_le_bytes());
                        w += 2;
                    }
                }
                Either::First(Err(e)) => {
                    // `Late` (RX ring overran) permanently wedges a
                    // circular transfer - esp-hal has no reset API, so
                    // retrying can never recover. End the session and
                    // hand StartLoopback back to the dispatcher: the
                    // rebuilt transfers start with fresh rings.
                    log::warn!("loopback rx.pop err: {:?}, rebuilding session", e);
                    return Some(AudioCommand::StartLoopback);
                }
                Either::Second(cmd) => {
                    if let Some(r) = loopback_command(cmd, amp) {
                        return r;
                    }
                }
            }
        }

        // -- Playback phase: replay `rec`, expanding each mono 8 kHz
        // sample into two identical stereo 16 kHz frames. Pushed one
        // TX ring (~64 ms) at a time, with an RX discard pop between
        // chunks so the RX ring stays drained however long the
        // recording is. A trailing ring of silence stops the tail
        // from stutter-looping through the next record phase (and
        // pre-cleans the ring for the next unmute).
        condition_play(&mut rec[..w]);
        amp.enable();
        let rec_ms = rec_start.elapsed().as_millis();
        let play_start = Instant::now();
        let mut roff = 0;
        while roff < w {
            let push = tx.push_with(|out| {
                // 8 output bytes per stored mono sample (2 bytes).
                let out_max = out.len().min((w - roff) * 4).min(TX_RING_BYTES);
                let take = out_max / 8 * 8;
                for (k, o) in out[..take].chunks_exact_mut(8).enumerate() {
                    let i = roff + k * 2;
                    let (lo, hi) = (rec[i], rec[i + 1]);
                    o.copy_from_slice(&[lo, hi, lo, hi, lo, hi, lo, hi]);
                }
                take
            });
            // Push and RX-discard CONCURRENTLY. Both waits are
            // descriptor-paced (~64 ms each); running them serially
            // halved the playback data rate and let the underfed TX
            // ring stutter-replay - the "slowed down" playback
            // (measured: rec 1.0 s, play 1.8 s). An interrupted push
            // is safe to drop: its closure never ran, so no data or
            // `roff` progress is lost.
            match select3(push, rx.pop(&mut *buf), AUDIO_COMMAND.receive()).await {
                // 0-byte grant (ring space < one expanded sample):
                // yield briefly instead of spinning.
                Either3::First(Ok(0)) => {
                    Timer::after(Duration::from_millis(1)).await;
                }
                Either3::First(Ok(k)) => roff += k / 4,
                // TX error: abandon this playback, cycle onwards.
                Either3::First(Err(_)) => break,
                // RX discarded; keep the ring drained while playing.
                Either3::Second(Ok(_)) => {}
                Either3::Second(Err(e)) => {
                    // An instantly-failing pop would win every select
                    // and starve the push - same permanent `Late`
                    // wedge as the record phase, same recovery.
                    log::warn!("loopback rx.pop err: {:?}, rebuilding session", e);
                    amp.disable();
                    return Some(AudioCommand::StartLoopback);
                }
                Either3::Third(cmd) => {
                    if let Some(r) = loopback_command(cmd, amp) {
                        return r;
                    }
                }
            }
        }
        // Speed telemetry: record and playback should report the same
        // duration (~1000 ms each). Diverging numbers mean a real
        // resampling bug; matching numbers with an odd-sounding voice
        // point at reproduction quality instead.
        log::info!(
            "parrot: rec {} ms, play {} ms ({} B)",
            rec_ms,
            play_start.elapsed().as_millis(),
            w,
        );
        if let Some(r) = parrot_push_silence(tx, amp, TX_RING_BYTES).await {
            return r;
        }
    }
}

/// Queue `len` bytes of silence into the TX ring, racing the command
/// queue throughout. Returns `Some(session-result)` if a command
/// ended the session, `None` once the bytes are fully queued (a TX
/// error abandons the push but keeps the session alive).
#[allow(clippy::option_option)]
async fn parrot_push_silence<B>(
    tx: &mut esp_hal::i2s::master::asynch::I2sWriteDmaTransferAsync<'_, B>,
    amp: &mut SpeakerAmp<'_>,
    len: usize,
) -> Option<Option<AudioCommand>> {
    let mut off = 0;
    while off < len {
        let push = tx.push_with(|out| {
            let take = out.len().min(len - off) / 4 * 4;
            out[..take].fill(0);
            take
        });
        match select(push, AUDIO_COMMAND.receive()).await {
            // 0-byte grant (ring space < one frame): yield briefly
            // instead of spinning on push_with.
            Either::First(Ok(0)) => Timer::after(Duration::from_millis(1)).await,
            Either::First(Ok(k)) => off += k,
            Either::First(Err(_)) => return None,
            Either::Second(cmd) => {
                if let Some(r) = loopback_command(cmd, amp) {
                    return Some(r);
                }
            }
        }
    }
    None
}

/// Shared command handling for the parrot loop's select points.
/// `None` = command ignored, keep looping. `Some(r)` = end the session
/// and give `r` back to the dispatcher (`Some(Some(cmd))` hands the
/// command off, `Some(None)` is a plain stop).
#[allow(clippy::option_option)]
fn loopback_command(
    cmd: AudioCommand,
    amp: &mut SpeakerAmp<'_>,
) -> Option<Option<AudioCommand>> {
    match cmd {
        AudioCommand::StopLoopback => {
            amp.disable();
            Some(None)
        }
        cmd @ (AudioCommand::PlayAlarm
        | AudioCommand::PlayTones
        | AudioCommand::StartCapture) => {
            amp.disable();
            Some(Some(cmd))
        }
        AudioCommand::StartLoopback => None, // already looping
        AudioCommand::StopAlarm
        | AudioCommand::StopCapture
        | AudioCommand::StopTones => None,
    }
}

/// Overwrite the stale TX ring with silence. A fresh transfer starts
/// streaming whatever the ring last held (e.g. the final 64 ms of
/// the previous session's tone), and in sessions that never push TX
/// the circular DMA loops it forever. Plain awaits - this completes
/// within ~2 ring laps and commands can wait that long.
async fn flush_tx_ring<B>(
    tx: &mut esp_hal::i2s::master::asynch::I2sWriteDmaTransferAsync<'_, B>,
) {
    let mut off = 0;
    while off < TX_RING_BYTES {
        match tx
            .push_with(|out| {
                let take = out.len().min(TX_RING_BYTES - off) / 4 * 4;
                out[..take].fill(0);
                take
            })
            .await
        {
            Ok(0) => Timer::after(Duration::from_millis(1)).await,
            Ok(k) => off += k,
            Err(e) => {
                log::warn!("Audio: ring flush err: {:?}", e);
                break;
            }
        }
    }
}

/// Which side of the audio stack a session should drive. Both sides of
/// the I2S come up either way (they share clocks), but the inner loop
/// differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    /// Playback: enable the speaker amp, stream the beep tone until
    /// `StopAlarm` (or an interrupting command) arrives.
    Play,
    /// Capture: keep the amp muted, drain the mic stream and emit
    /// `SystemEvent::MicLevel` until `StopCapture` arrives.
    Capture,
    /// Speaker test: play the three-tone sweep once, emit
    /// `SystemEvent::TonesDone`, and end the session.
    Tones,
    /// LOOP test: record-then-playback "parrot" cycles (mic and
    /// speaker never live simultaneously - see
    /// `loopback_until_interrupt` on why a live monitor howls here),
    /// level meter included, until `StopLoopback` arrives.
    Loopback,
}

/// Run **one** audio session: build the I2S + DMA stack from the
/// supplied (reborrowed) peripheral handles, re-init the codecs, run
/// the play/capture loop, and tear everything down cleanly when the
/// session ends.
///
/// **Why this shape:** esp-hal's circular DMA has no public reset API.
/// Once the descriptor ring wraps with no consumer draining it (which
/// happens any time the audio task pauses for more than ~half a
/// second), the transfer enters a permanently-stuck `Late` state. The
/// only recovery is to drop the whole transfer and rebuild fresh. So
/// instead of a "build-once-forever" task, every session-starting
/// command (`PlayAlarm`, `StartCapture`, `PlayTones`, `StartLoopback`)
/// invokes this function with `Peri::reborrow()`'d
/// peripheral tokens; the transfer lives for exactly one session and
/// the reborrows release on session end, leaving the caller's tokens
/// ready for the next session.
///
/// The bin's `audio_task` owns the underlying `Peri<'static, _>` tokens,
/// the speaker amp, and the tone-phase counter across sessions; this
/// function borrows them per call. `tune_i2s` is the bin's seam for
/// chip-specific I2S register fixups (see its call site below).
///
/// Returns the command the dispatcher should handle next, if the inner
/// loop consumed one (e.g. a `PlayAlarm` that interrupted capture).
/// `None` means the session ended cleanly via its mode's Stop and the
/// dispatcher should park on the next `AUDIO_COMMAND.receive()`.
///
/// The ALDO1 analog rail both codecs need is already enabled at boot
/// (the FT3168 touch controller shares it - see `Pmu::set_audio_rail`),
/// so no rail toggle happens here.
#[allow(clippy::too_many_arguments)]
pub async fn run_session<'d>(
    mode: SessionMode,
    i2c_bus: &'static SharedI2c,
    i2s: impl esp_hal::i2s::master::Instance + 'd,
    dma_ch: impl DmaChannelFor<AnyI2s<'d>>,
    mclk: impl PeripheralOutput<'d>,
    bclk: impl PeripheralOutput<'d>,
    ws: impl PeripheralOutput<'d>,
    dout: impl PeripheralOutput<'d>,
    din: impl PeripheralInput<'d>,
    amp: &mut SpeakerAmp<'_>,
    tx_buf: &'d mut [u8],
    tx_desc: &'d mut [DmaDescriptor],
    rx_buf: &'d mut [u8],
    rx_desc: &'d mut [DmaDescriptor],
    phase: &mut u32,
    tune_i2s: fn(),
) -> Option<AudioCommand> {
    log::info!("Audio: session {:?}", mode);
    // Full-duplex bring-up - both sides share one I2S0 + clocks.
    let (i2s_tx, i2s_rx) = audio_hal::build_i2s(
        i2s, dma_ch, mclk, bclk, ws, dout, din, tx_desc, rx_desc,
    );
    // Chip-specific I2S register fixups the HAL doesn't expose,
    // supplied by the bin (a no-op where the silicon needs none).
    // Runs after the HAL has configured the peripheral - so it can
    // override HAL-written fields - and before the transfers start
    // the clocks. Re-invoked every session because the HAL rewrites
    // its configuration each time.
    tune_i2s();
    // Start RX FIRST, while the clocks are still dead: TX drives
    // BCLK/WS, so an armed RX captures from the first frame edge once
    // TX starts, making the stream's head alignment DETERMINISTIC.
    // (Starting TX first left the alignment a per-session lottery on
    // shared-clock duplex silicon.) The determinism comes at a known
    // cost: one spurious leading byte in the RX stream on both
    // boards' silicon, which `rx_align_offset` detects and the
    // consumers compensate for. Do not delay RX past codec init
    // either - tried 2026-07-22, wedges every session's first pop
    // with `Late`; the RX DMA must be running before the clocks are.
    let mut rx = match i2s_rx.read_dma_circular_async(rx_buf) {
        Ok(r) => r,
        Err(e) => {
            log::error!("Audio: read_dma_circular_async failed: {:?}", e);
            return None;
        }
    };
    // Starting TX begins MCLK/BCLK/WS output; TX streams whatever's
    // in the ring until the silence flush below overwrites it.
    let mut tx = match i2s_tx.write_dma_circular_async(tx_buf) {
        Ok(t) => t,
        Err(e) => {
            log::error!("Audio: write_dma_circular_async failed: {:?}", e);
            return None;
        }
    };
    // The session's pop buffer, allocated up here so the RX ring can
    // be drained IMMEDIATELY - before the flush and codec init below,
    // which together take ~250 ms (more under a render-saturated
    // executor). Waiting until the mode loops' own drains to make the
    // first pop let the ring approach its ~512 ms capacity and birth
    // a permanently-wedged `Late` (observed on hw: capture session
    // dead, command queue starved). This drain resets the accumulation
    // clock to ~zero; the later phases then fit the budget easily.
    let mut buf = alloc::vec![0u8; CAPTURE_CHUNK_BYTES];
    if let Err(e) = rx.pop(&mut buf).await {
        // A pop error this early means the transfer was born broken;
        // rebuilding is the only recovery (see the mode loops).
        log::warn!("Audio: early drain err: {:?}, aborting session", e);
        return None;
    }

    // Overwrite the stale TX ring with silence before anything else
    // (see `flush_tx_ring`) - here the endless replay is inaudible
    // with the amp off, but loud enough on the DAC output to couple
    // into the ES7210 and peg the level meter in a quiet room.
    flush_tx_ring(&mut tx).await;
    Timer::after(Duration::from_millis(10)).await; // MCLK settle

    // Re-init the codecs on EVERY session. The previous session's
    // transfer drop stopped MCLK flowing to ES7210 / ES8311; on
    // empirical evidence the codecs don't auto-resume streaming when
    // MCLK comes back - the ADC silently produces no data, the DMA
    // never advances, and a capture session's first `pop` never
    // resolves (capture_until_interrupt races its pops against the
    // command queue, so even then the task stays responsive). Running
    // the I2C init sequence here (~30 ms total) re-asserts the codec
    // enable bits and makes streaming start again. The init is safe
    // to repeat - it's a defined reset+configure sequence per the
    // codec drivers.
    {
        let mut i2c = i2c_bus.lock().await;
        let es8311 = Es8311::new();
        match es8311.init(&mut *i2c) {
            Ok(()) => {
                // Reset hold, per the reference driver. Skipping it
                // left the chip half-reset on a coin-flip of session
                // re-inits: silent or warbling DAC, hot ADC path.
                Timer::after(Duration::from_millis(20)).await;
                match es8311.init_after_reset(&mut *i2c) {
                    Ok(()) => log::info!("Audio: ES8311 ready"),
                    Err(_) => log::error!("Audio: ES8311 config failed"),
                }
            }
            Err(_) => log::error!(
                "Audio: ES8311 not found at 0x{:02X}",
                drivers::es8311::ADDR,
            ),
        }
        let _ = es8311.set_volume(&mut *i2c, 0xAF);

        let es7210 = Es7210::new();
        match es7210.init(&mut *i2c) {
            Ok(()) => {
                Timer::after(Duration::from_millis(10)).await;
                let _ = es7210.init_after_delay(&mut *i2c);
                Timer::after(Duration::from_millis(10)).await;
                match es7210.finalize(&mut *i2c) {
                    Ok(()) => {
                        // Override the driver's 30 dB default - it
                        // saturates the ADC on ambient.
                        let _ = es7210.set_gain(&mut *i2c, MIC_GAIN);
                        log::info!("Audio: ES7210 ready (gain override applied)");
                    }
                    Err(_) => log::error!("Audio: ES7210 finalize failed"),
                }
            }
            Err(_) => log::error!(
                "Audio: ES7210 not found at 0x{:02X}",
                drivers::es7210::ADDR,
            ),
        }
    }

    let result = match mode {
        SessionMode::Play => {
            amp.enable();
            play_until_interrupt(&mut tx, amp, phase).await
        }
        SessionMode::Tones => {
            // The sweep enables the amp itself, after its lead-in
            // silence has flushed the stale ring and DAC ramp.
            amp.disable();
            play_tones_until_done(&mut tx, amp, phase).await
        }
        SessionMode::Capture => {
            // Defensive: amp stays muted during capture (no feedback).
            amp.disable();
            capture_until_interrupt(&mut rx, &mut buf, condition_passthrough).await
        }
        SessionMode::Loopback => {
            // The loop unmutes itself after the startup drain, so the
            // codec init burst never reaches the speaker.
            amp.disable();
            loopback_until_interrupt(
                &mut tx,
                &mut rx,
                amp,
                &mut buf,
                condition_passthrough,
                condition_passthrough,
            )
            .await
        }
    };

    // tx and rx drop here. That cascades through the type stack: each
    // transfer's drop releases its I2sTx / I2sRx; those release the
    // owning I2s; that releases all the reborrowed Peri tokens (i2s,
    // dma_ch, mclk, bclk, ws, dout, din) and the buffer/descriptor
    // borrows. The caller's `Peri<'static, _>` tokens then become
    // available for the next session.
    result
}

/// Run **one** playback-only audio session - the TX half of
/// [`run_session`] for boards whose speaker path is a clock-in-only
/// Class-D amp (MAX98357A): no codec to re-init (so no I2C), no
/// MCLK, no capture side. Same session-scoped transfer lifecycle and
/// for the same reason (circular DMA has no reset API - see
/// `run_session`'s design comment); same TX ring flush; the mode
/// loops themselves are the shared ones.
///
/// Only `Play` and `Tones` are playable here. The capture modes
/// (`Capture`, `Loopback`) need a capture backend this session kind
/// doesn't build (a board whose mic is a PDM device routes them to
/// [`run_session_pdm_mic`] instead); a dispatcher shouldn't send
/// them here, and defensively they end the session immediately.
pub async fn run_session_tx<'d>(
    mode: SessionMode,
    i2s: impl esp_hal::i2s::master::Instance + 'd,
    dma_ch: impl DmaChannelFor<AnyI2s<'d>>,
    bclk: impl PeripheralOutput<'d>,
    ws: impl PeripheralOutput<'d>,
    dout: impl PeripheralOutput<'d>,
    amp: &mut SpeakerAmp<'_>,
    tx_buf: &'d mut [u8],
    tx_desc: &'d mut [DmaDescriptor],
    phase: &mut u32,
    tune_i2s: fn(),
) -> Option<AudioCommand> {
    log::info!("Audio: TX session {:?}", mode);
    let i2s_tx = audio_hal::build_i2s_tx(i2s, dma_ch, bclk, ws, dout, tx_desc);
    // Chip-specific I2S register fixups, same seam as run_session.
    tune_i2s();
    // Starting TX begins BCLK/WS output; TX streams whatever's in
    // the ring until the silence flush below overwrites it.
    let mut tx = match i2s_tx.write_dma_circular_async(tx_buf) {
        Ok(t) => t,
        Err(e) => {
            log::error!("Audio: write_dma_circular_async failed: {:?}", e);
            return None;
        }
    };
    // Stale-ring flush, as in run_session. The amp is still in
    // standby while this runs (BCLK only just started; the replayed
    // ring is silence-overwritten within ~2 laps).
    flush_tx_ring(&mut tx).await;

    match mode {
        SessionMode::Play => {
            amp.enable();
            play_until_interrupt(&mut tx, amp, phase).await
        }
        SessionMode::Tones => {
            // The sweep enables the amp itself, after its lead-in.
            amp.disable();
            play_tones_until_done(&mut tx, amp, phase).await
        }
        SessionMode::Capture | SessionMode::Loopback => {
            log::warn!("Audio: {:?} not supported on a TX-only backend", mode);
            None
        }
    }
    // tx drops here, stopping the DMA and BCLK/WS; the amp then
    // enters its own standby. The reborrowed Peri tokens release for
    // the next session, as in run_session.
}

/// Run **one** audio session on a board whose capture side is a **PDM
/// microphone** - the third session kind, next to [`run_session`]
/// (codec duplex) and [`run_session_tx`] (playback only). Same
/// session-scoped transfer lifecycle, for the same circular-DMA
/// reasons.
///
/// Differences from [`run_session`]:
/// - No codec exists on either side, so there is no I2C re-init and
///   no MCLK; the amp is a clock-in-only Class-D and the mic is a raw
///   PDM device that wakes when its clock starts and auto-sleeps when
///   the session's transfer drop stops it.
/// - The RX unit is brought up in the HAL's standard mode and then
///   flipped into hardware PDM-to-PCM mode by the bin's `tune_i2s`
///   hook (the HAL has no PDM API; the registers are chip-specific).
///   The hook runs after the HAL configuration and before the
///   transfers start, and must commit its writes to the RX clock
///   domain itself (the HAL only does that during configuration).
/// - TX and RX have independent clocks here, so the RX stream's head
///   alignment doesn't depend on TX start order the way the
///   shared-clock duplex silicon's does. RX is still armed first so
///   no early DMA window is missed; `rx_align_offset` stays in the
///   loops as a self-correcting guard either way.
///
/// With both PDM line slots enabled by the tune hook, a single mic's
/// data appears in both 16-bit slots of each frame (the mic holds its
/// data line through the full clock period - datasheet-documented
/// single-mic behavior), so the wire format matches the codec boards'
/// stereo capture and every mode loop is shared unchanged.
///
/// All four modes run here. Dispatchers that want the cheaper
/// TX-only path for pure playback (no RX DMA, mic clock never starts)
/// can route `Play`/`Tones` to [`run_session_tx`] instead and send
/// only the capture modes' commands here.
#[allow(clippy::too_many_arguments)]
pub async fn run_session_pdm_mic<'d>(
    mode: SessionMode,
    i2s: impl esp_hal::i2s::master::Instance + 'd,
    dma_ch: impl DmaChannelFor<AnyI2s<'d>>,
    bclk: impl PeripheralOutput<'d>,
    ws: impl PeripheralOutput<'d>,
    dout: impl PeripheralOutput<'d>,
    pdm_clk: impl PeripheralOutput<'d>,
    pdm_din: impl PeripheralInput<'d>,
    amp: &mut SpeakerAmp<'_>,
    tx_buf: &'d mut [u8],
    tx_desc: &'d mut [DmaDescriptor],
    rx_buf: &'d mut [u8],
    rx_desc: &'d mut [DmaDescriptor],
    phase: &mut u32,
    tune_i2s: fn(),
) -> Option<AudioCommand> {
    log::info!("Audio: PDM-mic session {:?}", mode);
    let (i2s_tx, i2s_rx) = audio_hal::build_i2s_tx_pdm_rx(
        i2s, dma_ch, bclk, ws, dout, pdm_clk, pdm_din, tx_desc, rx_desc,
    );
    // The bin's hook switches the RX unit into PDM-to-PCM mode (and
    // is a required step here, not an optional fixup - the HAL left
    // the unit in standard TDM mode).
    tune_i2s();
    // Arm RX before TX, as in run_session. Starting RX also starts
    // the mic's clock; the mic's wake-up transient (~20 ms for the
    // T3902) lands in the early drain below.
    let mut rx = match i2s_rx.read_dma_circular_async(rx_buf) {
        Ok(r) => r,
        Err(e) => {
            log::error!("Audio: read_dma_circular_async failed: {:?}", e);
            return None;
        }
    };
    let mut tx = match i2s_tx.write_dma_circular_async(tx_buf) {
        Ok(t) => t,
        Err(e) => {
            log::error!("Audio: write_dma_circular_async failed: {:?}", e);
            return None;
        }
    };
    // Session pop buffer + immediate RX drain, same rationale as
    // run_session (an undrained ring wraps into a permanent `Late`).
    // There is no codec-init window here, but the TX ring flush below
    // still takes its time and the mode loops expect a fresh ring.
    let mut buf = alloc::vec![0u8; CAPTURE_CHUNK_BYTES];
    if let Err(e) = rx.pop(&mut buf).await {
        log::warn!("Audio: early drain err: {:?}, aborting session", e);
        return None;
    }
    // Stale-ring flush, as in the other session kinds.
    flush_tx_ring(&mut tx).await;
    // Mic settle window. After its clock starts the mic's output is
    // a large decaying ramp (DC-pedestal charge + decimator startup),
    // hardware-observed still climbing through raw 0x1d0c ~150 ms in
    // - past every drain above. The moving ramp defeats the per-chunk
    // mean subtraction, so undrained it reads as a phantom mid-bar
    // level blip at session open (and the first parrot cycle records
    // its tail as a click). Wait it out, then discard; the ~250 ms
    // accumulation sits well under the RX ring's ~512 ms capacity,
    // and the codec session kind spends a comparable span on codec
    // init at the same point.
    Timer::after(Duration::from_millis(250)).await;
    if let Err(e) = rx.pop(&mut buf).await {
        log::warn!("Audio: settle drain err: {:?}, aborting session", e);
        return None;
    }

    match mode {
        SessionMode::Play => {
            amp.enable();
            play_until_interrupt(&mut tx, amp, phase).await
        }
        SessionMode::Tones => {
            // The sweep enables the amp itself, after its lead-in.
            amp.disable();
            play_tones_until_done(&mut tx, amp, phase).await
        }
        SessionMode::Capture => {
            // Amp muted; the TX ring holds silence for the duration.
            amp.disable();
            capture_until_interrupt(&mut rx, &mut buf, pdm_condition_rx).await
        }
        SessionMode::Loopback => {
            amp.disable();
            loopback_until_interrupt(
                &mut tx,
                &mut rx,
                amp,
                &mut buf,
                pdm_condition_rx,
                pdm_condition_play,
            )
            .await
        }
    }
    // tx and rx drop here, stopping both DMA transfers and both
    // units' clocks: the amp enters standby and the mic enters its
    // sleep mode. The reborrowed Peri tokens release for the next
    // session, as in run_session.
}
