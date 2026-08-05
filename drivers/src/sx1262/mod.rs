//! Semtech SX1261/2 sub-GHz LoRa/(G)FSK transceiver driver.
//!
//! Datasheet-complete command layer over the chip's SPI opcode
//! interface: operating modes, DIO/IRQ routing, TCXO control, LoRa
//! and GFSK modulation/packet configuration, CAD, buffer access,
//! status/statistics, and the four known-limitation workarounds
//! (data sheet rev 2.2, chapter 15). Policy - init sequences, IRQ
//! handling, band plans - belongs to the caller; this layer only
//! speaks chip verbs, mirroring the crate's other drivers.
//!
//! Transport-agnostic: every method takes an `embedded-hal 1.x`
//! [`SpiDevice`] (chip select owned by the SPI stack, so the radio
//! can share a bus behind per-device CS) plus the BUSY [`InputPin`].
//! The driver polls BUSY low before every transaction, as required
//! by section 8.3.1 - commands are rejected while the state machine
//! is working, and in Sleep mode BUSY is held high until a falling
//! NSS edge wakes the chip (see [`Sx1262::wake`]).
//!
//! Blocking by design: the longest legitimate BUSY windows are the
//! 3.5 ms cold-start wake / full calibration (Table 8-2, 13-17),
//! plus the caller-chosen TCXO startup delay. The spin budget is
//! sized far above that; a `BusyTimeout` therefore means a wiring /
//! power / reset-state problem, not a slow chip.

use embedded_hal::digital::InputPin;
use embedded_hal::spi::{Operation, SpiDevice};

pub mod regs;
use regs::{opcode, reg};

/// BUSY poll budget. Each iteration is one pin read (tens of ns at
/// watch-class clock rates), so the budget comfortably covers the
/// several-ms worst cases documented in the module docs.
const BUSY_SPIN_BUDGET: u32 = 2_000_000;

/// RX/TX timeout special values (Tables 13-7 / 13-9).
pub const TIMEOUT_SINGLE: u32 = 0x000000;
pub const RX_CONTINUOUS: u32 = 0xFF_FFFF;

/// Errors surfaced by the command layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    /// SPI transaction failed.
    Spi(E),
    /// BUSY pin read failed.
    Pin,
    /// BUSY never released within [`BUSY_SPIN_BUDGET`] - the chip is
    /// unpowered, held in reset, or the BUSY wiring is wrong.
    BusyTimeout,
}

// ---- Parameter types -------------------------------------------------------

/// SetStandby clock source (Table 13-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StandbyClk {
    /// 13 MHz RC oscillator - the reset-default configuration mode.
    Rc = 0x00,
    /// 32 MHz crystal/TCXO running.
    Xosc = 0x01,
}

/// SetSleep configuration (Table 13-2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SleepConfig {
    /// Warm start: retain the active modem's configuration in the
    /// retention registers (wake in 340 us instead of 3.5 ms, no
    /// reconfiguration needed).
    pub warm_start: bool,
    /// Keep the RTC running on RC64k and allow it to wake the chip
    /// (listen-mode plumbing).
    pub rtc_wake: bool,
}

/// SetRegulatorMode parameter (Table 13-16).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegulatorMode {
    /// LDO only (reset default; roughly doubles RX/TX current).
    Ldo = 0x00,
    /// DC-DC + LDO for STDBY_XOSC, FS, RX and TX.
    DcDcLdo = 0x01,
}

/// SetRxTxFallbackMode parameter (Table 13-23).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackMode {
    Fs = 0x40,
    StandbyXosc = 0x30,
    /// Reset default.
    StandbyRc = 0x20,
}

/// SetPacketType parameter (Table 13-38).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    Gfsk = 0x00,
    LoRa = 0x01,
    LrFhss = 0x03,
}

/// PA ramp times (Table 13-41).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RampTime {
    Us10 = 0x00,
    Us20 = 0x01,
    Us40 = 0x02,
    Us80 = 0x03,
    Us200 = 0x04,
    Us800 = 0x05,
    Us1700 = 0x06,
    Us3400 = 0x07,
}

/// SX1262 PA operating points from the optimal-settings table
/// (Table 13-21): (paDutyCycle, hpMax) with deviceSel=0, paLut=1.
/// Each preset expects the matching `SetTxParams` power of +22 dBm;
/// lower nominal powers with these presets are reached by lowering
/// the SetTxParams value instead. The table's caution stands:
/// paDutyCycle above 0x04 risks PA overstress on the SX1262.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaPreset {
    Dbm22,
    Dbm20,
    Dbm17,
    Dbm14,
}

impl PaPreset {
    fn duty_hpmax(self) -> (u8, u8) {
        match self {
            PaPreset::Dbm22 => (0x04, 0x07),
            PaPreset::Dbm20 => (0x03, 0x05),
            PaPreset::Dbm17 => (0x02, 0x03),
            PaPreset::Dbm14 => (0x02, 0x02),
        }
    }
}

/// TCXO supply voltage on DIO3 (Table 13-35). The regulator needs
/// VDD at least 200 mV above the chosen voltage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcxoVoltage {
    V1_6 = 0x00,
    V1_7 = 0x01,
    V1_8 = 0x02,
    V2_2 = 0x03,
    V2_4 = 0x04,
    V2_7 = 0x05,
    V3_0 = 0x06,
    V3_3 = 0x07,
}

