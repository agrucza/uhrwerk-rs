//! CO5300 QSPI AMOLED display driver - HAL-agnostic, framebuffer-based.
//!
//! ## Architecture
//!
//! The driver holds a mutable reference to a framebuffer slice provided by the
//! caller. All drawing operations (fill, blit, DrawTarget) are synchronous RAM
//! writes into the framebuffer - no bus I/O, no waiting. When the scene is
//! ready, call [`flush_tile`] which pushes the framebuffer over the QSPI bus
//! (using DMA or whatever transport the `QspiWrite` implementor provides) to
//! panel rows `[tile_y, tile_y + fb_rows)`.
//!
//! The framebuffer can be sized for the full panel ([`FB_BYTES`]) or any
//! whole-row partial window ([`fb_bytes_for_rows`]). Callers always pass
//! panel-absolute coordinates to draw calls; the driver subtracts the
//! current `tile_y` (set via [`set_tile_y`]) and clips to the FB. A renderer
//! that loops `tile_y` across the panel in steps of `fb_rows` covers the
//! whole screen and works identically on a board with PSRAM (full-panel FB)
//! and one without (small partial FB).
//!
//! On ESP32-S3R8 the framebuffer is typically placed in PSRAM via
//! `esp_alloc::psram_allocator!`. GDMA cannot DMA directly from PSRAM, so
//! the `QspiWrite` implementor is responsible for bouncing the data through
//! an internal-SRAM buffer before handing it to the DMA engine (see
//! `EspQspi` in firmware-hal).
//!
//! ## Wire format (CO5300 datasheet section 5.2.2)
//!
//! - Control: opcode=0x02, addr=(CMD<<8), data on 1 wire
//! - Pixels:  opcode=0x32, addr=(CMD<<8), data on 4 wires
//!
//! Do NOT send 0x38 (Enter Quad SPI) - it breaks the mixed opcode approach.

use embedded_hal::digital::OutputPin;
use embedded_graphics_core::{
    draw_target::DrawTarget,
    geometry::{OriginDimensions, Size},
    pixelcolor::{Rgb565, RgbColor},
    primitives::Rectangle,
    Pixel,
};

use super::blend::{self, BlendTarget};
use super::qspi::QspiWrite;

/// Display resolution (panel H0198S005AMT005_V0_195).
pub const WIDTH:  u16 = 410;
pub const HEIGHT: u16 = 502;

/// Framebuffer size in bytes for a full-panel buffer (RGB565, 2 bytes per pixel).
///
/// Callers that don't have room for a full-panel buffer (e.g. ESP32-C6, no PSRAM)
/// can pass a smaller buffer to [`CO5300::new`] as long as its length is a whole
/// number of full panel rows (`WIDTH * 2` bytes each). See [`fb_bytes_for_rows`].
pub const FB_BYTES: usize = WIDTH as usize * HEIGHT as usize * 2;

/// Compute the framebuffer byte count for a partial-height buffer covering
/// the top `rows` rows of the panel. Use this to size a partial FB at the
/// call site without hand-multiplying.
pub const fn fb_bytes_for_rows(rows: u16) -> usize {
    WIDTH as usize * rows as usize * 2
}

/// Column / row offsets applied by this panel inside the CO5300 frame buffer.
const COL_OFFSET: u16 = 22;
const ROW_OFFSET: u16 = 0;

/// MIPI DCS command bytes.
pub mod cmd {
    pub const SLPIN:      u8 = 0x10;
    pub const SLPOUT:     u8 = 0x11;
    pub const DISPOFF:    u8 = 0x28;
    pub const DISPON:     u8 = 0x29;
    pub const CASET:      u8 = 0x2A;
    pub const RASET:      u8 = 0x2B;
    pub const RAMWR:      u8 = 0x2C;
    pub const RAMWRC:     u8 = 0x3C;
    pub const MADCTL:     u8 = 0x36;
    pub const PIXFMT:     u8 = 0x3A;
    pub const BRIGHTNESS: u8 = 0x51;
}

/// Named RGB565 colour constants.
pub mod color {
    pub const BLACK: u16 = 0x0000;
    pub const WHITE: u16 = 0xFFFF;
    pub const RED:   u16 = 0xF800;
    pub const GREEN: u16 = 0x07E0;
    pub const BLUE:  u16 = 0x001F;
}

