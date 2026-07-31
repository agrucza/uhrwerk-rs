//! Bosch BHI260AP smart sensor hub driver (I2C host interface).
//!
//! Unlike a register-style IMU, the BHI260AP boots dead: the host must
//! upload a firmware image into its program RAM on every cold start
//! (Host Boot Mode, datasheet Section 5.2.1) before any sensor exists.
//! After boot everything runs over the BHY2 host protocol: commands in
//! through channel 0, sensor data out through two FIFO channels,
//! command responses through a status channel - see `regs` for the
//! register/command reference and `fifo` for the stream format.
//!
//! The matching firmware image ships with this driver ([`FIRMWARE`],
//! from Bosch's BHI2xy_SensorAPI repository, BSD-3-Clause - see
//! `fw/BOSCH-LICENSE`), so a board only calls the boot sequence; no
//! bin-side blob handling.
//!
//! All methods are synchronous single transactions against a borrowed
//! `embedded-hal` I2C bus, mirroring the other drivers in this crate.
//! The two long-running operations are split into stages so a caller
//! sharing the bus can release it between steps and pace itself:
//!
//! - firmware upload: [`Bhi260::begin_ram_upload`] once, then
//!   [`Bhi260::upload_chunk`] repeatedly (a ~104 KB image in one
//!   transaction would monopolize a shared bus for seconds);
//! - operations that answer via the status FIFO (self-test, FOC):
//!   `request_*` to start, [`Bhi260::try_read_status_packet`] to poll.
//!
//! Parameter reads are answered quickly and are handled internally
//! with a bounded poll ([`Bhi260::param_read`]).

pub mod fifo;
pub mod regs;

pub use regs::{boot_status, chip_control, cmd, error_description, flush, hif_control,
    int_status, irq_control, param, phys, reg, status};

use embedded_hal::i2c::I2c as I2cTrait;

/// Default I2C address (HSDO strapped low; high selects 0x29 -
/// datasheet Table 5).
pub const ADDR: u8 = 0x28;

/// Expected identity values (datasheet 12.1.15/12.1.17/12.1.23).
pub const PRODUCT_ID: u8 = 0x89;
pub const ROM_VERSION: u16 = 0x142E;

/// The RAM-boot firmware image for a standalone BHI260AP (no aux
/// sensors, no hub-attached flash), from Bosch's BHI2xy_SensorAPI
/// repository. BSD-3-Clause - the license text ships alongside the
/// blob in `fw/BOSCH-LICENSE` and must stay with it.
pub const FIRMWARE: &[u8] = include_bytes!("fw/BHI260AP.fw");

/// Upload chunk size in bytes (multiple of 4). 256 bytes keeps a
/// shared-bus lock per chunk in the ~7 ms range at 400 kHz.
pub const UPLOAD_CHUNK: usize = 256;

/// Max bytes-to-follow this driver reads back for one parameter
/// (largest standard parameter is Virtual Sensor Information, 28
/// bytes; BSX calibration blocks go up to 68).
pub const MAX_PARAM_BYTES: usize = 68;

/// Bounded spins when waiting for a synchronous status packet in
/// [`Bhi260::param_read`]. Each spin is one Interrupt Status register
/// read (~100 us at 400 kHz), so this bounds one wait at ~50 ms.
/// Callers that can yield should treat a timeout as retryable - the
/// firmware answers parameter reads late while its framework is busy
/// (observed right after boot, while BSX finishes initializing).
const STATUS_SPINS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Error<E> {
    /// I2C bus error.
    Bus(E),
    /// The chip reported a firmware/bootloader error code (register
    /// 0x2E); decode with [`error_description`].
    Chip(u8),
    /// A command was rejected (Command Error status packet, Table 86).
    Command { command: u16, error: u8 },
    /// A response never arrived or had an unexpected shape.
    Protocol(&'static str),
}

/// Identity registers snapshot (see [`Bhi260::probe`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChipInfo {
    pub product_id: u8,
    pub revision: u8,
    pub rom_version: u16,
    pub kernel_version: u16,
    pub user_version: u16,
    pub feature_status: u8,
}

