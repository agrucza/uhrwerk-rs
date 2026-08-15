//! Shared icon glyphs for the UI.
//!
//! Every glyph follows the same signature:
//! `(display, cx, cy, radius, color)` - draw an icon centered at
//! `(cx, cy)` fitting inside the given `radius`, using `color` for
//! all strokes and fills.
//!
//! Screens and the panel picker import from here instead of keeping
//! local copies, so visual tweaks to an icon propagate everywhere.

use embedded_graphics::{
    geometry::{Point, Size},
    prelude::Primitive,
    primitives::{Circle, Line, PrimitiveStyle, Rectangle, Triangle},
    Drawable,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;

// -- Clock / time glyphs ----------------------------------------------------

/// Analog-clock glyph: circle with hour and minute hands, center dot.
pub fn clock<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let thin  = PrimitiveStyle::with_stroke(color, 2);
    let thick = PrimitiveStyle::with_stroke(color, 3);

    Circle::with_center(Point::new(cx, cy), (radius * 2) as u32)
        .into_styled(thin).draw(display).ok();

    // Minute hand (pointing up)
    Line::new(
        Point::new(cx, cy),
        Point::new(cx, cy - radius * 2 / 3),
    ).into_styled(thick).draw(display).ok();

    // Hour hand (pointing upper-right ~2 o'clock)
    Line::new(
        Point::new(cx, cy),
        Point::new(cx + radius / 3, cy - radius / 3),
    ).into_styled(thick).draw(display).ok();

    // Center cap
    Circle::with_center(Point::new(cx, cy), 4)
        .into_styled(PrimitiveStyle::with_fill(color))
        .draw(display).ok();
}

/// Stopwatch glyph: a small dial with a button stem on top and a
/// hand pointing up-right. Distinct from the regular clock glyph.
pub fn stopwatch<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let thin  = PrimitiveStyle::with_stroke(color, 2);
    let thick = PrimitiveStyle::with_stroke(color, 3);
    let fill  = PrimitiveStyle::with_fill(color);

    // Slightly smaller main dial so there's room for the stem above
    // without the glyph overflowing its allotted radius.
    let dial_r = radius * 4 / 5;

    // Button stem on top: small filled rectangle.
    Rectangle::new(
        Point::new(cx - 3, cy - dial_r - 5),
        Size::new(6, 5),
    ).into_styled(fill).draw(display).ok();

    // Main dial.
    Circle::with_center(Point::new(cx, cy), (dial_r * 2) as u32)
        .into_styled(thin).draw(display).ok();

    // Hand pointing up-right (~1 o'clock).
    Line::new(
        Point::new(cx, cy),
        Point::new(cx + dial_r / 2, cy - dial_r / 2),
    ).into_styled(thick).draw(display).ok();

    // Center cap.
    Circle::with_center(Point::new(cx, cy), 4)
        .into_styled(fill).draw(display).ok();
}

/// Hourglass glyph: two triangular chambers meeting at a point,
/// with flat horizontal caps top and bottom.
pub fn hourglass<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 3);

    let half_w = radius * 4 / 5;
    let top_y    = cy - radius;
    let bottom_y = cy + radius;

    // Top cap.
    Line::new(
        Point::new(cx - half_w, top_y),
        Point::new(cx + half_w, top_y),
    ).into_styled(stroke).draw(display).ok();

    // Left slant down (top cap left -> pinch point).
    Line::new(
        Point::new(cx - half_w, top_y),
        Point::new(cx, cy),
    ).into_styled(stroke).draw(display).ok();

    // Right slant down (top cap right -> pinch point).
    Line::new(
        Point::new(cx + half_w, top_y),
        Point::new(cx, cy),
    ).into_styled(stroke).draw(display).ok();

    // Left slant up (pinch point -> bottom cap left).
    Line::new(
        Point::new(cx, cy),
        Point::new(cx - half_w, bottom_y),
    ).into_styled(stroke).draw(display).ok();

    // Right slant up (pinch point -> bottom cap right).
    Line::new(
        Point::new(cx, cy),
        Point::new(cx + half_w, bottom_y),
    ).into_styled(stroke).draw(display).ok();

    // Bottom cap.
    Line::new(
        Point::new(cx - half_w, bottom_y),
        Point::new(cx + half_w, bottom_y),
    ).into_styled(stroke).draw(display).ok();
}