/// Display orientation / mirroring sent as the MADCTL parameter (0x36).
///
/// The CO5300 MADCTL only implements MY (bit7), MX (bit6), and RGB (bit3).
/// There is no MV bit - true 90/270 degree rotation requires software transposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    Normal  = 0x00,   // MY=0 MX=0
    MirrorX = 0x40,   // MX=1
    MirrorY = 0x80,   // MY=1
    Deg180  = 0xC0,   // MY=1 MX=1
}

// ---- Driver struct ----------------------------------------------------------

/// CO5300 driver. Generic over the QSPI bus `B` and reset pin `RST`.
///
/// `'fb` is the lifetime of the caller-provided framebuffer slice.
///
/// ## Tile-mode rendering
///
/// The framebuffer can cover the full panel ([`FB_BYTES`]) or only `N` rows
/// of it ([`fb_bytes_for_rows`]). Callers always pass panel-absolute
/// coordinates to draw calls; the driver subtracts [`tile_y`] (the panel-y
/// position currently represented by FB row 0) and clips into `[0, fb_rows)`.
/// [`flush_tile`] then pushes the FB to panel rows `[tile_y, tile_y + fb_rows)`.
/// Loop the caller over `tile_y` in steps of `fb_rows` to cover the whole
/// panel, hash the FB per-tile for dirty detection, and you have a
/// partial-FB renderer that works identically on a board with PSRAM and one
/// without.
///
/// A full-panel FB (`fb_rows == HEIGHT`) with `tile_y == 0` is the
/// degenerate case: translation is a no-op and a single `flush_tile` pushes
/// the whole panel.
pub struct CO5300<'fb, B, RST> {
    bus:         B,
    reset:       RST,
    framebuffer: &'fb mut [u8],
    /// Number of full panel rows that fit in `framebuffer`. Always
    /// `<= HEIGHT` and `framebuffer.len() == fb_rows * WIDTH * 2`.
    fb_rows:     u16,
    /// Panel-y of FB row 0. Draw calls subtract this from their y
    /// coordinate so callers stay in panel-absolute space. Always even
    /// (the panel requires 2-row-aligned address windows).
    tile_y:      u16,
}

// ---- Init / power / config - all async (bus I/O) ----------------------------

impl<'fb, B: QspiWrite, RST: OutputPin> CO5300<'fb, B, RST> {
    /// Create a new driver.
    ///
    /// `framebuffer` length must be a whole number of panel rows
    /// (`WIDTH * 2` bytes each) and at most [`FB_BYTES`] (the full panel).
    /// A full-panel buffer should be placed in PSRAM on ESP32-S3 via
    /// `#[link_section = ".ext_ram.bss"]`; a partial buffer fits in
    /// internal SRAM (use [`fb_bytes_for_rows`] to size it).
    pub fn new(bus: B, reset: RST, framebuffer: &'fb mut [u8]) -> Self {
        let row_bytes = WIDTH as usize * 2;
        assert!(
            framebuffer.len().is_multiple_of(row_bytes),
            "framebuffer must be a whole number of rows (multiple of WIDTH * 2 bytes)",
        );
        assert!(
            framebuffer.len() <= FB_BYTES,
            "framebuffer must be at most FB_BYTES bytes (the full panel)",
        );
        let fb_rows = (framebuffer.len() / row_bytes) as u16;
        Self { bus, reset, framebuffer, fb_rows, tile_y: 0 }
    }

    /// Number of panel rows covered by this driver's framebuffer.
    ///
    /// `fb_rows == HEIGHT` for a full-panel buffer; smaller for a partial
    /// buffer. Use this when computing what region of the FB to fill /
    /// flush from caller code.
    pub fn fb_rows(&self) -> u16 {
        self.fb_rows
    }

    /// Move the FB's "window" to a new panel-y position. Subsequent draw
    /// calls keep using panel-absolute coordinates; the driver translates
    /// by `panel_y` before clipping. Pair with [`flush_tile`] which pushes
    /// to panel rows `[panel_y, panel_y + fb_rows)`.
    ///
    /// Must be even (the panel requires 2-row-aligned address windows; all
    /// FB-y to panel-y mapping math assumes the FB starts on an even row).
    pub fn set_tile_y(&mut self, panel_y: u16) {
        debug_assert!(panel_y & 1 == 0, "tile_y must be even (panel needs 2-row alignment)");
        self.tile_y = panel_y;
    }