/// Self-test / FOC result decoded from their status packets
/// (Tables 54/56).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TestResult {
    pub sensor_id: u8,
    /// Self-test: 0 = pass, 1/2/4 = X/Y/Z axis failed, 7 = multiple,
    /// 8 = unsupported, 9 = no device. FOC: 0 = success, 0x65 = fail.
    pub status: u8,
    pub offsets: [i16; 3],
}

pub struct Bhi260 {
    addr: u8,
}

impl Default for Bhi260 {
    fn default() -> Self {
        Self::new()
    }
}

impl Bhi260 {
    pub fn new() -> Self {
        Self { addr: ADDR }
    }

    pub fn with_address(addr: u8) -> Self {
        Self { addr }
    }

    // ---- Raw register access -------------------------------------

    pub fn read_regs<I: I2cTrait>(
        &self,
        i2c: &mut I,
        reg: u8,
        buf: &mut [u8],
    ) -> Result<(), Error<I::Error>> {
        i2c.write_read(self.addr, &[reg], buf).map_err(Error::Bus)
    }

    pub fn read_reg<I: I2cTrait>(&self, i2c: &mut I, reg: u8) -> Result<u8, Error<I::Error>> {
        let mut b = [0u8];
        self.read_regs(i2c, reg, &mut b)?;
        Ok(b[0])
    }

    pub fn write_reg<I: I2cTrait>(
        &self,
        i2c: &mut I,
        reg: u8,
        val: u8,
    ) -> Result<(), Error<I::Error>> {
        i2c.write(self.addr, &[reg, val]).map_err(Error::Bus)
    }

    /// Burst write to a register (used for the command channel). One
    /// I2C transaction: [reg, data...].
    fn write_burst<I: I2cTrait>(
        &self,
        i2c: &mut I,
        reg: u8,
        data: &[u8],
    ) -> Result<(), Error<I::Error>> {
        let mut buf = [0u8; 1 + UPLOAD_CHUNK];
        debug_assert!(data.len() <= UPLOAD_CHUNK);
        buf[0] = reg;
        buf[1..1 + data.len()].copy_from_slice(data);
        i2c.write(self.addr, &buf[..1 + data.len()]).map_err(Error::Bus)
    }

    // ---- Identity / status ---------------------------------------

    /// Read the identity registers. Valid as soon as the bootloader is
    /// ready; kernel/user versions read 0 until firmware has booted.
    pub fn probe<I: I2cTrait>(&self, i2c: &mut I) -> Result<ChipInfo, Error<I::Error>> {
        let mut b = [0u8; 9]; // 0x1C..=0x24
        self.read_regs(i2c, reg::PRODUCT_ID, &mut b)?;
        Ok(ChipInfo {
            product_id: b[0],
            revision: b[1],
            rom_version: u16::from_le_bytes([b[2], b[3]]),
            kernel_version: u16::from_le_bytes([b[4], b[5]]),
            user_version: u16::from_le_bytes([b[6], b[7]]),
            feature_status: b[8],
        })
    }

    pub fn boot_status<I: I2cTrait>(&self, i2c: &mut I) -> Result<u8, Error<I::Error>> {
        self.read_reg(i2c, reg::BOOT_STATUS)
    }

    /// Single readiness check (Boot Status bit 4). After a reset the
    /// bootloader is ready within T_boot_bl_host (1.3 ms max); after a
    /// Boot Program RAM command the booted firmware re-asserts it
    /// within ~T_boot_fw_host (~81 ms typ). Callers poll with their
    /// own delays.
    pub fn host_interface_ready<I: I2cTrait>(&self, i2c: &mut I) -> Result<bool, Error<I::Error>> {
        Ok(self.boot_status(i2c)? & boot_status::HOST_INTERFACE_READY != 0)
    }

    pub fn interrupt_status<I: I2cTrait>(&self, i2c: &mut I) -> Result<u8, Error<I::Error>> {
        self.read_reg(i2c, reg::INTERRUPT_STATUS)
    }

    /// Firmware error code (0 = none); decode with
    /// [`error_description`].
    pub fn error_value<I: I2cTrait>(&self, i2c: &mut I) -> Result<u8, Error<I::Error>> {
        self.read_reg(i2c, reg::ERROR_VALUE)
    }