/// Stylised bell glyph: small handle on top, trapezoidal body
/// flaring to a horizontal base, and a clapper dot below.
pub fn bell<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    let fill   = PrimitiveStyle::with_fill(color);

    // Handle dot at the very top.
    Circle::with_center(Point::new(cx, cy - radius), 3)
        .into_styled(fill).draw(display).ok();

    // Bell body: narrow at the top, flaring to a wider base.
    let top_half_w  = radius / 3;
    let base_half_w = radius * 3 / 4;
    let top_y  = cy - radius * 2 / 3;
    let base_y = cy + radius / 3;

    // Top cap (narrow horizontal line connecting the two slants).
    Line::new(
        Point::new(cx - top_half_w, top_y),
        Point::new(cx + top_half_w, top_y),
    ).into_styled(stroke).draw(display).ok();

    // Left slant from top cap to base-left.
    Line::new(
        Point::new(cx - top_half_w, top_y),
        Point::new(cx - base_half_w, base_y),
    ).into_styled(stroke).draw(display).ok();

    // Right slant from top cap to base-right.
    Line::new(
        Point::new(cx + top_half_w, top_y),
        Point::new(cx + base_half_w, base_y),
    ).into_styled(stroke).draw(display).ok();

    // Horizontal base.
    Line::new(
        Point::new(cx - base_half_w, base_y),
        Point::new(cx + base_half_w, base_y),
    ).into_styled(stroke).draw(display).ok();

    // Clapper dot just below the base.
    Circle::with_center(Point::new(cx, base_y + 5), 3)
        .into_styled(fill).draw(display).ok();
}

// -- Playback control glyphs ------------------------------------------------

/// Filled right-pointing triangle (play icon).
pub fn play<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    Triangle::new(
        Point::new(cx - radius / 2, cy - radius),
        Point::new(cx - radius / 2, cy + radius),
        Point::new(cx + radius,     cy),
    )
    .into_styled(PrimitiveStyle::with_fill(color))
    .draw(display).ok();
}

/// Two filled vertical bars (pause icon).
pub fn pause<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let bar_w = (radius * 2 / 5).max(3);
    let bar_h = radius * 2;
    let gap = radius / 3;
    let fill = PrimitiveStyle::with_fill(color);
    Rectangle::new(
        Point::new(cx - gap - bar_w, cy - bar_h / 2),
        Size::new(bar_w as u32, bar_h as u32),
    ).into_styled(fill).draw(display).ok();
    Rectangle::new(
        Point::new(cx + gap, cy - bar_h / 2),
        Size::new(bar_w as u32, bar_h as u32),
    ).into_styled(fill).draw(display).ok();
}

/// Filled square (stop / reset icon).
pub fn stop<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let side = radius * 7 / 4;
    Rectangle::new(
        Point::new(cx - side / 2, cy - side / 2),
        Size::new(side as u32, side as u32),
    )
    .into_styled(PrimitiveStyle::with_fill(color))
    .draw(display).ok();
}

// -- App launcher glyphs ----------------------------------------------------

/// Three stacked horizontal bars (settings / menu icon).
pub fn settings<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let style = PrimitiveStyle::with_stroke(color, 3);
    let half_w = radius * 3 / 4;
    let spacing = radius / 3;
    for row in -1..=1 {
        let y = cy + row * spacing;
        Line::new(
            Point::new(cx - half_w, y),
            Point::new(cx + half_w, y),
        )
        .into_styled(style)
        .draw(display)
        .ok();
    }
}

