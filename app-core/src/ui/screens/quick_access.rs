//! Quick Access pull-down overlay.
//!
//! Reached by swiping down from the top edge (the Model routes that
//! gesture via `open_quick_access`). Brightness slider and four of
//! the eight tiles back real config; the remaining four are visual
//! stubs until their underlying drivers land.
//!
//! Layout (410x502 canvas):
//! - Top row: `QUICK.ACCESS` title in cyan + `↓ PULL` telemetry hint.
//! - Brightness section: `BRIGHTNESS` label and a clickable /
//!   draggable horizontal slider with right-aligned value readout.
//! - Toggle grid: 4 across x 2 rows = 8 tiles, each with an icon
//!   over its label. Off = steel border + chrome caption. On =
//!   signal fill + signal border + black icon/caption.
//! - Bottom: 2px signal-red home-indicator bar, centered.
//!
//! ## Tile kinds
//!
//! Each tile is one of three [`TileKind`]s:
//! - [`Toggle`]: backed by a `config` bool. The on-state comes from
//!   `is_on(data)` and tap fires `action`.
//! - [`Momentary`]: no on-state. Tap fires `action` and the tile
//!   resets to chrome.
//! - [`Stub`]: ephemeral, on-state stored in `tiles_on`. Tap flips
//!   it locally; nothing real changes. For tiles whose real backing
//!   isn't built yet (AIR / FLASH / BT / WIFI).
//!
//! [`Toggle`]: TileKind::Toggle
//! [`Momentary`]: TileKind::Momentary
//! [`Stub`]: TileKind::Stub
//!
//! Interactions:
//! - Tap / drag on the brightness bar: scrub via [`SetBrightness`].
//! - Tap on a tile: dispatch per its [`TileKind`].
//! - Swipe up from anywhere: close overlay, return to the pre-overlay
//!   screen.
//!
//! [`SetBrightness`]: Action::SetBrightness