    pub fn clear_error_regs<I: I2cTrait>(&self, i2c: &mut I) -> Result<(), Error<I::Error>> {
        self.write_reg(i2c, reg::CHIP_CONTROL, chip_control::CLEAR_ERROR_REGS)
    }

    // ---- Boot sequence (Section 5.2.1) ---------------------------

    /// Host reset (Reset Request bit 0, self-clearing). Must be a
    /// single-register transaction; the caller shall wait at least
    /// T_wait (4 us) before the next transaction and then poll
    /// [`Self::host_interface_ready`].
    pub fn host_reset<I: I2cTrait>(&self, i2c: &mut I) -> Result<(), Error<I::Error>> {
        self.write_reg(i2c, reg::RESET_REQUEST, 0x01)
    }

    /// Configure the host interrupt pin/reasons (register 0x07). The
    /// reset default 0 = active high, level, push-pull, all sources
    /// enabled.
    pub fn configure_host_interrupt<I: I2cTrait>(
        &self,
        i2c: &mut I,
        cfg: u8,
    ) -> Result<(), Error<I::Error>> {
        self.write_reg(i2c, reg::HOST_INTERRUPT_CTRL, cfg)
    }

    /// Cap the core clock during upload/verify (lower peak current at
    /// the cost of verify time - Chip Control bit 0).
    pub fn disable_turbo_for_upload<I: I2cTrait>(&self, i2c: &mut I) -> Result<(), Error<I::Error>> {
        self.write_reg(i2c, reg::CHIP_CONTROL, chip_control::CPU_TURBO_DISABLE)
    }

    /// Start a firmware upload: sends the Upload to Program RAM
    /// command header carrying the total image length in 32-bit words
    /// (Table 35). Follow with [`Self::upload_chunk`] for the image
    /// bytes, then check verify bits and [`Self::boot_program_ram`].
    pub fn begin_ram_upload<I: I2cTrait>(
        &self,
        i2c: &mut I,
        image_len: usize,
    ) -> Result<(), Error<I::Error>> {
        if image_len % 4 != 0 || image_len / 4 > u16::MAX as usize {
            return Err(Error::Protocol("firmware image length invalid"));
        }
        let words = (image_len / 4) as u16;
        let mut hdr = [0u8; 4];
        hdr[0..2].copy_from_slice(&cmd::UPLOAD_TO_PROGRAM_RAM.to_le_bytes());
        hdr[2..4].copy_from_slice(&words.to_le_bytes());
        self.write_burst(i2c, reg::CHANNEL_CMD, &hdr)
    }

    /// Upload one chunk of the firmware image (raw continuation write
    /// to channel 0 - only the first transaction carries the command
    /// header). Chunk length must be a multiple of 4 and at most
    /// [`UPLOAD_CHUNK`].
    pub fn upload_chunk<I: I2cTrait>(
        &self,
        i2c: &mut I,
        chunk: &[u8],
    ) -> Result<(), Error<I::Error>> {
        if chunk.len() % 4 != 0 || chunk.len() > UPLOAD_CHUNK {
            return Err(Error::Protocol("upload chunk length invalid"));
        }
        self.write_burst(i2c, reg::CHANNEL_CMD, chunk)
    }

    /// Convenience for callers that own the bus exclusively: header
    /// plus all chunks back to back. On a shared bus prefer the staged
    /// path and release the bus between chunks.
    pub fn upload_firmware<I: I2cTrait>(
        &self,
        i2c: &mut I,
        image: &[u8],
    ) -> Result<(), Error<I::Error>> {
        self.begin_ram_upload(i2c, image.len())?;
        for chunk in image.chunks(UPLOAD_CHUNK) {
            self.upload_chunk(i2c, chunk)?;
        }
        Ok(())
    }

    /// Check the verify outcome after an upload completed. Ok(true) =
    /// verified, Ok(false) = still verifying; a set error bit surfaces
    /// the Error Value register code.
    pub fn firmware_verify_done<I: I2cTrait>(&self, i2c: &mut I) -> Result<bool, Error<I::Error>> {
        let bs = self.boot_status(i2c)?;
        if bs & boot_status::FIRMWARE_VERIFY_ERROR != 0 {
            return Err(Error::Chip(self.error_value(i2c)?));
        }
        Ok(bs & boot_status::FIRMWARE_VERIFY_DONE != 0)
    }