/// Four small filled squares in a 2x2 grid (panel / app-grid icon).
pub fn panel<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let s = (radius / 3).max(4);
    let gap = 4;
    let offsets = [
        (-(s + gap / 2), -(s + gap / 2)),
        ( gap / 2,       -(s + gap / 2)),
        (-(s + gap / 2),  gap / 2),
        ( gap / 2,        gap / 2),
    ];
    let fill = PrimitiveStyle::with_fill(color);
    for (dx, dy) in offsets.iter() {
        Rectangle::new(
            Point::new(cx + dx, cy + dy),
            Size::new(s as u32, s as u32),
        )
        .into_styled(fill).draw(display).ok();
    }
}

// -- Nightwatch icons --------------------------------------------------------
//
// Primitive-drawn equivalents of the SVG sprite in the Nightwatch
// design handoff. Matches the existing glyph signature so the new
// icons drop straight into `tile` / `row` closures the same
// way the older ones do.

/// Heart glyph: two filled semicircles merging into a downward V.
/// Reads as an outline at small radii because the strokes stay 2 px.
pub fn heart<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    // 0.45 * radius via integer math so the glyph compiles in no_std
    // without pulling in a soft-float conversion.
    let r = radius * 9 / 20;
    // Two circles forming the top lobes.
    Circle::with_center(Point::new(cx - r, cy - r / 2), (r * 2) as u32)
        .into_styled(stroke).draw(display).ok();
    Circle::with_center(Point::new(cx + r, cy - r / 2), (r * 2) as u32)
        .into_styled(stroke).draw(display).ok();
    // Two lines meeting at the bottom point.
    Line::new(
        Point::new(cx - r * 2, cy),
        Point::new(cx, cy + radius),
    ).into_styled(stroke).draw(display).ok();
    Line::new(
        Point::new(cx + r * 2, cy),
        Point::new(cx, cy + radius),
    ).into_styled(stroke).draw(display).ok();
}

/// Map-pin glyph: teardrop outline with a hollow dot at the center.
pub fn map_pin<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    let r = radius * 2 / 3;
    // Head: full circle, centered above the geometric center.
    Circle::with_center(Point::new(cx, cy - r / 2), (r * 2) as u32)
        .into_styled(stroke).draw(display).ok();
    // Tail: two diagonal lines from the circle's sides down to the point.
    Line::new(
        Point::new(cx - r, cy),
        Point::new(cx, cy + radius),
    ).into_styled(stroke).draw(display).ok();
    Line::new(
        Point::new(cx + r, cy),
        Point::new(cx, cy + radius),
    ).into_styled(stroke).draw(display).ok();
    // Inner dot.
    Circle::with_center(Point::new(cx, cy - r / 2), (r / 2) as u32)
        .into_styled(PrimitiveStyle::with_fill(color)).draw(display).ok();
}

/// Handset glyph: a rounded bar with two stubs on each end. Drawn as
/// a single tilted rectangle + two short perpendicular lines so it
/// reads as a classic phone handset silhouette.
pub fn phone<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    // Handset body: a diagonal line from upper-left to lower-right.
    Line::new(
        Point::new(cx - radius, cy - radius + 2),
        Point::new(cx + radius, cy + radius - 2),
    ).into_styled(stroke).draw(display).ok();
    // Earpiece stub (upper-left perpendicular).
    Line::new(
        Point::new(cx - radius - 3, cy - radius + 5),
        Point::new(cx - radius + 3, cy - radius - 1),
    ).into_styled(stroke).draw(display).ok();
    // Mouthpiece stub (lower-right perpendicular).
    Line::new(
        Point::new(cx + radius - 3, cy + radius + 1),
        Point::new(cx + radius + 3, cy + radius - 5),
    ).into_styled(stroke).draw(display).ok();
}