    /// Current panel-y of FB row 0. See [`set_tile_y`].
    pub fn tile_y(&self) -> u16 {
        self.tile_y
    }

    // ---- Reset helpers (caller provides delays via Timer::after) --------

    pub fn reset_high(&mut self) { self.reset.set_high().ok(); }
    pub fn reset_low(&mut self)  { self.reset.set_low().ok();  }

    // ---- Init sequence --------------------------------------------------

    /// Send configuration commands. Call order:
    ///   1. hardware reset (RST low then high, 120 ms settle)
    ///   2. `init()`
    ///   3. `wake()` + 120 ms
    ///   4. `display_on()` + 70 ms
    /// `brightness` is the initial 0..=255 backlight register value -
    /// the caller passes the persisted setting so the first lit frame
    /// already matches it (no hardcoded default to reconcile later).
    pub async fn init(&mut self, brightness: u8) {
        self.write_cmd(0xC4,            &[0x80]).await; // enable QSPI opcode 0x32
        self.write_cmd(cmd::PIXFMT,     &[0x55]).await; // RGB565 (16 bpp)
        self.write_cmd(0x35,            &[0x00]).await; // tearing effect on, mode 0
        self.write_cmd(0x53,            &[0x20]).await; // BC_EN=1
        self.write_cmd(cmd::BRIGHTNESS, &[brightness]).await;
        self.write_cmd(0x63,            &[0xFF]).await; // HBM max
    }

    // ---- Power / backlight ----------------------------------------------

    /// Enter sleep mode.
    ///
    /// Timing (datasheet section 5.6.2 / 7.5.11):
    ///   - Wait >= 5 ms after this command before any next command.
    ///   - Wait >= 120 ms after SLPOUT before issuing SLPIN.
    pub async fn sleep(&mut self) {
        self.write_cmd(cmd::SLPIN, &[]).await;
    }

    /// Exit sleep mode.
    ///
    /// Timing (datasheet section 5.6.1 / 7.5.12):
    ///   - Wait >= 5 ms after this command before any next command.
    ///   - Wait >= 120 ms after SLPIN before issuing SLPOUT.
    ///   - Call [`flush_tile`] only after the 120 ms settle has elapsed.
    pub async fn wake(&mut self) {
        self.write_cmd(cmd::SLPOUT, &[]).await;
    }

    /// Turn pixels on (display visible).
    pub async fn display_on(&mut self) {
        self.write_cmd(cmd::DISPON, &[]).await;
    }

    /// Turn pixels off (panel blanked, backlight on).
    pub async fn display_off(&mut self) {
        self.write_cmd(cmd::DISPOFF, &[]).await;
    }

    /// Set display brightness (0 = off, 255 = maximum).
    pub async fn set_brightness(&mut self, brightness: u8) {
        self.write_cmd(cmd::BRIGHTNESS, &[brightness]).await;
    }

    /// Set display orientation. See [`Rotation`] for available modes.
    pub async fn set_rotation(&mut self, rotation: Rotation) {
        self.write_cmd(cmd::MADCTL, &[rotation as u8]).await;
    }

    /// Send one raw MIPI DCS command with optional parameters.
    pub async fn write_cmd(&mut self, command: u8, params: &[u8]) {
        // Many call sites (init sequence, SLPOUT/DISPON, brightness)
        // care that the command actually landed - silent failures here
        // surface as "panel stuck in sleep" or "wrong color format" with
        // no log trail. Warn but don't propagate, since the existing
        // call sites are infallible (`.await` without `.ok()`).
        if self.bus.write_cmd(command, params).await.is_err() {
            log::warn!("CO5300: cmd 0x{:02X} write failed", command);
        }
    }

    // ---- Flush ----------------------------------------------------------