    /// Start the verified RAM image (Table 36). Host Interface Ready
    /// clears, then re-asserts once the firmware has booted; the
    /// firmware then puts an Initialized meta event into both sensor
    /// FIFOs which the caller must drain before configuring sensors.
    pub fn boot_program_ram<I: I2cTrait>(&self, i2c: &mut I) -> Result<(), Error<I::Error>> {
        self.send_command(i2c, cmd::BOOT_PROGRAM_RAM, &[])
    }

    // ---- Command / parameter layer -------------------------------

    /// Send a host command packet: `[id][content len][content]`
    /// zero-padded to a multiple of 4 (padding counted in the length
    /// field, Section 4.4.2). Content capacity: [`MAX_PARAM_BYTES`].
    pub fn send_command<I: I2cTrait>(
        &self,
        i2c: &mut I,
        command: u16,
        content: &[u8],
    ) -> Result<(), Error<I::Error>> {
        let padded = (content.len() + 3) & !3;
        if padded > MAX_PARAM_BYTES + 4 {
            return Err(Error::Protocol("command content too long"));
        }
        let mut pkt = [0u8; 4 + MAX_PARAM_BYTES + 4];
        pkt[0..2].copy_from_slice(&command.to_le_bytes());
        pkt[2..4].copy_from_slice(&(padded as u16).to_le_bytes());
        pkt[4..4 + content.len()].copy_from_slice(content);
        self.write_burst(i2c, reg::CHANNEL_CMD, &pkt[..4 + padded])
    }

    /// Enable/reconfigure/disable a virtual sensor (Configure Sensor,
    /// Table 57): rate in Hz as f32 (0.0 disables), latency in ms
    /// (u24; 0 = report immediately). The framework picks the nearest
    /// supported rate at or above the request.
    pub fn configure_sensor<I: I2cTrait>(
        &self,
        i2c: &mut I,
        sensor_id: u8,
        rate_hz: f32,
        latency_ms: u32,
    ) -> Result<(), Error<I::Error>> {
        let r = rate_hz.to_le_bytes();
        let l = latency_ms.to_le_bytes();
        let content = [sensor_id, r[0], r[1], r[2], r[3], l[0], l[1], l[2]];
        self.send_command(i2c, cmd::CONFIGURE_SENSOR, &content)
    }

    /// Change a virtual sensor's dynamic range in SI units (g / dps /
    /// uT; 0 = default). Affects the scale of its 3D vector samples.
    pub fn change_dynamic_range<I: I2cTrait>(
        &self,
        i2c: &mut I,
        sensor_id: u8,
        range: u16,
    ) -> Result<(), Error<I::Error>> {
        let r = range.to_le_bytes();
        self.send_command(i2c, cmd::CHANGE_DYNAMIC_RANGE, &[sensor_id, r[0], r[1], 0])
    }

    /// FIFO Flush (Table 47) - transfer or discard FIFO contents, see
    /// the `flush` constants.
    pub fn flush_fifo<I: I2cTrait>(&self, i2c: &mut I, which: u8) -> Result<(), Error<I::Error>> {
        self.send_command(i2c, cmd::FIFO_FLUSH, &[which, 0, 0, 0])
    }

    /// Inform the hub of the host's power state (Host Interface
    /// Control bit 4). Suspended: only wake-up sensors assert the host
    /// interrupt. Recommended order per Section 16.3: flush + drain
    /// the FIFOs, then suspend.
    pub fn set_ap_suspended<I: I2cTrait>(
        &self,
        i2c: &mut I,
        suspended: bool,
    ) -> Result<(), Error<I::Error>> {
        let cur = self.read_reg(i2c, reg::HOST_INTERFACE_CTRL)?;
        let new = if suspended {
            cur | hif_control::AP_SUSPENDED
        } else {
            cur & !hif_control::AP_SUSPENDED
        };
        self.write_reg(i2c, reg::HOST_INTERFACE_CTRL, new)
    }