/// Calendar glyph: square outline with a thick top bar and two small
/// vertical "ring" lines poking above.
pub fn calendar<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    let fill = PrimitiveStyle::with_fill(color);
    let w = radius * 2;
    let h = radius * 2;
    let x = cx - radius;
    let y = cy - radius + 3;

    // Body outline.
    Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32))
        .into_styled(stroke).draw(display).ok();
    // Filled header strip.
    Rectangle::new(Point::new(x, y), Size::new(w as u32, 4))
        .into_styled(fill).draw(display).ok();
    // Two ring stubs above the header.
    Rectangle::new(
        Point::new(cx - radius / 2 - 1, y - 4),
        Size::new(2, 5),
    ).into_styled(fill).draw(display).ok();
    Rectangle::new(
        Point::new(cx + radius / 2 - 1, y - 4),
        Size::new(2, 5),
    ).into_styled(fill).draw(display).ok();
}

/// Running figure glyph: stick-figure head + diagonal body + bent legs
/// suggesting mid-stride.
pub fn run<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    let head_r = radius / 4;
    // Head.
    Circle::with_center(Point::new(cx + radius / 3, cy - radius), (head_r * 2) as u32)
        .into_styled(PrimitiveStyle::with_fill(color)).draw(display).ok();
    // Torso diagonal.
    Line::new(
        Point::new(cx + radius / 3, cy - radius + head_r),
        Point::new(cx - radius / 4, cy + radius / 3),
    ).into_styled(stroke).draw(display).ok();
    // Front leg.
    Line::new(
        Point::new(cx - radius / 4, cy + radius / 3),
        Point::new(cx + radius / 3, cy + radius),
    ).into_styled(stroke).draw(display).ok();
    // Back leg.
    Line::new(
        Point::new(cx - radius / 4, cy + radius / 3),
        Point::new(cx - radius, cy + radius),
    ).into_styled(stroke).draw(display).ok();
    // Front arm.
    Line::new(
        Point::new(cx + radius / 6, cy - radius / 3),
        Point::new(cx + radius, cy - radius / 4),
    ).into_styled(stroke).draw(display).ok();
    // Back arm.
    Line::new(
        Point::new(cx, cy - radius / 4),
        Point::new(cx - radius / 2, cy),
    ).into_styled(stroke).draw(display).ok();
}

/// Skull glyph: outlined dome with two filled circular eye sockets
/// and three short rectangular teeth poking below. Tuned to read at
/// a 16 px tile radius - eye dots are large enough not to get lost,
/// teeth are spaced so they clearly separate.
pub fn skull<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    let fill = PrimitiveStyle::with_fill(color);
    let r = radius;

    // Dome: outlined circle, centered above the glyph midline so
    // there is vertical room for teeth.
    let dome_cy = cy - r / 3;
    Circle::with_center(Point::new(cx, dome_cy), (r * 2 - 2) as u32)
        .into_styled(stroke).draw(display).ok();

    // Eyes: two filled circular sockets (~1/3 of the dome radius).
    let eye_r = (r / 3).max(2);
    let eye_off = r / 2;
    Circle::with_center(Point::new(cx - eye_off, dome_cy - 1), (eye_r * 2) as u32)
        .into_styled(fill).draw(display).ok();
    Circle::with_center(Point::new(cx + eye_off, dome_cy - 1), (eye_r * 2) as u32)
        .into_styled(fill).draw(display).ok();

    // Teeth: 3 short filled rectangles stepping below the dome,
    // spaced wider than the eye pair so the jaw reads clearly.
    let dome_bottom = dome_cy + r - 1;
    let tooth_w = 2i32;
    let tooth_h = r / 2 + 1;
    for dx in [-4, 0, 4] {
        Rectangle::new(
            Point::new(cx + dx - tooth_w / 2, dome_bottom),
            Size::new(tooth_w as u32, tooth_h as u32),
        ).into_styled(fill).draw(display).ok();
    }
}