/// LoRa spreading factor (Table 13-47).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoRaSf {
    Sf5 = 0x05,
    Sf6 = 0x06,
    Sf7 = 0x07,
    Sf8 = 0x08,
    Sf9 = 0x09,
    Sf10 = 0x0A,
    Sf11 = 0x0B,
    Sf12 = 0x0C,
}

/// LoRa bandwidth (Table 13-48).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoRaBw {
    Khz7 = 0x00,
    Khz10 = 0x08,
    Khz15 = 0x01,
    Khz20 = 0x09,
    Khz31 = 0x02,
    Khz41 = 0x0A,
    Khz62 = 0x03,
    Khz125 = 0x04,
    Khz250 = 0x05,
    Khz500 = 0x06,
}

/// LoRa coding rate (Table 13-49); the `Li` variants use the long
/// interleaver for extra interference robustness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoRaCr {
    Cr4_5 = 0x01,
    Cr4_6 = 0x02,
    Cr4_7 = 0x03,
    Cr4_8 = 0x04,
    Cr4_5Li = 0x05,
    Cr4_6Li = 0x06,
    Cr4_8Li = 0x07,
}

/// LoRa modulation parameters (section 13.4.5.2). Low data rate
/// optimization is required once the symbol time reaches 16.38 ms -
/// typically SF11/SF12 at BW125 and SF12 at BW250.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoRaModParams {
    pub sf: LoRaSf,
    pub bw: LoRaBw,
    pub cr: LoRaCr,
    pub low_data_rate_opt: bool,
}

/// LoRa packet parameters (section 13.4.6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoRaPacketParams {
    /// Preamble length in symbols (>= 1).
    pub preamble_symbols: u16,
    /// `true` = implicit (fixed-length) header, `false` = explicit
    /// (variable-length) header carrying length/CR/CRC to the peer.
    pub implicit_header: bool,
    /// Payload length to send, or the maximum accepted in RX.
    pub payload_len: u8,
    pub crc_on: bool,
    pub invert_iq: bool,
}

/// GFSK pulse shaping (Table 13-44).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GfskPulseShape {
    None = 0x00,
    GaussianBt0_3 = 0x08,
    GaussianBt0_5 = 0x09,
    GaussianBt0_7 = 0x0A,
    GaussianBt1 = 0x0B,
}

/// GFSK RX bandwidth, double-sideband (Table 13-45).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GfskBw {
    Khz4_8 = 0x1F,
    Khz5_8 = 0x17,
    Khz7_3 = 0x0F,
    Khz9_7 = 0x1E,
    Khz11_7 = 0x16,
    Khz14_6 = 0x0E,
    Khz19_5 = 0x1D,
    Khz23_4 = 0x15,
    Khz29_3 = 0x0D,
    Khz39_0 = 0x1C,
    Khz46_9 = 0x14,
    Khz58_6 = 0x0C,
    Khz78_2 = 0x1B,
    Khz93_8 = 0x13,
    Khz117_3 = 0x0B,
    Khz156_2 = 0x1A,
    Khz187_2 = 0x12,
    Khz234_3 = 0x0A,
    Khz312_0 = 0x19,
    Khz373_6 = 0x11,
    Khz467_0 = 0x09,
}

/// GFSK modulation parameters (section 13.4.5.1). Bit rate range
/// 600 b/s .. 500 kb/s; the RX bandwidth must cover twice the
/// deviation plus the bit rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GfskModParams {
    pub bitrate_bps: u32,
    pub pulse_shape: GfskPulseShape,
    pub rx_bw: GfskBw,
    pub fdev_hz: u32,
}

/// GFSK preamble detector gate length (Table 13-53).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreambleDetector {
    Off = 0x00,
    Bits8 = 0x04,
    Bits16 = 0x05,
    Bits24 = 0x06,
    Bits32 = 0x07,
}

/// GFSK address filtering (Table 13-56).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddrComp {
    Off = 0x00,
    Node = 0x01,
    NodeAndBroadcast = 0x02,
}

/// GFSK CRC handling (Table 13-61). Note the counter-intuitive
/// encoding: `Off` is 0x01, the 1-byte CRC is 0x00.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GfskCrc {
    Off = 0x01,
    OneByte = 0x00,
    TwoByte = 0x02,
    OneByteInverted = 0x04,
    TwoByteInverted = 0x06,
}

/// GFSK packet parameters (section 13.4.6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GfskPacketParams {
    /// Transmitted preamble length in bits (Table 13-52).
    pub preamble_bits: u16,
    pub detector: PreambleDetector,
    /// Sync word length in bits (0..=64); the sync word bytes
    /// themselves live in registers - see
    /// [`Sx1262::set_fsk_sync_word`].
    pub sync_word_bits: u8,
    pub addr_comp: AddrComp,
    /// `true` = variable length (first payload byte is the length).
    pub variable_len: bool,
    pub payload_len: u8,
    pub crc: GfskCrc,
    pub whitening: bool,
}

/// CAD symbol count (Table 13-72).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CadSymbols {
    S1 = 0x00,
    S2 = 0x01,
    S4 = 0x02,
    S8 = 0x03,
    S16 = 0x04,
}

/// CAD exit behavior (Table 13-73).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CadExitMode {
    /// Back to STDBY_RC after the detection window, whatever the
    /// outcome.
    CadOnly = 0x00,
    /// On detected activity stay in RX until a packet or
    /// `timeout_steps` expires.
    CadRx = 0x01,
}