    /// Send a horizontal band of the framebuffer to the display.
    ///
    /// Rows `[y0, y1)` are FB-local and panel-local at the same time:
    /// this method ignores `tile_y` and pushes FB row `y` to panel row
    /// `y` directly. Useful for callers that are managing the full
    /// panel from a full FB without the tile abstraction; the normal
    /// per-tile render path uses [`flush_tile`] instead.
    ///
    /// The CO5300 (like its QSPI AMOLED siblings SH8601 / GC9B71) packs
    /// pixels in 2x1 units internally and requires the row window to be
    /// aligned to 2-row pairs: the inclusive start row must be even and
    /// the inclusive end row must be odd. If a misaligned window is sent
    /// the panel rounds to the nearest pair on its side while our pixel
    /// data stays at the original offset, shifting every row by one and
    /// leaving stale pixels at the edges. To avoid that, `y0` is rounded
    /// down to the nearest even row and `y1` is rounded up, so the band
    /// we actually push always covers the caller's requested range.
    #[allow(dead_code)]
    pub async fn flush_rows(&mut self, y0: u16, y1: u16) {
        if y1 <= y0 || y0 >= self.fb_rows { return; }
        let y0 = y0 & !1;
        let y1 = ((y1 + 1) & !1).min(self.fb_rows);
        let h  = y1 - y0;
        self.set_addr_window(0, y0, WIDTH, h).await;
        let stride = WIDTH as usize * 2;
        let start  = y0 as usize * stride;
        let end    = y1 as usize * stride;
        self.bus.write_pixels(true, &self.framebuffer[start..end]).await.ok();
    }

    /// Wait for any DMA started by a previous [`flush_tile`] call to
    /// finish. Forwarded to [`QspiWrite::flush_pending`]; on bus
    /// implementations without pipelined DMA this is a no-op. Call at
    /// the end of a render frame so the bus is idle before other code
    /// paths (display power transitions, brightness writes) touch it.
    pub async fn flush_pending(&mut self) {
        if self.bus.flush_pending().await.is_err() {
            log::warn!("CO5300: flush_pending bus error");
        }
    }

    /// Push the framebuffer to panel rows `[tile_y, tile_y + fb_rows)`.
    ///
    /// For the short final tile that overlaps the bottom of the panel
    /// (e.g. tile_y=500 + fb_rows=50 on a 502-row panel) only the rows
    /// that fit on the panel are pushed; the rest of the FB is ignored.
    /// `set_tile_y` enforces 2-row alignment on `tile_y`; together with
    /// the panel's even `HEIGHT` this keeps every pushed window
    /// 2-row-aligned without further rounding.
    pub async fn flush_tile(&mut self) {
        if self.tile_y >= HEIGHT { return; }
        let panel_rows = (HEIGHT - self.tile_y).min(self.fb_rows);
        self.set_addr_window(0, self.tile_y, WIDTH, panel_rows).await;
        let stride = WIDTH as usize * 2;
        let end    = panel_rows as usize * stride;
        // Pixel-write errors leave the panel's GRAM partially updated -
        // the visible failure mode is a torn or solid-color band where
        // the dropped bytes should have gone. Surface as a warning so
        // it's not silent next time MAX_DMA_SIZE or similar trips.
        if self.bus.write_pixels(true, &self.framebuffer[..end]).await.is_err() {
            log::warn!(
                "CO5300: flush_tile write_pixels failed (tile_y={}, bytes={})",
                self.tile_y, end,
            );
        }
    }

    /// Read-only access to the framebuffer. The main loop uses this to
    /// hash rows for dirty-tracking; nothing else should need it.
    pub fn framebuffer(&self) -> &[u8] {
        self.framebuffer
    }

    // ---- Private helpers ------------------------------------------------

    async fn set_addr_window(&mut self, x: u16, y: u16, w: u16, h: u16) {
        let x0 = x + COL_OFFSET;
        let x1 = x + w - 1 + COL_OFFSET;
        let y0 = y + ROW_OFFSET;
        let y1 = y + h - 1 + ROW_OFFSET;
        // CASET/RASET failures mean the subsequent RAMWR writes pixels
        // to the wrong panel region. Warn rather than swallow; the
        // visible artifact (shifted / wrapped pixels) is much harder to
        // diagnose without a log trail.
        if self.bus.write_cmd(cmd::CASET, &[
            (x0 >> 8) as u8, (x0 & 0xFF) as u8,
            (x1 >> 8) as u8, (x1 & 0xFF) as u8,
        ]).await.is_err() {
            log::warn!("CO5300: CASET write failed (x={}..{})", x0, x1);
        }
        if self.bus.write_cmd(cmd::RASET, &[
            (y0 >> 8) as u8, (y0 & 0xFF) as u8,
            (y1 >> 8) as u8, (y1 & 0xFF) as u8,
        ]).await.is_err() {
            log::warn!("CO5300: RASET write failed (y={}..{})", y0, y1);
        }
    }
}

// ---- Sync drawing API (framebuffer writes only) -----------------------------