/// Battery glyph: horizontal rounded body with a small nub on the
/// right. Outline only.
pub fn battery<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    let fill = PrimitiveStyle::with_fill(color);
    let w = radius * 2;
    let h = radius;
    let x = cx - w / 2;
    let y = cy - h / 2;

    Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32))
        .into_styled(stroke).draw(display).ok();
    // Nub on the right side.
    let nub_w = 3i32;
    let nub_h = (h / 2).max(4);
    Rectangle::new(
        Point::new(x + w, y + (h - nub_h) / 2),
        Size::new(nub_w as u32, nub_h as u32),
    ).into_styled(fill).draw(display).ok();
}

/// IMU / 6-axis motion-sensor glyph: a circle with three axis lines
/// crossing its center (horizontal, vertical, and a diagonal "Z"
/// suggestion). Reads as a gyro / motion sensor affordance.
pub fn imu<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    let r = radius;
    Circle::with_center(Point::new(cx, cy), (r * 2) as u32)
        .into_styled(stroke).draw(display).ok();
    // X axis.
    Line::new(Point::new(cx - r, cy), Point::new(cx + r, cy))
        .into_styled(stroke).draw(display).ok();
    // Y axis.
    Line::new(Point::new(cx, cy - r), Point::new(cx, cy + r))
        .into_styled(stroke).draw(display).ok();
    // Z axis (diagonal, shorter so it doesn't overlap the circle).
    let d = r * 2 / 3;
    Line::new(Point::new(cx - d, cy + d), Point::new(cx + d, cy - d))
        .into_styled(stroke).draw(display).ok();
}

/// IC-chip glyph: a DIP-style rectangular body (taller than wide)
/// with short pin stubs on the left and right edges (3 each side).
/// Used as the flash storage icon.
pub fn chip<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    let fill = PrimitiveStyle::with_fill(color);
    // Body is narrower than it is tall - reads as a DIP package
    // rather than a generic square.
    let w = radius * 5 / 4;
    let h = radius * 2;
    let x = cx - w / 2;
    let y = cy - h / 2;

    // Body outline.
    Rectangle::new(Point::new(x, y), Size::new(w as u32, h as u32))
        .into_styled(stroke).draw(display).ok();

    // Pin stubs: 3 on each side, evenly spaced.
    let pin_w = 3i32;
    let pin_h = 2i32;
    let step = h / 4;
    for i in 1..=3 {
        let py = y + step * i - pin_h / 2;
        // Left pins.
        Rectangle::new(
            Point::new(x - pin_w, py),
            Size::new(pin_w as u32, pin_h as u32),
        ).into_styled(fill).draw(display).ok();
        // Right pins.
        Rectangle::new(
            Point::new(x + w, py),
            Size::new(pin_w as u32, pin_h as u32),
        ).into_styled(fill).draw(display).ok();
    }
}

/// SD card glyph: vertical rectangle with the top-right corner
/// chamfered (the orientation notch) plus three small contact stubs
/// near the top. Distinct from the chip glyph so flash + SD read
/// as different media.
pub fn sd_card<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    let fill = PrimitiveStyle::with_fill(color);
    let w = (radius * 3 / 2).max(10);
    let h = radius * 2;
    let x = cx - w / 2;
    let y = cy - h / 2;
    let notch = 4;

    // 5-line outline traced clockwise from top-left, with the
    // top-right corner replaced by a diagonal chamfer.
    Line::new(Point::new(x, y), Point::new(x + w - notch, y))
        .into_styled(stroke).draw(display).ok();
    Line::new(Point::new(x + w - notch, y), Point::new(x + w, y + notch))
        .into_styled(stroke).draw(display).ok();
    Line::new(Point::new(x + w, y + notch), Point::new(x + w, y + h))
        .into_styled(stroke).draw(display).ok();
    Line::new(Point::new(x + w, y + h), Point::new(x, y + h))
        .into_styled(stroke).draw(display).ok();
    Line::new(Point::new(x, y + h), Point::new(x, y))
        .into_styled(stroke).draw(display).ok();

    // Three tiny contact stubs near the top.
    for i in 0..3 {
        let px = x + 2 + i * 3;
        Rectangle::new(
            Point::new(px, y + notch + 2),
            Size::new(2, 3),
        ).into_styled(fill).draw(display).ok();
    }
}