    /// Request a physical sensor self-test (the sensor must be
    /// inactive). Poll [`Self::try_read_status_packet`] for
    /// [`status::SELF_TEST_RESULTS`], then decode with
    /// [`Self::decode_test_result`]. A rejected request (sensor
    /// active) sets the Error Value register instead of answering.
    pub fn request_self_test<I: I2cTrait>(
        &self,
        i2c: &mut I,
        phys_sensor_id: u8,
    ) -> Result<(), Error<I::Error>> {
        self.send_command(i2c, cmd::REQUEST_SELF_TEST, &[phys_sensor_id, 0, 0, 0])
    }

    /// Request fast offset compensation for a physical sensor; answer
    /// arrives as [`status::FOC_RESULTS`].
    pub fn request_foc<I: I2cTrait>(
        &self,
        i2c: &mut I,
        phys_sensor_id: u8,
    ) -> Result<(), Error<I::Error>> {
        self.send_command(i2c, cmd::REQUEST_FOC, &[phys_sensor_id, 0, 0, 0])
    }

    /// Decode a self-test/FOC status packet payload (Tables 54/56).
    pub fn decode_test_result(payload: &[u8]) -> Option<TestResult> {
        if payload.len() < 8 {
            return None;
        }
        Some(TestResult {
            sensor_id: payload[0],
            status: payload[1],
            offsets: [
                i16::from_le_bytes([payload[2], payload[3]]),
                i16::from_le_bytes([payload[4], payload[5]]),
                i16::from_le_bytes([payload[6], payload[7]]),
            ],
        })
    }

    /// Write a parameter (Section 13.3.1): command id = parameter id,
    /// payload = the parameter's data structure.
    pub fn param_write<I: I2cTrait>(
        &self,
        i2c: &mut I,
        param: u16,
        data: &[u8],
    ) -> Result<(), Error<I::Error>> {
        self.send_command(i2c, param & !cmd::PARAM_READ_FLAG, data)
    }

    /// Read a parameter: issues `0x1000 | param`, waits for the
    /// synchronous status packet on channel 3 (bounded poll), copies
    /// its payload into `out` and returns the byte count. Requires the
    /// status channel in synchronous mode (the reset default).
    pub fn param_read<I: I2cTrait>(
        &self,
        i2c: &mut I,
        param: u16,
        out: &mut [u8],
    ) -> Result<usize, Error<I::Error>> {
        self.send_command(i2c, cmd::PARAM_READ_FLAG | (param & 0x0FFF), &[])?;
        for _ in 0..STATUS_SPINS {
            if let Some((code, n)) = self.try_read_status_packet(i2c, out)? {
                // The response packet's code carries the parameter
                // number (the tables show it with and without the
                // 0x1000 read flag - accept both).
                if code & 0x0FFF == param & 0x0FFF {
                    return Ok(n);
                }
                if code == status::COMMAND_ERROR {
                    return Err(Error::Command {
                        command: u16::from_le_bytes([
                            *out.first().unwrap_or(&0),
                            *out.get(1).unwrap_or(&0),
                        ]),
                        error: *out.get(2).unwrap_or(&0),
                    });
                }
                // Unrelated packet (e.g. a stale response) - keep
                // polling for ours.
            }
        }
        Err(Error::Protocol("parameter read timed out"))
    }

