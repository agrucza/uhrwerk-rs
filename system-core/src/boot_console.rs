//! Hard-boot terminal console - the Cyberpunk-style boot sequence.
//!
//! `run()` drives this between bring-up steps: the panel is blanked
//! and lit right after display init, the title block types itself
//! out character by character, and each init step prints one line of
//! real telemetry as it completes (the steps that ran before the
//! display existed land as a burst of backlog lines). The sequence
//! closes with an inverted BOOT COMPLETE bar, then the first real
//! frame replaces the log. Hard boot only - the sleep/wake path
//! never touches this.
//!
//! Rendering goes through the same tile framebuffer as the normal
//! render loop, outside the Screen machinery: every flush redraws
//! the whole log into each tile the changed rows intersect (the
//! driver clips per pixel) and pushes only those tiles. No TE sync -
//! flushes here are one or two tiles onto a mostly static frame.

use alloc::string::String;
use alloc::vec::Vec;

use app_core::ui::{fonts, primitives, theme};
use embassy_time::{Duration, Timer};
use embedded_graphics::draw_target::DrawTarget;
use firmware_hal::display::{HEIGHT, NUM_TILES, TILE_H};

use crate::display::Display;

// -- Layout -------------------------------------------------------------------

/// Left edge of the log column - 12 px inside the bars, which are
/// themselves centered, so the whole block sits symmetric on the
/// panel (the reference's left-heavy margin only works on a round
/// face, where the curve eats the bar's right end).
const LEFT_X: i32 = BAR_X + 12;
/// Status column x for `LABEL  STATUS` log rows.
const STATUS_X: i32 = LEFT_X + 92;
/// Top of the first row. The log column is narrow enough that the
/// corner arcs barely reach it - the binding constraint is the
/// T-Watch case lip's arc (safe-area corner r=112 + 8 px top): at
/// the bars' x=68 it claims y < ~26. The round Waveshare bezel
/// (R=98) claims only y < ~5 there. NOT the theme's CONTENT_TOP -
/// that rule is for full-width content.
const TOP_Y: i32 = 36;
/// Rows never extend past this y: appending past it scrolls the log
/// instead (rows drop off the top, terminal-style). Same arc math
/// as [`TOP_Y`] mirrored at the bottom: worst case allows ~480 at
/// the bars' x extents; margin brings it in.
const BOTTOM_LIMIT: i32 = 472;
/// Extra leading between rows on top of each font's line box.
const LEADING: i32 = 6;
/// Blank-row height between sections.
const GAP_H: i32 = 8;
/// Inverted bars: centered, equal margins both sides.
const BAR_X: i32 = 68;
const BAR_W: i32 = theme::SCREEN_W as i32 - 2 * BAR_X;
/// Vertical text padding inside an inverted bar.
const BAR_PAD: i32 = 5;

// -- Pacing -------------------------------------------------------------------

/// Typewriter reveal per character (title block only).
const TYPE_MS: u64 = 14;
/// Hold after a typed line completes.
const TYPE_END_MS: u64 = 90;
/// Stagger between burst log lines.
const BURST_MS: u64 = 35;
/// Hold after an inverted bar lands.
const BAR_MS: u64 = 150;

/// What a bin declares about its boot log beyond the lines it prints
/// itself (bins print their synchronously-initialized hardware live,
/// from inside `make_input` / `make_sensors`). `run()` owns ordering
/// and pacing.
pub struct BootHw {
    /// Device name, printed as a full-width line ("LILYGO T-WATCH
    /// ULTRA").
    pub platform: &'static str,
    /// The PMU chip. Initialized before the display exists, so its
    /// line can only ever be backlog - declared here and printed by
    /// `run()` instead of live from `make_power`.
    pub pmu: &'static str,
    /// Hardware whose bring-up runs inside a task after spawn (IMU
    /// staged upload, radio canaries, haptics), as `(label, chip)`.
    /// `run()` prints "<chip> ..." for each and then waits (bounded)
    /// for the tasks' [`crate::bus::boot_report`] lines, matched by
    /// label.
    pub tasks: &'static [(&'static str, &'static str)],
    /// Hardware that deliberately does NOT init at boot (rail-gated
    /// session peripherals, lazy codecs), printed as "<chip> PARKED"
    /// so the inventory stays complete without faking an init.
    pub parked: &'static [(&'static str, &'static str)],
}

/// One rendered row of the log.
enum Row {
    /// Headline-font accent line (typewritten).
    Title(String),
    /// Dim `//`-style garnish line (typewritten).
    Comment(String),
    /// Full-width accent line, no status column (platform name).
    Wide(String),
    /// `LABEL  STATUS` telemetry line, body font.
    Log { label: String, status: String },
    /// Inverted accent bar with black text.
    Bar(String),
    /// Blank separator.
    Gap,
}

impl Row {
    fn height(&self) -> i32 {
        match self {
            Row::Title(_) => fonts::headline().line_height() + LEADING,
            Row::Comment(_) | Row::Wide(_) | Row::Log { .. } => {
                fonts::body().line_height() + LEADING
            }
            Row::Bar(_) => {
                fonts::body().line_height() + 2 * BAR_PAD + LEADING
            }
            Row::Gap => GAP_H,
        }
    }
}