/// SetCadParams arguments (section 13.4.7). Peak/min detection
/// thresholds are SF/BW-dependent; AN1200.48 carries the tuning
/// tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CadParams {
    pub symbols: CadSymbols,
    pub det_peak: u8,
    pub det_min: u8,
    pub exit_mode: CadExitMode,
    /// 15.625 us steps; only used with [`CadExitMode::CadRx`].
    pub timeout_steps: u32,
}

// ---- Decoded status types --------------------------------------------------

/// Chip mode from the status byte (Table 13-76, bits 6:4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipMode {
    StandbyRc,
    StandbyXosc,
    Fs,
    Rx,
    Tx,
    Unknown(u8),
}

/// Command status from the status byte (Table 13-76, bits 3:1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdStatus {
    /// A packet was received and data can be retrieved.
    DataAvailable,
    /// SPI transaction tripped the internal watchdog.
    CmdTimeout,
    /// Invalid opcode or malformed parameters.
    ProcessingError,
    /// Command understood but could not be executed (wrong mode).
    ExecutionFailure,
    /// Current packet transmission finished.
    TxDone,
    Other(u8),
}

/// Decoded GetStatus byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub chip_mode: ChipMode,
    pub cmd: CmdStatus,
}

impl Status {
    fn from_byte(b: u8) -> Self {
        let chip_mode = match (b >> 4) & 0x7 {
            0x2 => ChipMode::StandbyRc,
            0x3 => ChipMode::StandbyXosc,
            0x4 => ChipMode::Fs,
            0x5 => ChipMode::Rx,
            0x6 => ChipMode::Tx,
            m => ChipMode::Unknown(m),
        };
        let cmd = match (b >> 1) & 0x7 {
            0x2 => CmdStatus::DataAvailable,
            0x3 => CmdStatus::CmdTimeout,
            0x4 => CmdStatus::ProcessingError,
            0x5 => CmdStatus::ExecutionFailure,
            0x6 => CmdStatus::TxDone,
            c => CmdStatus::Other(c),
        };
        Status { chip_mode, cmd }
    }
}

/// LoRa packet metrics from GetPacketStatus (Table 13-80).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoRaPacketStatus {
    /// Average RSSI over the packet, dBm.
    pub rssi_pkt_dbm: i16,
    /// SNR of the packet in quarter-dB (two's complement * 4).
    pub snr_pkt_db_x4: i8,
    /// RSSI of the despread LoRa signal, dBm.
    pub signal_rssi_dbm: i16,
}

/// GFSK packet metrics from GetPacketStatus (Table 13-80).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GfskPacketStatus {
    /// Raw RxStatus flag byte (bit 0 pkt sent .. bit 7 preamble
    /// err - see Table 13-80).
    pub rx_status: u8,
    /// RSSI latched at sync-address detection, dBm.
    pub rssi_sync_dbm: i16,
    /// Average RSSI over the payload, dBm.
    pub rssi_avg_dbm: i16,
}

/// GetStats counters (Table 13-83). The third counter is length
/// errors in GFSK and header errors in LoRa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub packets_received: u16,
    pub crc_errors: u16,
    pub length_or_header_errors: u16,
}

// ---- Conversion helpers ----------------------------------------------------

/// Milliseconds to 15.625 us timer steps, saturating at the 24-bit
/// ceiling (~262 s). Used by TX/RX timeouts, the TCXO startup delay
/// and the duty-cycle periods.
pub fn ms_to_steps(ms: u32) -> u32 {
    ms.saturating_mul(regs::TIMER_STEPS_PER_MS).min(0xFF_FFFE)
}

/// RF frequency in Hz to the 32-bit PLL word:
/// `RfFreq = freq * 2^25 / F_XTAL` (section 13.4.1).
pub fn hz_to_pll_steps(freq_hz: u32) -> u32 {
    (((freq_hz as u64) << 25) / regs::F_XTAL_HZ as u64) as u32
}

/// GFSK bit rate in b/s to the 24-bit `br` word:
/// `br = 32 * F_XTAL / bitrate` (Table 13-43).
pub fn bitrate_to_br(bitrate_bps: u32) -> u32 {
    (32u64 * regs::F_XTAL_HZ as u64 / bitrate_bps as u64) as u32
}

/// GFSK frequency deviation in Hz to the 24-bit `fdev` word:
/// `fdev = deviation * 2^25 / F_XTAL` (Table 13-46).
pub fn fdev_to_steps(fdev_hz: u32) -> u32 {
    (((fdev_hz as u64) << 25) / regs::F_XTAL_HZ as u64) as u32
}

// ---- Driver ----------------------------------------------------------------

/// SX1261/2 command-layer driver. Stateless: the SPI device and the
/// BUSY pin are borrowed per call, so the radio can live on a
/// shared bus owned elsewhere.
pub struct Sx1262;

impl Sx1262 {
    pub fn new() -> Self {
        Sx1262
    }

    // -- Plumbing ------------------------------------------------------------

    /// Spin until BUSY releases. Every command entry point calls
    /// this first (section 8.3.1: a command sent while BUSY is high
    /// is ignored or corrupts state).
    fn wait_busy<B: InputPin>(&self, busy: &mut B) -> Result<(), Error<()>> {
        for _ in 0..BUSY_SPIN_BUDGET {
            match busy.is_low() {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(_) => return Err(Error::Pin),
            }
        }
        Err(Error::BusyTimeout)
    }