    /// Non-blocking check of the synchronous status channel. In
    /// synchronous mode the channel does NOT use the FIFO framing -
    /// there is no 2-byte transfer-length prefix; a read delivers a
    /// status packet directly: `[status code u16][length u16]
    /// [contents]` (Section 14.2.1 / Table 9; Bosch's reference host
    /// library reads it exactly this way). One packet is read per
    /// call and returned as (status code, payload bytes copied into
    /// `out`); the payload is always fully drained from the chip
    /// even when `out` is smaller. If more packets are pending, the
    /// Status interrupt bit stays set and the next call reads the
    /// next one. Callers filter by code themselves.
    pub fn try_read_status_packet<I: I2cTrait>(
        &self,
        i2c: &mut I,
        out: &mut [u8],
    ) -> Result<Option<(u16, usize)>, Error<I::Error>> {
        if self.interrupt_status(i2c)? & int_status::STATUS == 0 {
            return Ok(None);
        }
        let mut phdr = [0u8; 4];
        self.read_regs(i2c, reg::CHANNEL_STATUS_FIFO, &mut phdr)?;
        let code = u16::from_le_bytes([phdr[0], phdr[1]]);
        let plen = u16::from_le_bytes([phdr[2], phdr[3]]) as usize;
        if code == 0 && plen == 0 {
            // Spurious status bit with an empty channel.
            return Ok(None);
        }
        let copy = plen.min(out.len());
        if copy > 0 {
            self.read_regs(i2c, reg::CHANNEL_STATUS_FIFO, &mut out[..copy])?;
        }
        // Drain any tail past the caller's buffer.
        let mut remaining = plen - copy;
        let mut scratch = [0u8; 16];
        while remaining > 0 {
            let n = remaining.min(scratch.len());
            self.read_regs(i2c, reg::CHANNEL_STATUS_FIFO, &mut scratch[..n])?;
            remaining -= n;
        }
        Ok(Some((code, copy)))
    }

    // ---- Sensor discovery ----------------------------------------

    /// 256-bit bitmap of virtual sensors the loaded firmware provides
    /// (parameter 0x011F); test with [`Self::sensor_present`].
    pub fn virt_sensors_present<I: I2cTrait>(
        &self,
        i2c: &mut I,
    ) -> Result<[u8; 32], Error<I::Error>> {
        let mut map = [0u8; 32];
        let n = self.param_read(i2c, param::VIRT_SENSORS_PRESENT, &mut map)?;
        if n < 32 {
            return Err(Error::Protocol("virtual sensor bitmap short"));
        }
        Ok(map)
    }

    pub fn sensor_present(bitmap: &[u8; 32], sensor_id: u8) -> bool {
        bitmap[(sensor_id / 8) as usize] & (1 << (sensor_id % 8)) != 0
    }

    /// 64-bit bitmap of present physical sensors (parameter 0x0120).
    pub fn phys_sensors_present<I: I2cTrait>(
        &self,
        i2c: &mut I,
    ) -> Result<[u8; 8], Error<I::Error>> {
        let mut map = [0u8; 8];
        let n = self.param_read(i2c, param::PHYS_SENSORS_PRESENT, &mut map)?;
        if n < 8 {
            return Err(Error::Protocol("physical sensor bitmap short"));
        }
        Ok(map)
    }

    /// Physical Sensor Information structure (Table 69, 20 bytes) for
    /// one physical sensor ID.
    pub fn phys_sensor_info<I: I2cTrait>(
        &self,
        i2c: &mut I,
        phys_sensor_id: u8,
        out: &mut [u8; 20],
    ) -> Result<(), Error<I::Error>> {
        let p = param::PHYS_SENSOR_INFO_BASE + phys_sensor_id as u16;
        let n = self.param_read(i2c, p, out)?;
        if n < 20 {
            return Err(Error::Protocol("physical sensor info short"));
        }
        Ok(())
    }

    /// Pack a 3x3 axis-remap matrix (row-major c0..c8, elements -1,
    /// 0 or 1) into the 5-byte nibble format of Table 70: low nibble
    /// holds the even-index element, -1 encodes as 0xF. Use with
    /// [`Bhi260::set_orientation_matrix`]; the board supplies its
    /// mounting.
    pub const fn pack_orientation_matrix(c: [i8; 9]) -> [u8; 5] {
        const fn nib(v: i8) -> u8 {
            (v as u8) & 0x0F
        }
        [
            nib(c[1]) << 4 | nib(c[0]),
            nib(c[3]) << 4 | nib(c[2]),
            nib(c[5]) << 4 | nib(c[4]),
            nib(c[7]) << 4 | nib(c[6]),
            nib(c[8]),
        ]
    }

    /// Write a physical sensor's 5-byte packed orientation matrix
    /// (Table 70; nibble-packed C0..C8 as read back in info bytes
    /// 0x12-0x16) to remap axes to the device coordinate system.
    pub fn set_orientation_matrix<I: I2cTrait>(
        &self,
        i2c: &mut I,
        phys_sensor_id: u8,
        matrix: [u8; 5],
    ) -> Result<(), Error<I::Error>> {
        let p = param::PHYS_SENSOR_INFO_BASE + phys_sensor_id as u16;
        let content = [matrix[0], matrix[1], matrix[2], matrix[3], matrix[4], 0, 0, 0];
        self.param_write(i2c, p, &content)
    }