/// The console state: the rows printed so far plus the typewriter
/// reveal cursor. Owns no hardware - every operation borrows the
/// display for its own flush, so the display can move into the
/// manager mid-boot and the epilogue keeps working on the same rows.
pub struct BootConsole {
    rows: Vec<Row>,
    /// Byte length of the visible prefix of the LAST row's text while
    /// it is being typed out (always a char boundary); `None` = all
    /// rows fully shown, no cursor.
    reveal: Option<usize>,
}

impl BootConsole {
    pub fn new() -> Self {
        Self { rows: Vec::new(), reveal: None }
    }

    // -- Public sequence steps ------------------------------------------------

    /// Blank the whole panel. Must run before the panel is lit: at
    /// power-on GRAM holds random pixels (CO5300 datasheet 7.5.23),
    /// which light as a colored flash otherwise.
    pub async fn clear_panel(&self, d: &mut Display<'_>) {
        for tile in 0..NUM_TILES {
            d.set_tile_y(tile as u16 * TILE_H);
            d.clear(theme::BG).ok();
            d.flush_tile().await;
        }
        d.flush_pending().await;
    }

    /// Typewritten headline row.
    pub async fn title(&mut self, d: &mut Display<'_>, text: &str) {
        self.type_out(d, Row::Title(String::from(text))).await;
    }

    /// Typewritten dim garnish row.
    pub async fn comment(&mut self, d: &mut Display<'_>, text: &str) {
        self.type_out(d, Row::Comment(String::from(text))).await;
    }

    /// Inverted accent bar (with a gap above), landed in one flush.
    pub async fn bar(&mut self, d: &mut Display<'_>, text: &str) {
        let row = Row::Bar(String::from(text));
        let scrolled = self.make_room(GAP_H + row.height());
        let top = self.content_bottom();
        self.rows.push(Row::Gap);
        self.rows.push(row);
        if scrolled {
            self.flush_range(d, TOP_Y, BOTTOM_LIMIT).await;
        } else {
            self.flush_range(d, top, self.content_bottom()).await;
        }
        Timer::after(Duration::from_millis(BAR_MS)).await;
    }

    /// Full-width accent line (no status column), burst-paced.
    pub async fn wide(&mut self, d: &mut Display<'_>, text: &str) {
        self.append(d, Row::Wide(String::from(text))).await;
        Timer::after(Duration::from_millis(BURST_MS)).await;
    }

    /// Completed telemetry line, burst-paced.
    pub async fn log(&mut self, d: &mut Display<'_>, label: &str, status: &str) {
        self.append(d, Row::Log {
            label: String::from(label),
            status: String::from(status),
        })
        .await;
        Timer::after(Duration::from_millis(BURST_MS)).await;
    }

    /// Telemetry line for a step still in flight - prints immediately
    /// (no stagger; the hardware supplies the wait), rewritten via
    /// [`Self::log_set`] (matched by label) when its report lands.
    pub async fn log_begin(
        &mut self,
        d: &mut Display<'_>,
        label: &str,
        status: &str,
    ) {
        self.append(d, Row::Log {
            label: String::from(label),
            status: String::from(status),
        })
        .await;
    }

    /// Rewrite the status column of the last row (a `log_begin` line).
    pub async fn log_update(&mut self, d: &mut Display<'_>, status: &str) {
        let idx = self.rows.len() - 1;
        if let Some(Row::Log { status: s, .. }) = self.rows.last_mut() {
            *s = String::from(status);
        }
        let (top, h) = self.row_span(idx);
        self.flush_range(d, top, top + h).await;
        Timer::after(Duration::from_millis(BURST_MS)).await;
    }

    /// Rewrite the status of the most recent log row carrying
    /// `label` - task reports land in whatever order the hardware
    /// finishes, so the row is found by label, not position. Appends
    /// a fresh line when no such row exists (a report from a task
    /// the bin forgot to declare still shows up).
    pub async fn log_set(&mut self, d: &mut Display<'_>, label: &str, status: &str) {
        let found = self
            .rows
            .iter()
            .rposition(|r| matches!(r, Row::Log { label: l, .. } if l == label));
        match found {
            Some(idx) => {
                if let Row::Log { status: s, .. } = &mut self.rows[idx] {
                    *s = String::from(status);
                }
                let (top, h) = self.row_span(idx);
                self.flush_range(d, top, top + h).await;
                Timer::after(Duration::from_millis(BURST_MS)).await;
            }
            None => self.log(d, label, status).await,
        }
    }

    // -- Internals ------------------------------------------------------------

    /// Push one row and flush: just the new row normally, the whole
    /// band when appending scrolled the log.
    async fn append(&mut self, d: &mut Display<'_>, row: Row) {
        let scrolled = self.make_room(row.height());
        self.rows.push(row);
        if scrolled {
            self.flush_range(d, TOP_Y, BOTTOM_LIMIT).await;
        } else {
            let (top, h) = self.row_span(self.rows.len() - 1);
            self.flush_range(d, top, top + h).await;
        }
    }