impl<'fb, B, RST> CO5300<'fb, B, RST> {
    /// Fill a panel-absolute rectangle with a single RGB565 colour.
    /// Y is translated by `tile_y` and clipped to `[0, fb_rows)`.
    pub fn fill_solid(&mut self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        let ty = self.tile_y as i32;
        let ay0 = y as i32 - ty;
        let ay1 = ay0 + h as i32;
        let ax0 = x as i32;
        let ax1 = ax0 + w as i32;
        if ax1 <= 0 || ay1 <= 0 || ax0 >= WIDTH as i32 || ay0 >= self.fb_rows as i32 {
            return;
        }
        let cy0 = ay0.max(0) as u16;
        let cy1 = ay1.min(self.fb_rows as i32) as u16;
        let cx0 = ax0.max(0) as u16;
        let cx1 = ax1.min(WIDTH as i32) as u16;

        let hi = (color >> 8) as u8;
        let lo = (color & 0xFF) as u8;
        // Build one row of the fill pattern, then memcpy it into each
        // FB row. Sequential wide copies instead of per-byte stores:
        // on a PSRAM-resident FB every individual store pays external-
        // memory latency (plus a cache-line fill on first touch), so
        // byte-wise filling is the slowest possible access pattern.
        let w_bytes = (cx1 - cx0) as usize * 2;
        let mut row_buf = [0u8; WIDTH as usize * 2];
        for px in row_buf[..w_bytes].chunks_exact_mut(2) {
            px[0] = hi;
            px[1] = lo;
        }
        for row in cy0..cy1 {
            let start = (row as usize * WIDTH as usize + cx0 as usize) * 2;
            self.framebuffer[start..start + w_bytes]
                .copy_from_slice(&row_buf[..w_bytes]);
        }
    }

    /// Blit a rectangle of RGB565 pixels from a slice into a panel-absolute
    /// rectangle. Y is translated by `tile_y` and the rect is clipped to
    /// the FB; source pixels for clipped-away cells are skipped.
    ///
    /// `pixels` must contain exactly `w * h` values.
    pub fn draw_raw(&mut self, x: u16, y: u16, w: u16, h: u16, pixels: &[u16]) {
        let ty = self.tile_y as i32;
        let ay0 = y as i32 - ty;
        let ay1 = ay0 + h as i32;
        let ax0 = x as i32;
        let ax1 = ax0 + w as i32;
        if ax1 <= 0 || ay1 <= 0 || ax0 >= WIDTH as i32 || ay0 >= self.fb_rows as i32 {
            return;
        }
        let cy0 = ay0.max(0);
        let cy1 = ay1.min(self.fb_rows as i32);
        let cx0 = ax0.max(0);
        let cx1 = ax1.min(WIDTH as i32);

        let skip_left  = (cx0 - ax0) as usize;
        let skip_right = (ax1 - cx1) as usize;
        let skip_top   = (cy0 - ay0) as usize;
        let src_row_w  = w as usize;
        let mut src = skip_top * src_row_w;

        for row in cy0..cy1 {
            src += skip_left;
            let row_base = row as usize * WIDTH as usize;
            for col in cx0..cx1 {
                if src >= pixels.len() { return; }
                let px  = pixels[src];
                let idx = (row_base + col as usize) * 2;
                self.framebuffer[idx]     = (px >> 8) as u8;
                self.framebuffer[idx + 1] = (px & 0xFF) as u8;
                src += 1;
            }
            src += skip_right;
        }
    }

    /// Write a single panel-absolute pixel into the framebuffer.
    pub fn draw_pixel(&mut self, x: u16, y: u16, color: u16) {
        let y_local = y as i32 - self.tile_y as i32;
        if x < WIDTH && y_local >= 0 && y_local < self.fb_rows as i32 {
            let idx = (y_local as usize * WIDTH as usize + x as usize) * 2;
            self.framebuffer[idx]     = (color >> 8) as u8;
            self.framebuffer[idx + 1] = (color & 0xFF) as u8;
        }
    }

    /// Clip a panel-absolute rectangle against the FB window. Returns
    /// `(cx0, cx1, cy0, cy1)` in *panel* coordinates, or `None` when
    /// the rect misses the FB entirely. Shared by the blend fills.
    fn clip_rect(&self, x: i32, y: i32, w: i32, h: i32) -> Option<(i32, i32, i32, i32)> {
        let ty = self.tile_y as i32;
        let cx0 = x.max(0);
        let cx1 = (x + w).min(WIDTH as i32);
        let cy0 = y.max(ty);
        let cy1 = (y + h).min(ty + self.fb_rows as i32);
        if cx0 >= cx1 || cy0 >= cy1 { None } else { Some((cx0, cx1, cy0, cy1)) }
    }