    /// Meta event enable bitmap write (parameters 0x0101 non-wake /
    /// 0x0102 wake, Table 63: 32 two-bit sections, MSbit of each pair
    /// = event enable, LSbit = interrupt enable).
    pub fn set_meta_event_control<I: I2cTrait>(
        &self,
        i2c: &mut I,
        wake_fifo: bool,
        bitmap: [u8; 8],
    ) -> Result<(), Error<I::Error>> {
        let p = if wake_fifo {
            param::META_EVENT_CTRL_WAKE
        } else {
            param::META_EVENT_CTRL_NONWAKE
        };
        self.param_write(i2c, p, &bitmap)
    }

    /// Set the FIFO watermark levels in bytes (0 = disabled) -
    /// parameter 0x0103, Table 64.
    pub fn set_fifo_watermarks<I: I2cTrait>(
        &self,
        i2c: &mut I,
        wake: u32,
        nonwake: u32,
    ) -> Result<(), Error<I::Error>> {
        let w = wake.to_le_bytes();
        let n = nonwake.to_le_bytes();
        let content = [
            w[0], w[1], w[2], w[3], 0, 0, 0, 0, // wake wm, size ro
            n[0], n[1], n[2], n[3], 0, 0, 0, 0, // non-wake wm, size ro
        ];
        self.param_write(i2c, param::FIFO_CONTROL, &content)
    }

    // ---- Sensor data ---------------------------------------------

    /// Pending transfer length of a FIFO channel without consuming
    /// events: reads the 2-byte FIFO descriptor length. 0 = no data.
    /// NOTE: this starts a transfer - follow up with
    /// [`Self::read_fifo_payload`] to drain exactly that many bytes,
    /// or the channel stays mid-transfer and blocks new interrupts.
    pub fn fifo_transfer_len<I: I2cTrait>(
        &self,
        i2c: &mut I,
        channel: u8,
    ) -> Result<usize, Error<I::Error>> {
        let mut hdr = [0u8; 2];
        self.read_regs(i2c, channel, &mut hdr)?;
        Ok(u16::from_le_bytes(hdr) as usize)
    }

    /// Continue a FIFO transfer: read `len` bytes into `buf` (in one
    /// transaction - size the read to the caller's chunking). Splitting
    /// a transfer across several calls is allowed by the protocol.
    pub fn read_fifo_payload<I: I2cTrait>(
        &self,
        i2c: &mut I,
        channel: u8,
        buf: &mut [u8],
    ) -> Result<(), Error<I::Error>> {
        self.read_regs(i2c, channel, buf)
    }

    /// Read one complete pending transfer of a sensor FIFO into `buf`,
    /// returning the byte count to parse with [`fifo::Parser`]. If the
    /// transfer exceeds `buf`, the remainder is drained and discarded
    /// (the FIFO must be emptied for the interrupt line to release; a
    /// large enough `buf` is the caller's responsibility - FIFO sizes
    /// are readable via parameter 0x0103).
    pub fn read_fifo<I: I2cTrait>(
        &self,
        i2c: &mut I,
        channel: u8,
        buf: &mut [u8],
    ) -> Result<usize, Error<I::Error>> {
        let total = self.fifo_transfer_len(i2c, channel)?;
        if total == 0 {
            return Ok(0);
        }
        let used = total.min(buf.len());
        self.read_fifo_payload(i2c, channel, &mut buf[..used])?;
        let mut remaining = total - used;
        if remaining > 0 {
            log::warn!(
                "BHI260: FIFO transfer {} B exceeds buffer {} B - discarding tail",
                total,
                buf.len(),
            );
            let mut scratch = [0u8; 32];
            while remaining > 0 {
                let n = remaining.min(scratch.len());
                self.read_fifo_payload(i2c, channel, &mut scratch[..n])?;
                remaining -= n;
            }
        }
        Ok(used)
    }
}
