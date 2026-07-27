//! HAL glue for the audio I2S interface. The SoC is the I2S master;
//! pins are the bins' business and arrive as parameters.
//!
//! Two build paths, matching the two speaker architectures we carry:
//! [`build_i2s`] brings up full duplex (TX DAC + RX ADC codec pair
//! sharing one I2S unit and its clocks, MCLK required) and
//! [`build_i2s_tx`] brings up playback only (a clock-in-only Class-D
//! amp like the MAX98357A - no codec, no MCLK, no capture path).
//!
//! ## Sample format
//!
//! 16 kHz, 16-bit stereo, standard I2S (Philips/TDM).
//! DMA buffer layout: interleaved [left_ch, right_ch, ...] as i16 samples.

use esp_hal::{
    Async,
    dma::{DmaChannelFor, DmaDescriptor},
    gpio::{Output, interconnect::{PeripheralInput, PeripheralOutput}},
    i2s::master::{I2s, I2sRx, I2sTx, Config, DataFormat, Channels},
    time::Rate,
};

use esp_hal::i2s::AnyI2s;

pub const SAMPLE_RATE_HZ: u32 = 16_000;

/// Speaker amplifier control. Wraps the amp-enable GPIO on boards
/// that have one (e.g. NS4150B PA_CTRL, active HIGH). Boards whose
/// amp has no host-driven enable line construct with [`Self::fixed`]
/// and both calls are no-ops - the MAX98357A mutes itself (auto
/// standby, outputs high-impedance) whenever BCLK stops toggling,
/// which happens naturally when the session's transfer drops.
pub struct SpeakerAmp<'d> {
    ctrl: Option<Output<'d>>,
}

impl<'d> SpeakerAmp<'d> {
    pub fn new(pin: impl Into<Output<'d>>) -> Self {
        Self { ctrl: Some(pin.into()) }
    }

    /// An amp with no enable line - it manages its own standby off
    /// the I2S clocks.
    pub fn fixed() -> Self {
        Self { ctrl: None }
    }

    pub fn enable(&mut self) {
        if let Some(ctrl) = self.ctrl.as_mut() {
            ctrl.set_high();
        }
    }

    #[allow(dead_code)]
    pub fn disable(&mut self) {
        if let Some(ctrl) = self.ctrl.as_mut() {
            ctrl.set_low();
        }
    }
}

/// Build the I2S TX (playback) and RX (capture) channels.
///
/// Returns `(I2sTx, I2sRx)` both in async mode, ready for DMA transfers.
///
/// Descriptors are taken at lifetime `'d` rather than `'static` so the
/// caller can reborrow session-scoped slices into the I2S (and have the
/// reborrows release on session end - see `run_session`). The original
/// backing storage is still typically static (allocated by
/// `dma_circular_buffers!`), but is reborrowed at each call site so the
/// transfer's lifetime is bounded by the session, not by the static.
pub fn build_i2s<'d>(
    i2s:          impl esp_hal::i2s::master::Instance + 'd,
    dma_channel:  impl DmaChannelFor<AnyI2s<'d>>,
    mclk:         impl PeripheralOutput<'d>,
    bclk:         impl PeripheralOutput<'d>,
    ws:           impl PeripheralOutput<'d>,
    dout:         impl PeripheralOutput<'d>,
    din:          impl PeripheralInput<'d>,
    tx_desc:      &'d mut [DmaDescriptor],
    rx_desc:      &'d mut [DmaDescriptor],
) -> (I2sTx<'d, Async>, I2sRx<'d, Async>) {
    // SAFETY: esp-hal's `.build()` requires `&'static mut [DmaDescriptor]`
    // because the DMA hardware reads descriptors as raw pointers and the
    // chain has no statically-tracked stop. In our session-scoped use,
    // both the returned `I2sTx<'d>` / `I2sRx<'d>` and the descriptors are
    // bounded by `'d` (the caller's session scope) - `run_session` drops
    // the transfers BEFORE returning, which stops the DMA and releases
    // the descriptor borrow. The descriptor storage itself is `'static`
    // (allocated by `dma_circular_buffers!` in the bin); the `'d` borrow
    // is just a Rust-side reborrow of that static storage. Extending the
    // lifetime to `'static` here is sound because the underlying memory
    // really does live `'static`; the borrow checker just can't see it
    // through esp-hal's API.
    let tx_desc_static: &'static mut [DmaDescriptor] =
        unsafe { core::mem::transmute(tx_desc) };
    let rx_desc_static: &'static mut [DmaDescriptor] =
        unsafe { core::mem::transmute(rx_desc) };

    let i2s = I2s::new(
        i2s,
        dma_channel,
        Config::new_tdm_philips()
            .with_sample_rate(Rate::from_hz(SAMPLE_RATE_HZ))
            .with_data_format(DataFormat::Data16Channel16)
            .with_channels(Channels::STEREO),
    )
    .unwrap()
    .with_mclk(mclk)
    .into_async();

    let tx = i2s.i2s_tx
        .with_bclk(bclk)
        .with_ws(ws)
        .with_dout(dout)
        .build(tx_desc_static);

    // RX shares BCLK and WS driven by TX above; only DIN is a new pin.
    let rx = i2s.i2s_rx
        .with_din(din)
        .build(rx_desc_static);

    (tx, rx)
}