    /// Drop rows from the top until a row of height `incoming` fits
    /// above [`BOTTOM_LIMIT`]. Returns true when anything scrolled.
    /// A `Gap` left leading after the drop goes too - a blank first
    /// line reads as a glitch, not a terminal.
    fn make_room(&mut self, incoming: i32) -> bool {
        let mut scrolled = false;
        while !self.rows.is_empty()
            && self.content_bottom() + incoming > BOTTOM_LIMIT
        {
            self.rows.remove(0);
            scrolled = true;
        }
        if scrolled && matches!(self.rows.first(), Some(Row::Gap)) {
            self.rows.remove(0);
        }
        scrolled
    }

    /// Append a Title/Comment row and reveal it character by
    /// character with a block cursor riding the pen.
    async fn type_out(&mut self, d: &mut Display<'_>, row: Row) {
        let scrolled = self.make_room(row.height());
        self.rows.push(row);
        let idx = self.rows.len() - 1;
        let (top, h) = self.row_span(idx);
        let ends: Vec<usize> = match &self.rows[idx] {
            Row::Title(t) | Row::Comment(t) => {
                t.char_indices().map(|(i, c)| i + c.len_utf8()).collect()
            }
            _ => Vec::new(),
        };
        self.reveal = Some(0);
        if scrolled {
            self.flush_range(d, TOP_Y, BOTTOM_LIMIT).await;
        } else {
            self.flush_range(d, top, top + h).await;
        }
        for end in ends {
            Timer::after(Duration::from_millis(TYPE_MS)).await;
            self.reveal = Some(end);
            self.flush_range(d, top, top + h).await;
        }
        // Line complete - drop the cursor and hold a beat.
        self.reveal = None;
        self.flush_range(d, top, top + h).await;
        Timer::after(Duration::from_millis(TYPE_END_MS)).await;
    }

    /// y just below the last row.
    fn content_bottom(&self) -> i32 {
        TOP_Y + self.rows.iter().map(Row::height).sum::<i32>()
    }

    /// `(top, height)` of row `idx`.
    fn row_span(&self, idx: usize) -> (i32, i32) {
        let mut y = TOP_Y;
        for row in &self.rows[..idx] {
            y += row.height();
        }
        (y, self.rows[idx].height())
    }

    /// Re-render + push every tile the y-range `[y0, y1)` touches.
    async fn flush_range(&self, d: &mut Display<'_>, y0: i32, y1: i32) {
        let y0 = y0.clamp(0, HEIGHT as i32 - 1);
        let y1 = y1.clamp(y0 + 1, HEIGHT as i32);
        let t0 = (y0 as u16 / TILE_H) as usize;
        let t1 = (((y1 - 1) as u16) / TILE_H) as usize;
        for tile in t0..=t1.min(NUM_TILES - 1) {
            d.set_tile_y(tile as u16 * TILE_H);
            d.clear(theme::BG).ok();
            self.draw_all(d);
            d.flush_tile().await;
        }
        d.flush_pending().await;
    }

    /// Draw every row at panel-absolute coordinates; the driver clips
    /// to the currently parked tile.
    fn draw_all(&self, d: &mut Display<'_>) {
        let last = self.rows.len().saturating_sub(1);
        let mut y = TOP_Y;
        for (i, row) in self.rows.iter().enumerate() {
            let h = row.height();
            match row {
                Row::Title(t) | Row::Comment(t) => {
                    let (font, color) = match row {
                        Row::Title(_) => (fonts::headline(), theme::ACCENT),
                        _ => (fonts::body(), theme::ACCENT_DIM),
                    };
                    let shown = match self.reveal {
                        Some(n) if i == last => &t[..n],
                        _ => t.as_str(),
                    };
                    fonts::draw_at(d, &font, shown, LEFT_X, y, color);
                    if i == last && self.reveal.is_some() {
                        // Block cursor at the pen position.
                        let pen =
                            LEFT_X + fonts::measure_width(&font, shown) + 2;
                        primitives::rounded_panel(
                            d, pen, y + 2, 9, font.ascent() - 2,
                            0, Some(color), None,
                        );
                    }
                }
                Row::Wide(t) => {
                    fonts::draw_at(
                        d, &fonts::body(), t, LEFT_X, y, theme::ACCENT,
                    );
                }
                Row::Log { label, status } => {
                    let font = fonts::body();
                    fonts::draw_at(d, &font, label, LEFT_X, y, theme::ACCENT);
                    fonts::draw_at(
                        d, &font, status, STATUS_X, y, theme::ACCENT,
                    );
                }
                Row::Bar(t) => {
                    let font = fonts::body();
                    primitives::rounded_panel(
                        d, BAR_X, y, BAR_W, h - LEADING,
                        3, Some(theme::ACCENT), None,
                    );
                    fonts::draw_at(d, &font, t, LEFT_X, y + BAR_PAD, theme::BG);
                }
                Row::Gap => {}
            }
            y += h;
        }
    }
}