use embedded_graphics::{
    geometry::{Point, Size},
    prelude::Primitive,
    primitives::{PrimitiveStyle, Rectangle},
    Drawable,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;
use heapless::String;
use core::fmt::Write;

use crate::events::{SwipeDir, SystemEvent};
use crate::ui::{fonts, glyphs, theme};
use crate::ui::types::{Action, RenderCtx, Screen, ScreenId, SystemData};
use crate::ui::widgets::{
    chamfered_panel, home_indicator, slider, slider_value_from_x, status_bar,
    NOTCH, SLIDER_BAR_H, STATUS_BAR_H,
};

/// Slider lower bound for brightness. The hardware can render
/// below this but in practice anything dimmer is unreadable on
/// AMOLED at room light, so the slider clips the bottom 5 %.
const BRIGHT_MIN_PCT: u8 = 5;

// -- Tile metadata -----------------------------------------------------------

/// Icon kind for a tile. Enum-dispatched to the concrete glyph at
/// render time (same pattern the App Drawer uses).
#[derive(Clone, Copy)]
enum TileIcon {
    Dnd,
    Vibrate,   // phone-handset glyph, stands in for the haptic toggle
    Flash,     // lightning bolt
    Sounds,    // bell
    Bluetooth,
    Wifi,
    NightMode, // moon
    Lock,
}

fn draw_tile_icon<D: BlendTarget>(
    display: &mut D, kind: TileIcon, cx: i32, cy: i32, color: Color,
) {
    let r = 9;
    match kind {
        TileIcon::Dnd       => glyphs::dnd(display, cx, cy, r, color),
        TileIcon::Vibrate   => glyphs::phone(display, cx, cy, r, color),
        TileIcon::Flash     => glyphs::bolt(display, cx, cy, r, color),
        TileIcon::Sounds    => glyphs::bell(display, cx, cy, r, color),
        TileIcon::Bluetooth => glyphs::bluetooth_small(display, cx, cy, r, color),
        TileIcon::Wifi      => glyphs::signal_small(display, cx, cy, r, color),
        TileIcon::NightMode => glyphs::moon(display, cx, cy, r, color),
        TileIcon::Lock      => glyphs::lock(display, cx, cy, r, color),
    }
}

/// What a tile does on tap, plus how it sources its on-state.
#[derive(Clone, Copy)]
enum TileKind {
    /// Backed by a real config bool. `is_on(data)` paints the visual
    /// state; tap fires `action` (typically `Action::Toggle*`).
    Toggle { is_on: fn(&SystemData) -> bool, action: Action },
    /// One-shot button: no on-state; tap fires `action` once.
    Momentary { action: Action },
    /// No backing yet. On-state lives on the screen struct in
    /// [`QuickAccessScreen::tiles_on`]; tap flips it locally so the
    /// tile still feels responsive while we wait for the underlying
    /// driver / feature to land.
    Stub,
}

#[derive(Clone, Copy)]
struct TileDef {
    label: &'static str,
    icon: TileIcon,
    kind: TileKind,
}

fn dnd_is_on(d: &SystemData) -> bool { d.config.alerts.dnd }
fn haptics_is_on(d: &SystemData) -> bool { d.config.alerts.haptics_enabled }
fn sound_is_on(d: &SystemData) -> bool { d.config.alerts.sound_enabled }
fn night_mode_is_on(d: &SystemData) -> bool { d.config.display.night_mode }

const TILES: [TileDef; 8] = [
    TileDef {
        label: "DND", icon: TileIcon::Dnd,
        kind: TileKind::Toggle { is_on: dnd_is_on, action: Action::ToggleDnd },
    },
    TileDef {
        label: "VIBRATE", icon: TileIcon::Vibrate,
        kind: TileKind::Toggle { is_on: haptics_is_on, action: Action::ToggleHaptics },
    },
    TileDef { label: "FLASH",  icon: TileIcon::Flash,     kind: TileKind::Stub },
    TileDef {
        label: "SOUNDS", icon: TileIcon::Sounds,
        kind: TileKind::Toggle { is_on: sound_is_on, action: Action::ToggleSound },
    },
    TileDef { label: "BT",     icon: TileIcon::Bluetooth, kind: TileKind::Stub },
    TileDef { label: "WIFI",   icon: TileIcon::Wifi,      kind: TileKind::Stub },
    TileDef {
        label: "NIGHT", icon: TileIcon::NightMode,
        kind: TileKind::Toggle { is_on: night_mode_is_on, action: Action::ToggleNightMode },
    },
    TileDef {
        label: "LOCK", icon: TileIcon::Lock,
        kind: TileKind::Momentary { action: Action::Sleep },
    },
];

// -- Layout constants --------------------------------------------------------

const PAD_X: i32 = 22;

/// Y of the top status bar.
const STATUS_Y: i32 = 0;
/// Horizontal inset for status-bar content around the bezel arc.
const STATUS_X_INSET: i32 = 85;

const HEADER_Y: i32 = STATUS_Y + STATUS_BAR_H + 26;

/// Brightness bar block.
const BRIGHT_LABEL_Y: i32 = HEADER_Y + 46;
const BRIGHT_BAR_Y:   i32 = BRIGHT_LABEL_Y + 26;

/// Toggle grid: 4 tiles per row, 2 rows.
const TOGGLE_TOP: i32 = BRIGHT_BAR_Y + 54;
const TOGGLE_GAP: i32 = 6;

/// Bottom indicator bar.
const HOME_BAR_Y: i32 = theme::SCREEN_H as i32 - 18;

fn bright_bar_rect() -> Rectangle {
    Rectangle::new(
        Point::new(PAD_X, BRIGHT_BAR_Y),
        Size::new(
            (theme::SCREEN_W as i32 - PAD_X * 2) as u32,
            SLIDER_BAR_H as u32,
        ),
    )
}

/// Side length of a square toggle tile, sized so 4 tiles + 3 gaps
/// fill the row's available width.
fn tile_size() -> i32 {
    let total_w = theme::SCREEN_W as i32 - PAD_X * 2;
    (total_w - TOGGLE_GAP * 3) / 4
}

fn toggle_rect(idx: usize) -> Rectangle {
    let row = idx / 4;
    let col = idx % 4;
    let s = tile_size();
    let x = PAD_X + col as i32 * (s + TOGGLE_GAP);
    let y = TOGGLE_TOP + row as i32 * (s + TOGGLE_GAP);
    Rectangle::new(Point::new(x, y), Size::new(s as u32, s as u32))
}

// -- Screen ------------------------------------------------------------------

pub struct QuickAccessScreen {
    /// Pre-overlay screen. The Model's nav stack already carries
    /// this entry, so the close path uses `Action::Back` to pop it.
    /// Field kept for future use (e.g. a "launched from X" hint).
    #[allow(dead_code)]
    previous: ScreenId,
    /// Ephemeral on/off state for [`TileKind::Stub`] tiles only.
    /// Resets every time the overlay reopens, since stub tiles don't
    /// back any real config yet. Indices align with [`TILES`]; values
    /// at non-Stub indices are unread.
    tiles_on: [bool; 8],
}

impl QuickAccessScreen {
    pub fn new(previous: ScreenId) -> Self {
        Self {
            previous,
            tiles_on: [false; 8],
        }
    }

    /// Current brightness percent as read from the live config on
    /// `data`. Converts the hardware 0..=255 back into the 5..=100
    /// slider range.
    fn brightness_pct(data: &SystemData) -> u8 {
        let hw = data.config.display.brightness_active as u16;
        ((hw * 100 / 255) as u8).clamp(BRIGHT_MIN_PCT, 100)
    }
}

impl Screen for QuickAccessScreen {
    fn render<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        _ctx: &RenderCtx,
    ) {
        // Top status bar with cyan tint per the spec (Quick Access is
        // a cyan-accent overlay).
        let mut time_buf: heapless::String<8> = heapless::String::new();
        let _ = core::fmt::Write::write_fmt(
            &mut time_buf,
            format_args!("{:02}:{:02}", data.time.hour, data.time.minute),
        );
        status_bar(
            display,
            STATUS_Y,
            time_buf.as_str(),
            data.power.battery_percent,
            theme::CYAN,
            STATUS_X_INSET,
        );
        // Header.
        let font_title = fonts::value();
        fonts::draw_at(
            display, &font_title,
            "QUICK.ACCESS",
            PAD_X, HEADER_Y - 8,
            theme::CYAN,
        );
        fonts::draw_right(
            display, &fonts::caption(),
            "v PULL",
            theme::SCREEN_W as i32 - PAD_X, HEADER_Y,
            theme::FG_MUTED,
        );

        // Brightness label - the slider widget handles its own value
        // readout, this just paints the section title on the left.
        let brightness = Self::brightness_pct(data);
        fonts::draw_at(
            display, &fonts::caption(),
            "BRIGHTNESS",
            PAD_X, BRIGHT_LABEL_Y,
            theme::CYAN,
        );

        // Brightness bar: generic slider widget. Bar full width
        // represents `5..=max_pct`, where `max_pct` honours night
        // mode (30 % cap when on, 100 % otherwise), so a value at
        // the top of the range fills the bar regardless of mode.
        let max_pct = data.config.display.max_brightness_pct();
        let mut label: String<8> = String::new();
        let _ = write!(label, "{:02}%", brightness);
        slider(
            display, bright_bar_rect(),
            brightness, BRIGHT_MIN_PCT, max_pct,
            Some(label.as_str()),
        );

        // Tile grid.
        for (i, t) in TILES.iter().enumerate() {
            let rect = toggle_rect(i);
            let on = match t.kind {
                TileKind::Toggle { is_on, .. } => is_on(data),
                TileKind::Momentary { .. }     => false,
                TileKind::Stub                 => self.tiles_on[i],
            };

            // Off: chrome label + chrome icon + steel border, no fill.
            // On: black label + black icon on a signal fill, signal border.
            let border = if on { theme::SIGNAL } else { theme::STEEL };
            let content_color = if on { theme::BG } else { theme::FG_MUTED };

            if on {
                Rectangle::new(
                    Point::new(rect.top_left.x + 1, rect.top_left.y + 1),
                    Size::new(
                        (rect.size.width as i32 - 2) as u32,
                        (rect.size.height as i32 - 2) as u32,
                    ),
                )
                .into_styled(PrimitiveStyle::with_fill(theme::SIGNAL))
                .draw(display).ok();
            }

            let tile_notch = NOTCH - 4;
            chamfered_panel(display, rect, tile_notch, border, 1);

            // Icon in the upper half, label in the lower half.
            let h = rect.size.height as i32;
            let icon_cx = rect.top_left.x + rect.size.width as i32 / 2;
            let icon_cy = rect.top_left.y + h * 38 / 100;
            draw_tile_icon(display, t.icon, icon_cx, icon_cy, content_color);

            let label_rect = Rectangle::new(
                Point::new(rect.top_left.x, rect.top_left.y + h * 60 / 100),
                Size::new(rect.size.width, (h * 40 / 100) as u32),
            );
            fonts::draw_centered_in_rect(
                display, &fonts::caption(),
                t.label, label_rect, content_color,
            );
        }

        home_indicator(display, HOME_BAR_Y, theme::SIGNAL);
    }

    fn on_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::PowerButtonLong => Action::Shutdown,

            // Swipe up from anywhere → close. `Action::Back` pops
            // the pre-overlay screen off the nav stack and switches
            // to it, so no orphan entry is left behind.
            SystemEvent::Swipe { dir: SwipeDir::Up, .. } => Action::Back,

            // Dragging on the brightness bar scrubs the value live.
            // Each meaningful delta fires a SetBrightness action so the
            // Model can update config + hardware. The screen itself
            // keeps no brightness state - next render reads back from
            // `data.config`.
            SystemEvent::TouchPressed { x, y } => {
                let max_pct = data.config.display.max_brightness_pct();
                if let Some(v) = slider_value_from_x(
                    bright_bar_rect(), *x as i32, *y as i32,
                    BRIGHT_MIN_PCT, max_pct,
                ) {
                    if v != Self::brightness_pct(data) {
                        return Action::SetBrightness { percent: v };
                    }
                }
                Action::None
            }

            // Tap handling: only toggle tiles here. Bar taps are
            // already handled by the TouchPressed -> TouchReleased
            // cycle above (TouchPressed applies via SetBrightness,
            // TouchReleased flushes SaveConfig). Re-handling here
            // would re-dirty the config with no following release
            // to flush it.
            SystemEvent::Tap { x, y } => {
                let pt = Point::new(*x as i32, *y as i32);
                let max_pct = data.config.display.max_brightness_pct();
                // Ignore taps that land on the brightness bar.
                if slider_value_from_x(
                    bright_bar_rect(), *x as i32, *y as i32,
                    BRIGHT_MIN_PCT, max_pct,
                ).is_some() {
                    return Action::None;
                }
                for (i, t) in TILES.iter().enumerate() {
                    if !toggle_rect(i).contains(pt) { continue; }
                    return match t.kind {
                        TileKind::Toggle { action, .. } => action,
                        TileKind::Momentary { action }  => action,
                        TileKind::Stub => {
                            self.tiles_on[i] = !self.tiles_on[i];
                            Action::Redraw
                        }
                    };
                }
                Action::None
            }

            _ => Action::None,
        }
    }
}