    /// Read the packed RGB565 value at FB byte offset `idx`.
    fn fb_read(&self, idx: usize) -> u16 {
        ((self.framebuffer[idx] as u16) << 8) | self.framebuffer[idx + 1] as u16
    }

    /// Write a packed RGB565 value at FB byte offset `idx`.
    fn fb_write(&mut self, idx: usize, px: u16) {
        self.framebuffer[idx]     = (px >> 8) as u8;
        self.framebuffer[idx + 1] = (px & 0xFF) as u8;
    }
}

// ---- BlendTarget (read-modify-write drawing) --------------------------------

impl<'fb, B, RST> BlendTarget for CO5300<'fb, B, RST> {
    fn blend_pixel(&mut self, x: i32, y: i32, color: Rgb565, alpha: u8) {
        if alpha == 0 { return; }
        let y_local = y - self.tile_y as i32;
        if x < 0 || x >= WIDTH as i32 || y_local < 0 || y_local >= self.fb_rows as i32 {
            return;
        }
        let src = blend::pack565(color);
        let idx = (y_local as usize * WIDTH as usize + x as usize) * 2;
        let px = if alpha == 255 { src } else { blend::blend565(self.fb_read(idx), src, alpha) };
        self.fb_write(idx, px);
    }

    fn fill_vgradient(&mut self, x: i32, y: i32, w: i32, h: i32, top: Rgb565, bottom: Rgb565) {
        let Some((cx0, cx1, cy0, cy1)) = self.clip_rect(x, y, w, h) else { return };
        let (tr, tg, tb) = blend::expand565(blend::pack565(top));
        let (br, bg, bb) = blend::expand565(blend::pack565(bottom));
        let den = (h - 1).max(1) as u32;
        let ty = self.tile_y as i32;
        for py in cy0..cy1 {
            // Ramp position from the *panel-absolute* rect top, so the
            // gradient is seamless across tile boundaries.
            let num = (py - y) as u32;
            let r8 = blend::lerp8(tr, br, num, den);
            let g8 = blend::lerp8(tg, bg, num, den);
            let b8 = blend::lerp8(tb, bb, num, den);
            // Bayer thresholds repeat every 4 columns: precompute this
            // row's 4 candidate pixels, then tile them across the span.
            let brow = &blend::BAYER4[(py & 3) as usize];
            let pats: [u16; 4] = core::array::from_fn(|i| {
                blend::dither565(r8, g8, b8, brow[i])
            });
            let row_base = (py - ty) as usize * WIDTH as usize;
            for px in cx0..cx1 {
                let p = pats[(px & 3) as usize];
                self.fb_write((row_base + px as usize) * 2, p);
            }
        }
    }

    fn fill_blend(&mut self, x: i32, y: i32, w: i32, h: i32, color: Rgb565, alpha: u8) {
        if alpha == 0 { return; }
        if alpha == 255 {
            // Opaque degenerates to the plain fill (row-memcpy fast path).
            if let Some((cx0, cx1, cy0, cy1)) = self.clip_rect(x, y, w, h) {
                self.fill_solid(
                    cx0 as u16, cy0 as u16,
                    (cx1 - cx0) as u16, (cy1 - cy0) as u16,
                    blend::pack565(color),
                );
            }
            return;
        }
        let Some((cx0, cx1, cy0, cy1)) = self.clip_rect(x, y, w, h) else { return };
        let src = blend::pack565(color);
        let ty = self.tile_y as i32;
        for py in cy0..cy1 {
            let row_base = (py - ty) as usize * WIDTH as usize;
            for px in cx0..cx1 {
                let idx = (row_base + px as usize) * 2;
                let blended = blend::blend565(self.fb_read(idx), src, alpha);
                self.fb_write(idx, blended);
            }
        }
    }
}

// ---- embedded-graphics DrawTarget ------------------------------------------

impl<'fb, B, RST> OriginDimensions for CO5300<'fb, B, RST> {
    fn size(&self) -> Size {
        // The conceptual canvas is the full panel; the driver translates
        // by `tile_y` and clips per-pixel, so callers can draw at any
        // panel-absolute coordinate and let the tile loop sort it out.
        Size::new(WIDTH as u32, HEIGHT as u32)
    }
}

