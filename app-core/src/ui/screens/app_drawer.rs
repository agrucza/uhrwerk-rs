//! App Drawer - the 3x3 launcher grid.
//!
//! Reached by swiping up from the bottom edge (the Model routes that
//! gesture) or by tapping anywhere on the Watch Face. Overlay-style:
//! launching an app via a tile tap replaces the drawer on the nav
//! stack (same modal semantics the old pull-down Panel had), so
//! `Action::Back` from the launched app returns to the pre-drawer
//! screen, not the drawer itself.
//!
//! Layout (410x502 canvas):
//! - Top row: `APPS` title in signal red + `N INSTALLED` telemetry.
//! - Middle: 3x3 grid of chamfered tiles (per-app accent border +
//!   uppercase caption).
//! - Bottom: 2px home-indicator bar in signal red, centered.
//!
//! Non-real tiles are dimmed with a `BORDER` steel + muted caption
//! so the grid geometry stays complete even with fewer than 9 apps.

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
use crate::ui::{glyphs, theme};
use crate::ui::types::{Action, RenderCtx, Screen, ScreenId, SystemData};
use crate::ui::widgets::{app_home_bar_y, draw_overlay_chrome, overlay_title_y, tile};

// -- Icon dispatch -----------------------------------------------------------
//
// Tile icons are held as an `IconKind` enum rather than a generic
// function pointer because the glyph functions are generic over
// `BlendTarget` and `BlendTarget` isn't object-safe (generic methods).
// The enum lets a const tile table exist while still dispatching to
// the right concrete glyph in `render`.

#[derive(Clone, Copy)]
enum IconKind {
    Clock,
    Stopwatch,
    Timer,
    Alarm,
    Settings,
    Heart,
    /// Placeholder glyph for unused slots: a small hollow square.
    Empty,
}

fn draw_icon<D: BlendTarget>(
    display: &mut D,
    kind: IconKind,
    cx: i32, cy: i32,
    radius: i32,
    color: Color,
) {
    match kind {
        IconKind::Clock     => glyphs::clock(display, cx, cy, radius, color),
        IconKind::Stopwatch => glyphs::stopwatch(display, cx, cy, radius, color),
        IconKind::Timer     => glyphs::hourglass(display, cx, cy, radius, color),
        IconKind::Alarm     => glyphs::bell(display, cx, cy, radius, color),
        IconKind::Settings  => glyphs::settings(display, cx, cy, radius, color),
        IconKind::Heart     => glyphs::heart(display, cx, cy, radius, color),
        IconKind::Empty     => {
            let size = (radius * 2 / 3).max(6);
            let x = cx - size / 2;
            let y = cy - size / 2;
            Rectangle::new(Point::new(x, y), Size::new(size as u32, size as u32))
                .into_styled(PrimitiveStyle::with_stroke(color, 1))
                .draw(display).ok();
        }
    }
}

// -- Tile table --------------------------------------------------------------

/// One tile. `target` is `None` for placeholders that fill geometry
/// without launching.
#[derive(Clone, Copy)]
struct TileDef {
    target: Option<ScreenId>,
    caption: &'static str,
    border: Color,
    icon: IconKind,
}

/// The 9 drawer tiles in row-major order. 5 launch real screens, 4
/// are placeholders to keep the grid complete.
const TILES: [TileDef; 9] = [
    // Row 0
    TileDef { target: Some(ScreenId::Settings),  caption: "SYS.CFG",  border: theme::ACCENT, icon: IconKind::Settings  },
    TileDef { target: None,                      caption: "VITALS",   border: theme::BORDER,  icon: IconKind::Heart     },
    TileDef { target: Some(ScreenId::Clock),     caption: "CLOCK",    border: theme::INFO,   icon: IconKind::Clock     },
    // Row 1
    TileDef { target: Some(ScreenId::Stopwatch), caption: "STPWCH",   border: theme::OK,  icon: IconKind::Stopwatch },
    TileDef { target: Some(ScreenId::Timer),     caption: "TIMER",    border: theme::MEDIA, icon: IconKind::Timer     },
    TileDef { target: Some(ScreenId::Alarm),     caption: "ALARM",    border: theme::ALERT, icon: IconKind::Alarm     },
    // Row 2
    TileDef { target: None,                      caption: "",         border: theme::BORDER,  icon: IconKind::Empty     },
    TileDef { target: None,                      caption: "MSG",      border: theme::BORDER,  icon: IconKind::Empty     },
    TileDef { target: None,                      caption: "CAL",      border: theme::BORDER,  icon: IconKind::Empty     },
];