    fn map_busy<E>(e: Error<()>) -> Error<E> {
        match e {
            Error::Pin => Error::Pin,
            Error::BusyTimeout => Error::BusyTimeout,
            Error::Spi(()) => Error::BusyTimeout, // unreachable
        }
    }

    /// Write-only command: opcode + parameters in one transaction.
    fn command<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        bytes: &[u8],
    ) -> Result<(), Error<S::Error>> {
        self.wait_busy(busy).map_err(Self::map_busy)?;
        spi.write(bytes).map_err(Error::Spi)
    }

    /// Read command: opcode (+ params) out, then `out` clocked in.
    /// Per the transaction tables the first byte read back is the
    /// status byte, so callers size `out` as 1 + payload.
    fn read_command<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        cmd: &[u8],
        out: &mut [u8],
    ) -> Result<(), Error<S::Error>> {
        self.wait_busy(busy).map_err(Self::map_busy)?;
        spi.transaction(&mut [Operation::Write(cmd), Operation::Read(out)])
            .map_err(Error::Spi)
    }

    // -- Reset / wake --------------------------------------------------------

    /// Wake the chip from Sleep mode: any NSS falling edge starts
    /// the wake-up (section 9.3), so a harmless GetStatus is
    /// clocked out WITHOUT the usual BUSY pre-check (BUSY is held
    /// high for the whole sleep). Then wait for the boot to finish -
    /// 340 us warm, 3.5 ms cold (Table 8-2).
    pub fn wake<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<(), Error<S::Error>> {
        let mut st = [0u8; 1];
        spi.transaction(&mut [
            Operation::Write(&[opcode::GET_STATUS]),
            Operation::Read(&mut st),
        ])
        .map_err(Error::Spi)?;
        self.wait_busy(busy).map_err(Self::map_busy)
    }

    // -- Operational modes ---------------------------------------------------

    /// Enter SLEEP (section 13.1.1; STDBY only). After the NSS
    /// rising edge the chip is unresponsive for ~500 us while it
    /// saves state and powers down - do not talk to it (the next
    /// BUSY wait does NOT cover this: BUSY stays high for the whole
    /// sleep, so the next contact must be [`Sx1262::wake`]).
    pub fn set_sleep<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        cfg: SleepConfig,
    ) -> Result<(), Error<S::Error>> {
        let mut b = 0u8;
        if cfg.warm_start {
            b |= 1 << 2;
        }
        if cfg.rtc_wake {
            b |= 1 << 0;
        }
        self.command(spi, busy, &[opcode::SET_SLEEP, b])
    }

    pub fn set_standby<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        clk: StandbyClk,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::SET_STANDBY, clk as u8])
    }

    pub fn set_fs<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::SET_FS])
    }

    /// Enter TX. `timeout_steps` of 15.625 us each;
    /// [`TIMEOUT_SINGLE`] disables the timeout (section 13.1.4).
    pub fn set_tx<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        timeout_steps: u32,
    ) -> Result<(), Error<S::Error>> {
        let t = timeout_steps.to_be_bytes();
        self.command(spi, busy, &[opcode::SET_TX, t[1], t[2], t[3]])
    }

    /// Enter RX. [`TIMEOUT_SINGLE`] = single reception, no timeout;
    /// [`RX_CONTINUOUS`] = stay in RX across packets; anything else
    /// is a timeout in 15.625 us steps (section 13.1.5).
    pub fn set_rx<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        timeout_steps: u32,
    ) -> Result<(), Error<S::Error>> {
        let t = timeout_steps.to_be_bytes();
        self.command(spi, busy, &[opcode::SET_RX, t[1], t[2], t[3]])
    }

    /// Stop the RX timeout on preamble detection instead of sync
    /// word / header detection (section 13.1.6).
    pub fn stop_timer_on_preamble<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        enable: bool,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::STOP_TIMER_ON_PREAMBLE, enable as u8])
    }

    /// Listen mode: loop RX for `rx_steps` then sleep for
    /// `sleep_steps`, both in 15.625 us units (section 13.1.7).
    /// Exit via packet reception or SetStandby during an RX window.
    pub fn set_rx_duty_cycle<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        rx_steps: u32,
        sleep_steps: u32,
    ) -> Result<(), Error<S::Error>> {
        let r = rx_steps.to_be_bytes();
        let s = sleep_steps.to_be_bytes();
        self.command(
            spi,
            busy,
            &[opcode::SET_RX_DUTY_CYCLE, r[1], r[2], r[3], s[1], s[2], s[3]],
        )
    }

    /// Run one channel-activity detection with the parameters from
    /// [`Sx1262::set_cad_params`] (LoRa only, section 13.1.8).
    pub fn set_cad<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::SET_CAD])
    }

    /// Test mode: unmodulated carrier at the configured frequency
    /// and power (section 13.1.9).
    pub fn set_tx_continuous_wave<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::SET_TX_CONTINUOUS_WAVE])
    }

    /// Test mode: endless preamble (section 13.1.10).
    pub fn set_tx_infinite_preamble<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::SET_TX_INFINITE_PREAMBLE])
    }

    /// Select LDO-only or DC-DC+LDO regulation (section 13.1.11).
    /// Hardware-dependent - only enable DC-DC on designs that fit
    /// the inductor.
    pub fn set_regulator_mode<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        mode: RegulatorMode,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::SET_REGULATOR_MODE, mode as u8])
    }

    /// Calibrate the blocks in `mask` (see [`regs::calibrate`];
    /// STDBY_RC only, BUSY high for the ~3.5 ms of a full run).
    pub fn calibrate<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        mask: u8,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::CALIBRATE, mask])
    }

    /// Image calibration for a band preset from
    /// [`regs::image_band`] (section 13.1.13).
    pub fn calibrate_image<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        band: (u8, u8),
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::CALIBRATE_IMAGE, band.0, band.1])
    }

    /// Image calibration for an arbitrary MHz range, using the
    /// 4 MHz-step floor/ceil encoding from the section 9.2.1
    /// reference code.
    pub fn calibrate_image_mhz<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        freq1_mhz: u16,
        freq2_mhz: u16,
    ) -> Result<(), Error<S::Error>> {
        let f1 = (freq1_mhz / 4) as u8;
        let f2 = ((freq2_mhz + 3) / 4) as u8;
        self.calibrate_image(spi, busy, (f1, f2))
    }

    /// Raw SetPaConfig (section 13.1.14) - prefer
    /// [`Sx1262::set_pa_preset`] unless a non-tabulated operating
    /// point is genuinely needed.
    pub fn set_pa_config<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        pa_duty_cycle: u8,
        hp_max: u8,
        device_sel: u8,
        pa_lut: u8,
    ) -> Result<(), Error<S::Error>> {
        self.command(
            spi,
            busy,
            &[opcode::SET_PA_CONFIG, pa_duty_cycle, hp_max, device_sel, pa_lut],
        )
    }

    /// SX1262 PA operating point from the optimal-settings table.
    pub fn set_pa_preset<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        preset: PaPreset,
    ) -> Result<(), Error<S::Error>> {
        let (duty, hpmax) = preset.duty_hpmax();
        self.set_pa_config(spi, busy, duty, hpmax, 0x00, 0x01)
    }

    /// Mode entered after TxDone / RxDone (section 13.1.15).
    pub fn set_rx_tx_fallback_mode<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        mode: FallbackMode,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::SET_RX_TX_FALLBACK_MODE, mode as u8])
    }

    // -- Registers and buffer ------------------------------------------------

    /// Write `data` to consecutive registers starting at `addr`
    /// (section 13.2.1).
    pub fn write_register<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        addr: u16,
        data: &[u8],
    ) -> Result<(), Error<S::Error>> {
        self.wait_busy(busy).map_err(Self::map_busy)?;
        let a = addr.to_be_bytes();
        spi.transaction(&mut [
            Operation::Write(&[opcode::WRITE_REGISTER, a[0], a[1]]),
            Operation::Write(data),
        ])
        .map_err(Error::Spi)
    }

    /// Read consecutive registers starting at `addr` into `data`
    /// (section 13.2.2 - one NOP/status byte precedes the data).
    pub fn read_register<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        addr: u16,
        data: &mut [u8],
    ) -> Result<(), Error<S::Error>> {
        self.wait_busy(busy).map_err(Self::map_busy)?;
        let a = addr.to_be_bytes();
        let mut status = [0u8; 1];
        spi.transaction(&mut [
            Operation::Write(&[opcode::READ_REGISTER, a[0], a[1]]),
            Operation::Read(&mut status),
            Operation::Read(data),
        ])
        .map_err(Error::Spi)
    }

    /// Read-modify-write one register byte: clear `clear` bits,
    /// then set `set` bits.
    pub fn update_register<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        addr: u16,
        clear: u8,
        set: u8,
    ) -> Result<(), Error<S::Error>> {
        let mut v = [0u8; 1];
        self.read_register(spi, busy, addr, &mut v)?;
        let new = (v[0] & !clear) | set;
        self.write_register(spi, busy, addr, &[new])
    }

    /// Write TX payload into the 256-byte data buffer at `offset`
    /// (section 13.2.3; the address wraps at 256).
    pub fn write_buffer<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        offset: u8,
        data: &[u8],
    ) -> Result<(), Error<S::Error>> {
        self.wait_busy(busy).map_err(Self::map_busy)?;
        spi.transaction(&mut [
            Operation::Write(&[opcode::WRITE_BUFFER, offset]),
            Operation::Write(data),
        ])
        .map_err(Error::Spi)
    }

    /// Read received payload from the data buffer at `offset`
    /// (section 13.2.4 - one NOP/status byte precedes the data).
    pub fn read_buffer<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        offset: u8,
        data: &mut [u8],
    ) -> Result<(), Error<S::Error>> {
        self.wait_busy(busy).map_err(Self::map_busy)?;
        let mut status = [0u8; 1];
        spi.transaction(&mut [
            Operation::Write(&[opcode::READ_BUFFER, offset]),
            Operation::Read(&mut status),
            Operation::Read(data),
        ])
        .map_err(Error::Spi)
    }

    // -- DIO / IRQ -----------------------------------------------------------

    /// Route IRQ sources (see [`regs::irq`]) to the DIO pins
    /// (section 13.3.1). A source fires a DIO only when present in
    /// both `irq_mask` and that DIO's mask; DIOs claimed by the RF
    /// switch or TCXO never fire.
    pub fn set_dio_irq_params<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        irq_mask: u16,
        dio1_mask: u16,
        dio2_mask: u16,
        dio3_mask: u16,
    ) -> Result<(), Error<S::Error>> {
        let i = irq_mask.to_be_bytes();
        let d1 = dio1_mask.to_be_bytes();
        let d2 = dio2_mask.to_be_bytes();
        let d3 = dio3_mask.to_be_bytes();
        self.command(
            spi,
            busy,
            &[
                opcode::SET_DIO_IRQ_PARAMS,
                i[0], i[1], d1[0], d1[1], d2[0], d2[1], d3[0], d3[1],
            ],
        )
    }

    /// Currently latched IRQ flags (section 13.3.3).
    pub fn irq_status<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<u16, Error<S::Error>> {
        let mut out = [0u8; 3];
        self.read_command(spi, busy, &[opcode::GET_IRQ_STATUS], &mut out)?;
        Ok(u16::from_be_bytes([out[1], out[2]]))
    }

    /// Clear the IRQ flags in `mask` (section 13.3.4).
    pub fn clear_irq<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        mask: u16,
    ) -> Result<(), Error<S::Error>> {
        let m = mask.to_be_bytes();
        self.command(spi, busy, &[opcode::CLEAR_IRQ_STATUS, m[0], m[1]])
    }

    /// Hand DIO2 to the internal state machine as a TX/RX antenna
    /// switch control: high during TX, low otherwise
    /// (section 13.3.5).
    pub fn set_dio2_rf_switch<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        enable: bool,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::SET_DIO2_AS_RF_SWITCH_CTRL, enable as u8])
    }

    /// Hand DIO3 to the state machine as the TCXO supply
    /// (section 13.3.6). `delay_steps` (15.625 us units) gates the
    /// 32 MHz clock while the oscillator stabilizes. At POR or
    /// cold-start wake the chip has already tried its crystal path,
    /// so XOSC_START_ERR is latched afterwards by design - clear it
    /// via [`Sx1262::clear_device_errors`] and recalibrate.
    pub fn set_dio3_tcxo<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        voltage: TcxoVoltage,
        delay_steps: u32,
    ) -> Result<(), Error<S::Error>> {
        let d = delay_steps.to_be_bytes();
        self.command(
            spi,
            busy,
            &[opcode::SET_DIO3_AS_TCXO_CTRL, voltage as u8, d[1], d[2], d[3]],
        )
    }

    // -- RF, modulation, packet ----------------------------------------------

    /// Carrier frequency in Hz (section 13.4.1).
    pub fn set_rf_frequency_hz<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        freq_hz: u32,
    ) -> Result<(), Error<S::Error>> {
        let f = hz_to_pll_steps(freq_hz).to_be_bytes();
        self.command(spi, busy, &[opcode::SET_RF_FREQUENCY, f[0], f[1], f[2], f[3]])
    }

    /// Select the modem. Must be the FIRST radio-configuration
    /// command (section 14.5); switching drops the other modem's
    /// parameters and requires STDBY_RC.
    pub fn set_packet_type<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        t: PacketType,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::SET_PACKET_TYPE, t as u8])
    }

    /// Currently selected modem, raw (Table 13-38 values).
    pub fn packet_type<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<u8, Error<S::Error>> {
        let mut out = [0u8; 2];
        self.read_command(spi, busy, &[opcode::GET_PACKET_TYPE], &mut out)?;
        Ok(out[1])
    }

    /// TX power in dBm and PA ramp time (section 13.4.4). Range
    /// -9..=+22 dBm with the high-power PA (SX1262); the PA config
    /// preset decides how much of that range is efficient.
    pub fn set_tx_params<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        power_dbm: i8,
        ramp: RampTime,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::SET_TX_PARAMS, power_dbm as u8, ramp as u8])
    }

    /// LoRa modulation parameters (section 13.4.5.2). Also applies
    /// the section 15.1 modulation-quality workaround: TxModulation
    /// bit 2 must be 0 at BW500 and 1 at every other LoRa
    /// bandwidth.
    pub fn set_lora_modulation<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        p: LoRaModParams,
    ) -> Result<(), Error<S::Error>> {
        self.command(
            spi,
            busy,
            &[
                opcode::SET_MODULATION_PARAMS,
                p.sf as u8,
                p.bw as u8,
                p.cr as u8,
                p.low_data_rate_opt as u8,
            ],
        )?;
        if p.bw == LoRaBw::Khz500 {
            self.update_register(spi, busy, reg::TX_MODULATION, 0x04, 0x00)
        } else {
            self.update_register(spi, busy, reg::TX_MODULATION, 0x00, 0x04)
        }
    }

    /// GFSK modulation parameters (section 13.4.5.1). Also applies
    /// the section 15.1 workaround (TxModulation bit 2 = 1 for any
    /// (G)FSK configuration).
    pub fn set_gfsk_modulation<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        p: GfskModParams,
    ) -> Result<(), Error<S::Error>> {
        let br = bitrate_to_br(p.bitrate_bps).to_be_bytes();
        let fd = fdev_to_steps(p.fdev_hz).to_be_bytes();
        self.command(
            spi,
            busy,
            &[
                opcode::SET_MODULATION_PARAMS,
                br[1],
                br[2],
                br[3],
                p.pulse_shape as u8,
                p.rx_bw as u8,
                fd[1],
                fd[2],
                fd[3],
            ],
        )?;
        self.update_register(spi, busy, reg::TX_MODULATION, 0x00, 0x04)
    }

    /// LoRa packet parameters (section 13.4.6.2). Also applies the
    /// section 15.4 inverted-IQ workaround (IQ polarity register
    /// bit 2: cleared for inverted IQ, set for standard).
    pub fn set_lora_packet_params<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        p: LoRaPacketParams,
    ) -> Result<(), Error<S::Error>> {
        let pre = p.preamble_symbols.to_be_bytes();
        self.command(
            spi,
            busy,
            &[
                opcode::SET_PACKET_PARAMS,
                pre[0],
                pre[1],
                p.implicit_header as u8,
                p.payload_len,
                p.crc_on as u8,
                p.invert_iq as u8,
            ],
        )?;
        if p.invert_iq {
            self.update_register(spi, busy, reg::IQ_POLARITY, 0x04, 0x00)
        } else {
            self.update_register(spi, busy, reg::IQ_POLARITY, 0x00, 0x04)
        }
    }

    /// GFSK packet parameters (section 13.4.6.1).
    pub fn set_gfsk_packet_params<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        p: GfskPacketParams,
    ) -> Result<(), Error<S::Error>> {
        let pre = p.preamble_bits.to_be_bytes();
        self.command(
            spi,
            busy,
            &[
                opcode::SET_PACKET_PARAMS,
                pre[0],
                pre[1],
                p.detector as u8,
                p.sync_word_bits,
                p.addr_comp as u8,
                p.variable_len as u8,
                p.payload_len,
                p.crc as u8,
                p.whitening as u8,
            ],
        )
    }

    /// CAD parameters (section 13.4.7).
    pub fn set_cad_params<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        p: CadParams,
    ) -> Result<(), Error<S::Error>> {
        let t = p.timeout_steps.to_be_bytes();
        self.command(
            spi,
            busy,
            &[
                opcode::SET_CAD_PARAMS,
                p.symbols as u8,
                p.det_peak,
                p.det_min,
                p.exit_mode as u8,
                t[1],
                t[2],
                t[3],
            ],
        )
    }

    /// TX and RX base addresses inside the 256-byte data buffer
    /// (section 13.4.8).
    pub fn set_buffer_base<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        tx_base: u8,
        rx_base: u8,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::SET_BUFFER_BASE_ADDRESS, tx_base, rx_base])
    }

    /// Number of LoRa symbols required to validate a lock before
    /// RxDone can fire (section 13.4.9; 0 = first symbol wins).
    pub fn set_lora_symb_num_timeout<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        symb_num: u8,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::SET_LORA_SYMB_NUM_TIMEOUT, symb_num])
    }

    // -- Status --------------------------------------------------------------

    /// Decoded chip status (section 13.5.1).
    pub fn status<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<Status, Error<S::Error>> {
        let mut out = [0u8; 1];
        self.read_command(spi, busy, &[opcode::GET_STATUS], &mut out)?;
        Ok(Status::from_byte(out[0]))
    }

    /// Length and start offset of the last received payload
    /// (section 13.5.2).
    pub fn rx_buffer_status<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<(u8, u8), Error<S::Error>> {
        let mut out = [0u8; 3];
        self.read_command(spi, busy, &[opcode::GET_RX_BUFFER_STATUS], &mut out)?;
        Ok((out[1], out[2]))
    }

    /// LoRa metrics of the last packet (section 13.5.3).
    pub fn lora_packet_status<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<LoRaPacketStatus, Error<S::Error>> {
        let mut out = [0u8; 4];
        self.read_command(spi, busy, &[opcode::GET_PACKET_STATUS], &mut out)?;
        Ok(LoRaPacketStatus {
            rssi_pkt_dbm: -((out[1] as i16) / 2),
            snr_pkt_db_x4: out[2] as i8,
            signal_rssi_dbm: -((out[3] as i16) / 2),
        })
    }

    /// GFSK metrics of the last packet (section 13.5.3).
    pub fn gfsk_packet_status<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<GfskPacketStatus, Error<S::Error>> {
        let mut out = [0u8; 4];
        self.read_command(spi, busy, &[opcode::GET_PACKET_STATUS], &mut out)?;
        Ok(GfskPacketStatus {
            rx_status: out[1],
            rssi_sync_dbm: -((out[2] as i16) / 2),
            rssi_avg_dbm: -((out[3] as i16) / 2),
        })
    }

    /// Instantaneous RSSI during reception, dBm (section 13.5.4).
    pub fn rssi_inst_dbm<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<i16, Error<S::Error>> {
        let mut out = [0u8; 2];
        self.read_command(spi, busy, &[opcode::GET_RSSI_INST], &mut out)?;
        Ok(-((out[1] as i16) / 2))
    }

    /// Reception counters since the last reset (section 13.5.5).
    pub fn stats<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<Stats, Error<S::Error>> {
        let mut out = [0u8; 7];
        self.read_command(spi, busy, &[opcode::GET_STATS], &mut out)?;
        Ok(Stats {
            packets_received: u16::from_be_bytes([out[1], out[2]]),
            crc_errors: u16::from_be_bytes([out[3], out[4]]),
            length_or_header_errors: u16::from_be_bytes([out[5], out[6]]),
        })
    }

    /// Reset the GetStats counters (section 13.5.6).
    pub fn reset_stats<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::RESET_STATS, 0, 0, 0, 0, 0, 0])
    }

    /// Latched device error flags (see [`regs::op_error`];
    /// section 13.6.1).
    pub fn device_errors<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<u16, Error<S::Error>> {
        let mut out = [0u8; 3];
        self.read_command(spi, busy, &[opcode::GET_DEVICE_ERRORS], &mut out)?;
        Ok(u16::from_be_bytes([out[1], out[2]]))
    }

    /// Clear ALL latched device errors (section 13.6.2 - they
    /// cannot be cleared individually).
    pub fn clear_device_errors<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<(), Error<S::Error>> {
        self.command(spi, busy, &[opcode::CLEAR_DEVICE_ERRORS, 0, 0])
    }

    // -- Register-level helpers ----------------------------------------------

    /// LoRa sync word (see [`regs::lora_sync_word`] for the
    /// public/private network values).
    pub fn set_lora_sync_word<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        word: u16,
    ) -> Result<(), Error<S::Error>> {
        self.write_register(spi, busy, reg::LORA_SYNC_WORD_MSB, &word.to_be_bytes())
    }

    /// GFSK sync word bytes (up to 8; pair with
    /// `GfskPacketParams::sync_word_bits`).
    pub fn set_fsk_sync_word<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        word: &[u8],
    ) -> Result<(), Error<S::Error>> {
        self.write_register(spi, busy, reg::FSK_SYNC_WORD_0, &word[..word.len().min(8)])
    }

    /// RX gain: boosted (+~2 dB sensitivity for ~2 mA) or the
    /// power-saving default. Boosted mode also programs the
    /// retention trio so the setting survives warm-start sleep
    /// (section 9.6).
    pub fn set_rx_gain_boosted<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        boosted: bool,
    ) -> Result<(), Error<S::Error>> {
        self.write_register(spi, busy, reg::RX_GAIN, &[if boosted { 0x96 } else { 0x94 }])?;
        if boosted {
            self.write_register(spi, busy, reg::RETENTION_LIST_0, &[0x01])?;
            self.write_register(spi, busy, reg::RETENTION_LIST_1, &[0x08, 0xAC])?;
        }
        Ok(())
    }

    /// Over-current protection, raw register value in 2.5 mA steps
    /// (reset default 0x38 = 140 mA on the SX1262).
    pub fn set_ocp_raw<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
        value: u8,
    ) -> Result<(), Error<S::Error>> {
        self.write_register(spi, busy, reg::OCP_CONFIGURATION, &[value])
    }

    /// Section 15.2 workaround: relax the SX1262 PA clamp
    /// (TxClampConfig bits 4:1 to 1111) so antenna mismatch does
    /// not cost 5-6 dB of output. Apply once after every POR or
    /// cold-start wake.
    pub fn apply_tx_clamp_workaround<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<(), Error<S::Error>> {
        self.update_register(spi, busy, reg::TX_CLAMP_CONFIG, 0x00, 0x1E)
    }

    /// Section 15.3 workaround: after any RX with an active timeout
    /// in implicit-header LoRa mode, stop the RTC and clear the
    /// latched event so the stale timer cannot fire in a later
    /// mode.
    pub fn apply_implicit_timeout_workaround<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<(), Error<S::Error>> {
        self.write_register(spi, busy, reg::RTC_CONTROL, &[0x00])?;
        self.update_register(spi, busy, reg::EVENT_MASK, 0x00, 0x02)
    }

    /// 4 bytes of hardware entropy (Table 12-1). Only meaningful
    /// while the receiver is running - put the chip in RX
    /// (continuous, IRQs masked) before sampling.
    pub fn random_u32<S: SpiDevice<u8>, B: InputPin>(
        &self,
        spi: &mut S,
        busy: &mut B,
    ) -> Result<u32, Error<S::Error>> {
        let mut b = [0u8; 4];
        self.read_register(spi, busy, reg::RANDOM_NUMBER_0, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pll_steps_for_eu868() {
        // 868 MHz * 2^25 / 32 MHz = 27.125 * 2^25
        assert_eq!(hz_to_pll_steps(868_000_000), 0x3640_0000);
    }

    #[test]
    fn timer_steps_per_ms() {
        assert_eq!(ms_to_steps(1), 64);
        assert_eq!(ms_to_steps(1000), 64_000);
        // Saturates below the continuous-RX sentinel.
        assert!(ms_to_steps(u32::MAX) < RX_CONTINUOUS);
    }

    #[test]
    fn gfsk_conversions() {
        // Table 13-43 example rate: br = 32 * 32 MHz / 4800 b/s.
        assert_eq!(bitrate_to_br(4800), 213_333);
        // 25 kHz deviation: 25e3 * 2^25 / 32e6.
        assert_eq!(fdev_to_steps(25_000), 26_214);
    }

    #[test]
    fn image_calibration_step_encoding() {
        // Section 9.2.1 pseudo-code: floor / ceil on the 4 MHz
        // grid. 863 floors to 0xD7 like the Table 9-2 preset; the
        // ceil side is one step tighter than the preset's 0xDB -
        // the presets are deliberately wider than the exact grid.
        assert_eq!(863u16 / 4, 0xD7);
        assert_eq!((870u16 + 3) / 4, 0xDA);
    }

    #[test]
    fn status_decode() {
        // STBY_RC + data available: mode 0x2 << 4, cmd 0x2 << 1.
        let s = Status::from_byte(0x24);
        assert_eq!(s.chip_mode, ChipMode::StandbyRc);
        assert_eq!(s.cmd, CmdStatus::DataAvailable);
    }
}