impl<'fb, B, RST> DrawTarget for CO5300<'fb, B, RST> {
    type Color = Rgb565;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Rgb565>>,
    {
        let ty = self.tile_y as i32;
        for Pixel(pt, color) in pixels {
            // Compare in i32 so values outside the u16 range can't wrap
            // through `as u16` into an apparently-valid coordinate.
            let y_local = pt.y - ty;
            if pt.x >= 0
                && y_local >= 0
                && pt.x < WIDTH  as i32
                && y_local < self.fb_rows as i32
            {
                let raw = ((color.r() as u16) << 11)
                    | ((color.g() as u16) << 5)
                    | (color.b() as u16);
                let idx = (y_local as usize * WIDTH as usize + pt.x as usize) * 2;
                self.framebuffer[idx]     = (raw >> 8) as u8;
                self.framebuffer[idx + 1] = (raw & 0xFF) as u8;
            }
        }
        Ok(())
    }

    fn fill_contiguous<I>(
        &mut self,
        area: &Rectangle,
        colors: I,
    ) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Rgb565>,
    {
        // Work entirely in signed i32 so we can reason about areas that
        // straddle the framebuffer edges (e.g. a rectangle with negative
        // y origin). Casting an i32 < 0 to u16 wraps to a huge value
        // and silently breaks any subsequent .min() clipping.
        //
        // y is translated by tile_y before clipping into the FB so the
        // caller stays in panel-absolute coordinates.
        let ty  = self.tile_y as i32;
        let ax0 = area.top_left.x;
        let ay0 = area.top_left.y - ty;
        let ax1 = ax0 + area.size.width  as i32; // exclusive
        let ay1 = ay0 + area.size.height as i32; // exclusive

        // Completely outside the framebuffer - nothing to draw and the
        // source iterator can simply be dropped.
        if ax1 <= 0 || ay1 <= 0 || ax0 >= WIDTH as i32 || ay0 >= self.fb_rows as i32 {
            return Ok(());
        }

        // Clip to framebuffer bounds, exclusive on the right/bottom.
        let cx0 = ax0.max(0);
        let cy0 = ay0.max(0);
        let cx1 = ax1.min(WIDTH  as i32);
        let cy1 = ay1.min(self.fb_rows as i32);

        // Count of source pixels to skip on each side of the clip. These
        // are always >= 0 because of the .max(0) / .min(...) clamps.
        let skip_left  = (cx0 - ax0) as usize;
        let skip_right = (ax1 - cx1) as usize;
        let skip_top   = (cy0 - ay0) as usize;
        let src_row_w  = area.size.width as usize;

        let mut colors = colors.into_iter();

        // Advance past any entirely-clipped top rows so the first drawn
        // row reads the correct source pixels.
        for _ in 0..(skip_top * src_row_w) {
            if colors.next().is_none() { return Ok(()); }
        }

        for row in cy0..cy1 {
            // Skip pixels clipped on the left edge.
            for _ in 0..skip_left {
                if colors.next().is_none() { return Ok(()); }
            }

            let row_base = row as usize * WIDTH as usize;
            for col in cx0..cx1 {
                match colors.next() {
                    None => return Ok(()),
                    Some(color) => {
                        let raw = ((color.r() as u16) << 11)
                            | ((color.g() as u16) << 5)
                            | (color.b() as u16);
                        let idx = (row_base + col as usize) * 2;
                        self.framebuffer[idx]     = (raw >> 8) as u8;
                        self.framebuffer[idx + 1] = (raw & 0xFF) as u8;
                    }
                }
            }

            // Skip pixels clipped on the right edge.
            for _ in 0..skip_right {
                if colors.next().is_none() { return Ok(()); }
            }
        }
        Ok(())
    }

    fn clear(&mut self, color: Rgb565) -> Result<(), Self::Error> {
        let raw = ((color.r() as u16) << 11)
            | ((color.g() as u16) << 5)
            | (color.b() as u16);
        let hi = (raw >> 8) as u8;
        let lo = (raw & 0xFF) as u8;
        for chunk in self.framebuffer.chunks_exact_mut(2) {
            chunk[0] = hi;
            chunk[1] = lo;
        }
        Ok(())
    }
}