/// Build a playback-only I2S TX channel - for boards whose speaker
/// path is a clock-in-only Class-D amp (MAX98357A): no codec to
/// clock, so no MCLK output, and no capture side. Same wire format
/// as [`build_i2s`]; the amp auto-detects the clocking scheme from
/// BCLK/LRCLK alone (datasheet "MCLK Elimination").
///
/// Descriptor lifetime story is identical to [`build_i2s`] - see the
/// SAFETY comment there.
pub fn build_i2s_tx<'d>(
    i2s:          impl esp_hal::i2s::master::Instance + 'd,
    dma_channel:  impl DmaChannelFor<AnyI2s<'d>>,
    bclk:         impl PeripheralOutput<'d>,
    ws:           impl PeripheralOutput<'d>,
    dout:         impl PeripheralOutput<'d>,
    tx_desc:      &'d mut [DmaDescriptor],
) -> I2sTx<'d, Async> {
    // SAFETY: see build_i2s - the backing storage really is 'static,
    // the transfer (and this borrow) is dropped at session end.
    let tx_desc_static: &'static mut [DmaDescriptor] =
        unsafe { core::mem::transmute(tx_desc) };

    let i2s = I2s::new(
        i2s,
        dma_channel,
        Config::new_tdm_philips()
            .with_sample_rate(Rate::from_hz(SAMPLE_RATE_HZ))
            .with_data_format(DataFormat::Data16Channel16)
            .with_channels(Channels::STEREO),
    )
    .unwrap()
    .into_async();

    i2s.i2s_tx
        .with_bclk(bclk)
        .with_ws(ws)
        .with_dout(dout)
        .build(tx_desc_static)
}

/// Build playback TX plus a capture RX destined for a **PDM
/// microphone** - for boards whose speaker is a clock-in-only Class-D
/// amp (TX side identical to [`build_i2s_tx`]) and whose mic is a raw
/// PDM device on its own two pins rather than an I2S-slave codec.
///
/// The TX and RX units of one I2S peripheral are independent (own
/// clock dividers, own pins), so the speaker path here is exactly the
/// [`build_i2s_tx`] one. The RX unit routes only two signals: its WS
/// output - which *is* the PDM microphone clock once the unit is in
/// PDM mode - and DIN for the mic's data line. No BCLK is routed and
/// no MCLK exists.
///
/// The HAL has no PDM API (it configures the RX unit for standard
/// TDM), so the caller MUST flip the RX unit into PDM-to-PCM mode
/// via its chip-specific tune hook after this returns and before the
/// transfers start - see `run_session_pdm_mic`. Once tuned, the
/// hardware PDM-to-PCM converter delivers interleaved 16-bit PCM at
/// [`SAMPLE_RATE_HZ`]; with both line slots enabled the wire format
/// matches [`build_i2s`]'s stereo layout, so the session mode loops
/// are shared unchanged.
///
/// Descriptor lifetime story is identical to [`build_i2s`] - see the
/// SAFETY comment there.
#[allow(clippy::too_many_arguments)]
pub fn build_i2s_tx_pdm_rx<'d>(
    i2s:          impl esp_hal::i2s::master::Instance + 'd,
    dma_channel:  impl DmaChannelFor<AnyI2s<'d>>,
    bclk:         impl PeripheralOutput<'d>,
    ws:           impl PeripheralOutput<'d>,
    dout:         impl PeripheralOutput<'d>,
    pdm_clk:      impl PeripheralOutput<'d>,
    pdm_din:      impl PeripheralInput<'d>,
    tx_desc:      &'d mut [DmaDescriptor],
    rx_desc:      &'d mut [DmaDescriptor],
) -> (I2sTx<'d, Async>, I2sRx<'d, Async>) {
    // SAFETY: see build_i2s - the backing storage really is 'static,
    // the transfers (and these borrows) are dropped at session end.
    let tx_desc_static: &'static mut [DmaDescriptor] =
        unsafe { core::mem::transmute(tx_desc) };
    let rx_desc_static: &'static mut [DmaDescriptor] =
        unsafe { core::mem::transmute(rx_desc) };

    let i2s = I2s::new(
        i2s,
        dma_channel,
        Config::new_tdm_philips()
            .with_sample_rate(Rate::from_hz(SAMPLE_RATE_HZ))
            .with_data_format(DataFormat::Data16Channel16)
            .with_channels(Channels::STEREO),
    )
    .unwrap()
    .into_async();

    let tx = i2s.i2s_tx
        .with_bclk(bclk)
        .with_ws(ws)
        .with_dout(dout)
        .build(tx_desc_static);

    // RX unit: WS out doubles as the PDM clock (once the tune hook
    // has switched the unit into PDM mode), DIN carries the mic data.
    let rx = i2s.i2s_rx
        .with_ws(pdm_clk)
        .with_din(pdm_din)
        .build(rx_desc_static);

    (tx, rx)
}