/// Moon glyph: crescent shape drawn as a filled circle with a
/// smaller offset circle cut out (in the display's background color,
/// which is always `#000` on AMOLED). Used as the Night Mode icon.
pub fn moon<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let fill = PrimitiveStyle::with_fill(color);
    let carve = PrimitiveStyle::with_fill(Color::new(0, 0, 0));

    // Base disc.
    Circle::with_center(Point::new(cx, cy), (radius * 2) as u32)
        .into_styled(fill).draw(display).ok();
    // Offset "bite" to create the crescent. The offset is deliberate:
    // up-right so the crescent opens to the lower-left, matching the
    // classic night-mode moon shape.
    Circle::with_center(
        Point::new(cx + radius / 2, cy - radius / 3),
        (radius * 2) as u32,
    ).into_styled(carve).draw(display).ok();
}

// -- Status-bar mini glyphs --------------------------------------------------
//
// Tiny 10 px variants drawn for the 18 px top status bar. The regular
// signal / bluetooth glyphs (if/when added in the future) would size
// for tile use; these two stay small and readable at ~10 px.

/// Signal-strength glyph: 3 ascending vertical bars. Reads at ~10 px.
/// `radius` is half the glyph's visible size (so a 10 px icon is
/// radius = 5).
pub fn signal_small<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let fill = PrimitiveStyle::with_fill(color);
    let bar_w = 2i32;
    let gap = 1i32;
    let total_w = 3 * bar_w + 2 * gap;
    let start_x = cx - total_w / 2;
    let base_y = cy + radius;
    let heights = [radius, radius * 3 / 2, radius * 2];
    for (i, h) in heights.iter().enumerate() {
        let x = start_x + i as i32 * (bar_w + gap);
        let y = base_y - *h;
        Rectangle::new(Point::new(x, y), Size::new(bar_w as u32, *h as u32))
            .into_styled(fill).draw(display).ok();
    }
}

/// Bluetooth glyph: stylised "B" rune formed by a vertical spine
/// and two pairs of diagonals creating the upper and lower lobes.
/// Readable at ~10 px.
pub fn bluetooth_small<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 1);
    let r = radius;
    // Vertical spine.
    Line::new(Point::new(cx, cy - r), Point::new(cx, cy + r))
        .into_styled(stroke).draw(display).ok();
    // Upper lobe: (cx-r, cy-r/2) -> (cx+r/2, cy-r) -> (cx, cy)
    Line::new(Point::new(cx - r, cy - r / 2), Point::new(cx + r / 2, cy - r))
        .into_styled(stroke).draw(display).ok();
    Line::new(Point::new(cx + r / 2, cy - r), Point::new(cx, cy))
        .into_styled(stroke).draw(display).ok();
    // Lower lobe: (cx-r, cy+r/2) -> (cx+r/2, cy+r) -> (cx, cy)
    Line::new(Point::new(cx - r, cy + r / 2), Point::new(cx + r / 2, cy + r))
        .into_styled(stroke).draw(display).ok();
    Line::new(Point::new(cx + r / 2, cy + r), Point::new(cx, cy))
        .into_styled(stroke).draw(display).ok();
}

/// Lightning-bolt glyph: classic filled zigzag. Used as the flash /
/// flashlight icon.
pub fn bolt<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    // 3-segment zigzag from upper-right down through middle-left,
    // middle-right, to lower-left apex.
    let r = radius;
    Line::new(Point::new(cx + r / 2, cy - r), Point::new(cx - r / 2, cy))
        .into_styled(stroke).draw(display).ok();
    Line::new(Point::new(cx - r / 2, cy), Point::new(cx + r / 2, cy))
        .into_styled(stroke).draw(display).ok();
    Line::new(Point::new(cx + r / 2, cy), Point::new(cx - r / 2, cy + r))
        .into_styled(stroke).draw(display).ok();
}

