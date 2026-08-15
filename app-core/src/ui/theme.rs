//! Nightwatch OS palette - Cyberpunk 2077-inspired HUD tokens.
//!
//! All colors are full-saturation to fake emission on the AMOLED.
//! Only `#000000` is acceptable as a background - anywhere a "dark
//! surface" is needed, use `INK` or `INK_2` (near-black panel tints),
//! never grey. Borders are accent-colored when active, `STEEL` only
//! when the control is disabled.
//!
//! Layer convention:
//! * `BG` / `INK*` - surfaces, filled rectangles
//! * `STEEL*` / `CHROME` / `BONE` - neutrals for dividers, muted text,
//!   body text
//! * `SIGNAL` / `CYAN` / `YELLOW` / `GREEN` / `ORANGE` - accents; one
//!   dominant accent per screen. Signal red is the default HUD chrome;
//!   cyan is secondary (labels, info); yellow is reserved for the
//!   active-tab state; green = ok; orange = media.
//!
//! Semantic aliases (`FG`, `FG_MUTED`, `DANGER`, ...) are resolved at
//! compile time to one of the palette constants, so screens can
//! reference intent rather than color and the palette can shift
//! centrally.

use embedded_graphics::pixelcolor::Rgb565;

/// The UI's pixel type. Every screen, widget, and helper names this
/// alias instead of a concrete `embedded-graphics` color type, so the
/// whole UI can move to a deeper format (e.g. `Rgb888` composited
/// down to the panel) by changing this one line plus the driver's
/// `BlendTarget` supertrait. The renderer stays on RGB565 + ordered
/// dithering by decision - this alias is the escape hatch, not a plan.
pub type Color = Rgb565;

// -- Neutrals ---------------------------------------------------------------

/// True black - the only acceptable background on AMOLED.
pub const BG:      Color = Color::new(0, 0, 0);
/// Near-black panel fill (#050608).
pub const INK:     Color = Color::new(0, 1, 1);
/// Slightly lifted surface (#0B0D12). Use for pulldown overlays or
/// panels that need to read as "above" the base INK layer.
#[allow(dead_code)]
pub const INK_2:   Color = Color::new(1, 3, 2);
/// Elevated surface (#14171E). Toggle trough, deeper sub-panel fill.
pub const INK_3:   Color = Color::new(2, 5, 3);
/// Divider / inactive border (#2A2F3A).
pub const STEEL:   Color = Color::new(5, 11, 7);
/// Disabled-state text / inert pill handle (#474D5A).
pub const STEEL_2: Color = Color::new(8, 19, 11);
/// Muted metadata / captions (#8A93A3).
pub const CHROME:  Color = Color::new(17, 36, 20);
/// Body text (#E6E9EE). Technically not pure white so it reads as
/// "bone" rather than "paper" on the black field.
pub const BONE:    Color = Color::new(28, 58, 29);

// -- Accents ----------------------------------------------------------------

/// Signal red (#FF003C). Primary HUD chrome, default accent.
pub const SIGNAL:      Color = Color::new(31, 0, 7);
/// Signal red hover peak (#FF3355).
#[allow(dead_code)]
pub const SIGNAL_HOT:  Color = Color::new(31, 6, 10);
/// Dim signal red (#A8002A). Pressed / inactive.
pub const SIGNAL_DIM:  Color = Color::new(21, 0, 5);
/// Very dim signal red (#4A0014). Panel tint.
#[allow(dead_code)]
pub const SIGNAL_DEEP: Color = Color::new(9, 0, 2);

/// Cyan (#00F0FF). Secondary - labels, info, computer icons.
pub const CYAN:      Color = Color::new(0, 60, 31);
/// Cyan hover peak (#7BFBFF).
#[allow(dead_code)]
pub const CYAN_HOT:  Color = Color::new(15, 62, 31);
/// Dim cyan (#0098A6).
#[allow(dead_code)]
pub const CYAN_DIM:  Color = Color::new(0, 38, 20);

/// Yellow (#FFEE00). Active-tab state only.
pub const YELLOW: Color = Color::new(31, 59, 0);

/// Green (#00FF9C). Ok / safe / charging.
pub const GREEN:  Color = Color::new(0, 63, 19);

/// Orange (#FF8A00). Media, data streams, secondary warning.
pub const ORANGE: Color = Color::new(31, 34, 0);

// -- Semantic aliases -------------------------------------------------------

/// Primary body text.
pub const FG:       Color = BONE;
/// Secondary / caption text.
pub const FG_MUTED: Color = CHROME;
/// Tertiary / disabled text.
pub const FG_DIM:   Color = STEEL_2;
/// Default surface fill.
#[allow(dead_code)]
pub const SURFACE:  Color = INK;
/// Divider / inactive border.
#[allow(dead_code)]
pub const BORDER:   Color = STEEL;
/// Semantic danger / critical.
pub const DANGER:   Color = SIGNAL;
/// Semantic ok.
#[allow(dead_code)]
pub const OK:       Color = GREEN;
/// Semantic warning.
#[allow(dead_code)]
pub const WARN:     Color = YELLOW;
/// Semantic info.
#[allow(dead_code)]
pub const INFO:     Color = CYAN;

// -- Screen geometry ---------------------------------------------------------

pub const SCREEN_W: u16 = 410;
pub const SCREEN_H: u16 = 502;

/// Bezel rounded-corner radius. No content should land outside this inset
/// from each corner.
pub const CORNER_R: i32 = 98;

// -- Layout zones ------------------------------------------------------------
//
// These describe the bezel-safe content band. Full-screen apps may
// still draw into the corner zones, but content placed there needs to
// stay horizontally centered enough to clear the rounded bezel.

/// Full-width-safe content band starts here.
pub const CONTENT_TOP: i32 = CORNER_R;
/// Full-width-safe content band ends here.
pub const CONTENT_BOTTOM: i32 = (SCREEN_H as i32) - CORNER_R;
/// Full-width-safe content band height (306 px).
pub const CONTENT_H: i32 = CONTENT_BOTTOM - CONTENT_TOP;

/// Distance from the bottom screen edge that a bottom-anchored
/// element's bottom edge should sit at to clear the bezel arc with
/// breathing room. Use for CTA button rows, info tiles, status pills,
/// or any other UI parked at the foot of a screen. Not meant for the
/// natural bottom of a scrolling list - those scroll past the bezel
/// and rely on clipping, not on a clearance margin.
pub const BOTTOM_SAFE_MARGIN: i32 = 64;

/// Side margin for content area.
#[allow(dead_code)]
pub const MARGIN: i32 = 8;
/// Default corner radius for rounded panels and cards.
#[allow(dead_code)]
pub const CARD_RADIUS: u32 = 16;

/// Depth (in pixels) of the system-gesture edge zone at the top and
/// bottom of the display. A swipe whose *start* y lands within this
/// many pixels of the top or bottom screen edge is classified as an
/// edge gesture (system-level, e.g. pull-down-to-open-panel) rather
/// than a content gesture. Kept deliberately tighter than the bezel
/// corner radius so only gestures actually starting near the edge
/// qualify - accidental brushes in the middle of the screen should
/// be classified as content.
pub const EDGE_GESTURE_ZONE: i32 = 48;