// -- Layout constants --------------------------------------------------------

fn grid_top(safe: &crate::data::SafeArea) -> i32 {
    overlay_title_y(safe) + 34
}
const GRID_PAD_X: i32 = 24;
const GRID_GAP: i32 = 8;

fn grid_bottom(safe: &crate::data::SafeArea) -> i32 {
    app_home_bar_y(safe) - 24
}

fn tile_rect(row: usize, col: usize, safe: &crate::data::SafeArea) -> Rectangle {
    let total_w = theme::SCREEN_W as i32 - GRID_PAD_X * 2;
    let tile_w = (total_w - GRID_GAP * 2) / 3;
    let total_h = grid_bottom(safe) - grid_top(safe);
    let tile_h = (total_h - GRID_GAP * 2) / 3;

    let x = GRID_PAD_X + col as i32 * (tile_w + GRID_GAP);
    let y = grid_top(safe) + row as i32 * (tile_h + GRID_GAP);
    Rectangle::new(
        Point::new(x, y),
        Size::new(tile_w as u32, tile_h as u32),
    )
}

// -- Screen -----------------------------------------------------------------

pub struct AppDrawerScreen {
    /// Pre-drawer screen the user came from. Used to render the
    /// matching tile with a thicker border so the grid shows the
    /// "launched from here" context. The Model's nav stack also
    /// carries this entry, so the close path uses `Action::Back`
    /// to pop it.
    previous: ScreenId,
}

impl AppDrawerScreen {
    pub fn new(previous: ScreenId) -> Self {
        Self { previous }
    }
}

impl Screen for AppDrawerScreen {
    fn render<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        let installed = TILES.iter().filter(|t| t.target.is_some()).count();
        let mut buf: String<16> = String::new();
        let _ = write!(buf, "{:02} INSTALLED", installed);
        draw_overlay_chrome(
            display, data,
            "APPS", buf.as_str(),
            theme::ACCENT, GRID_PAD_X,
            ctx,
        );

        // 3x3 tile grid. The tile whose `target` matches the
        // pre-drawer screen gets two visual cues as a "you came from
        // here" indicator (the spec's glow can't be rendered):
        //   - interior fill in SURFACE_3 (raised dark, distinct from
        //     the black background the other tiles sit on)
        //   - 2 px border instead of 1 px
        for (i, t) in TILES.iter().enumerate() {
            let row = i / 3;
            let col = i % 3;
            let rect = tile_rect(row, col, &data.safe_area);

            let is_active = t.target == Some(self.previous);

            if is_active {
                // Fill the tile interior with SURFACE_3 before the border.
                // A small inset keeps the fill inside the chamfer
                // lines so the corners still read as cut.
                let inset = Rectangle::new(
                    Point::new(rect.top_left.x + 2, rect.top_left.y + 2),
                    Size::new(
                        (rect.size.width as i32 - 4) as u32,
                        (rect.size.height as i32 - 4) as u32,
                    ),
                );
                inset.into_styled(PrimitiveStyle::with_fill(theme::SURFACE_3))
                    .draw(display).ok();
            }

            let icon_color = if t.target.is_some() {
                t.border
            } else {
                theme::FG_DIM
            };
            let stroke = if is_active { 2 } else { 1 };
            let kind = t.icon;
            tile(
                display, rect,
                t.border, stroke,
                |d, cx, cy, c| draw_icon(d, kind, cx, cy, 12, c),
                icon_color,
                t.caption,
            );
        }

    }

    fn on_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::PowerButtonLong => Action::Shutdown,

            // Swipe-down from anywhere closes the drawer. `Action::Back`
            // pops the pre-drawer screen off the nav stack (pushed when
            // the overlay opened) and switches to it, so we don't
            // leave an orphan entry behind.
            SystemEvent::Swipe { dir: SwipeDir::Down, .. } => Action::Back,

            // Tile tap.
            SystemEvent::Tap { x, y } => {
                let pt = Point::new(*x as i32, *y as i32);
                for (i, t) in TILES.iter().enumerate() {
                    let row = i / 3;
                    let col = i % 3;
                    if !tile_rect(row, col, &data.safe_area).contains(pt) { continue; }
                    if let Some(target) = t.target {
                        return Action::SwitchScreen(target);
                    }
                    return Action::None;
                }
                Action::None
            }

            _ => Action::None,
        }
    }
}