/// Lock glyph: padlock body (filled rect) with a shackle arc above
/// drawn as three line segments approximating a half-circle.
pub fn lock<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    let fill = PrimitiveStyle::with_fill(color);

    // Body: filled rectangle in the lower half.
    let body_w = radius * 7 / 4;
    let body_h = radius * 5 / 4;
    let body_x = cx - body_w / 2;
    let body_y = cy;
    Rectangle::new(
        Point::new(body_x, body_y),
        Size::new(body_w as u32, body_h as u32),
    ).into_styled(fill).draw(display).ok();

    // Shackle: three strokes forming a U inverted above the body.
    let sh_hw = radius * 3 / 4;
    let sh_top = cy - radius * 3 / 2;
    // Left vertical.
    Line::new(
        Point::new(cx - sh_hw, cy),
        Point::new(cx - sh_hw, sh_top + 2),
    ).into_styled(stroke).draw(display).ok();
    // Top curve approximated as a horizontal line.
    Line::new(
        Point::new(cx - sh_hw, sh_top + 2),
        Point::new(cx + sh_hw, sh_top + 2),
    ).into_styled(stroke).draw(display).ok();
    // Right vertical.
    Line::new(
        Point::new(cx + sh_hw, sh_top + 2),
        Point::new(cx + sh_hw, cy),
    ).into_styled(stroke).draw(display).ok();
}

/// Do-not-disturb glyph: outlined circle with a diagonal slash from
/// upper-left to lower-right. Universal "no" symbol; works as
/// shorthand at row-icon scale (~16 px) without needing a literal
/// bell-with-slash composition.
pub fn dnd<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    Circle::with_center(Point::new(cx, cy), (radius * 2) as u32)
        .into_styled(stroke).draw(display).ok();
    Line::new(
        Point::new(cx - radius * 7 / 10, cy + radius * 7 / 10),
        Point::new(cx + radius * 7 / 10, cy - radius * 7 / 10),
    ).into_styled(stroke).draw(display).ok();
}

/// Power glyph: outlined circle with the top arc broken, plus a
/// short vertical line in the gap. IEC standby symbol.
pub fn power<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    // Approximate the broken-top circle with a full circle - at
    // row-icon scale the overlaid vertical line reads as the gap.
    Circle::with_center(Point::new(cx, cy + 1), (radius * 2 - 2) as u32)
        .into_styled(stroke).draw(display).ok();
    // Vertical bar from above the circle into its top, drawn last
    // so it covers the circle stroke at the top.
    Line::new(
        Point::new(cx, cy - radius - 1),
        Point::new(cx, cy + 1),
    ).into_styled(PrimitiveStyle::with_stroke(color, 3))
        .draw(display).ok();
}

/// Zigbee glyph: hexagon outline. Six lines tracing a regular
/// hexagon, the classic Zigbee branding shape and a clean way to
/// signal "mesh network" at row-icon scale.
pub fn zigbee<D: BlendTarget>(
    display: &mut D, cx: i32, cy: i32, radius: i32, color: Color,
) {
    let stroke = PrimitiveStyle::with_stroke(color, 2);
    let half_w = radius * 7 / 8;
    let qtr_h  = radius / 2;
    let pts = [
        (cx,            cy - radius),
        (cx + half_w,   cy - qtr_h),
        (cx + half_w,   cy + qtr_h),
        (cx,            cy + radius),
        (cx - half_w,   cy + qtr_h),
        (cx - half_w,   cy - qtr_h),
    ];
    for i in 0..6 {
        let (x1, y1) = pts[i];
        let (x2, y2) = pts[(i + 1) % 6];
        Line::new(Point::new(x1, y1), Point::new(x2, y2))
            .into_styled(stroke).draw(display).ok();
    }
}
