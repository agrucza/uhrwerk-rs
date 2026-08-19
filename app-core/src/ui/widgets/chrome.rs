//! Chrome widgets - screen-level decorations.
//!
//! Chrome is stuff that belongs to a screen's outer frame rather than
//! its content: top status bar, title headers, back chevron hit zones,
//! page-indicator scrollbars, bottom home indicators.
//!
//! * **Nightwatch `header`** - chevron-back + accent title + right
//!   telemetry + 1 px hairline underline. The standard app header.
//! * **`status_bar`** + **`home_indicator`** - the 18 px top strip
//!   and the 2 px bottom bar drawn on every non-watch-face screen.
//! * **`draw_app_chrome`** - convenience helper that renders all
//!   three together with a single accent + telemetry argument set,
//!   used by every full-app screen.

use embedded_graphics::{
    geometry::{Point, Size},
    prelude::Primitive,
    primitives::{Line, PrimitiveStyle, Rectangle},
    Drawable,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;

use crate::ui::{fonts, glyphs, theme};
use crate::ui::types::SystemData;

// -- Nightwatch header constants ---------------------------------------------

/// Height of the Nightwatch header. Titles and telemetry centre
/// vertically on this.
pub const HEADER_H: i32 = 28;

/// Hit-target width for the Nightwatch `header` back chevron.
pub const HEADER_ICON_HIT_W: i32 = 110;

/// Vertical slack on the Nightwatch header hit zone.
pub const HEADER_ICON_HIT_V_SLACK: i32 = 12;

// -- Status bar + home indicator constants -----------------------------------

/// Height of the top status bar drawn on every non-watch-face screen.
pub const STATUS_BAR_H: i32 = 18;

/// Width of the home-indicator bar.
pub const HOME_INDICATOR_W: i32 = 56;

/// Height (thickness) of the home-indicator bar.
pub const HOME_INDICATOR_H: i32 = 2;

// -- Nightwatch header -------------------------------------------------------

/// Draw the Nightwatch screen header:
/// ```text
/// [chevron-left]  TITLE .............. telemetry
/// ───────────────────────────────────────────────
/// ```
/// The hairline sits on the bottom pixel of the rect so a screen can
/// line content up directly below.
pub fn header<D: BlendTarget>(
    display: &mut D,
    rect: Rectangle,
    title: &str,
    right_text: &str,
    accent: Color,
) {
    let x = rect.top_left.x;
    let y = rect.top_left.y;
    let w = rect.size.width as i32;
    let h = rect.size.height as i32;
    // Horizontal pad inside the header rect. 24 keeps the chevron's
    // leftmost pixel (at cx - 4 = 26) clear of the bezel arc at the
    // header's y-band, and widens title/right-telemetry breathing room.
    let pad = 24i32;

    let cy = y + h / 2;
    let cx = x + pad + 6;
    let stroke = PrimitiveStyle::with_stroke(accent, 2);
    Line::new(
        Point::new(cx + 4, cy - 6),
        Point::new(cx - 4, cy),
    ).into_styled(stroke).draw(display).ok();
    Line::new(
        Point::new(cx - 4, cy),
        Point::new(cx + 4, cy + 6),
    ).into_styled(stroke).draw(display).ok();

    let title_font = fonts::value();
    let title_h = fonts::measure_bbox(&title_font, title)
        .map(|b| b.size.height as i32)
        .unwrap_or(18);
    let title_top = y + (h - title_h) / 2;
    let title_x = x + pad + 26;
    fonts::draw_at(
        display, &title_font, title,
        title_x, title_top,
        accent,
    );

    // Right telemetry yields to a long title: skip it when the title
    // run would collide (title + breathing gap reaches the telemetry's
    // left edge). The title is the header's identity; the telemetry is
    // decoration.
    let tele_font = fonts::caption();
    let tele_left = x + w - pad - fonts::measure_width(&tele_font, right_text);
    if title_x + fonts::measure_width(&title_font, title) + 12 <= tele_left {
        fonts::draw_right(
            display, &tele_font, right_text,
            x + w - pad, y + h - 12,
            theme::FG_MUTED,
        );
    }

    Line::new(
        Point::new(x, y + h - 1),
        Point::new(x + w - 1, y + h - 1),
    ).into_styled(PrimitiveStyle::with_stroke(accent, 1))
    .draw(display).ok();
}

/// Shrink a header rect clear of the case's top corner arcs at the
/// rect's own height (no-op on boards without corner data). Every
/// `header` call site routes its rect through this so the chevron
/// and right telemetry keep their padding relative to the *visible*
/// glass instead of the panel edge.
pub fn corner_safe_header_rect(
    rect: Rectangle,
    safe: &crate::data::SafeArea,
) -> Rectangle {
    let mid_y = rect.top_left.y + rect.size.height as i32 / 2;
    let panel_h = theme::SCREEN_H as i32;
    let li = safe.left_inset_at(mid_y, panel_h);
    let ri = safe.right_inset_at(mid_y, panel_h);
    Rectangle::new(
        Point::new(rect.top_left.x + li, rect.top_left.y),
        Size::new((rect.size.width as i32 - li - ri).max(0) as u32, rect.size.height),
    )
}

/// Returns `true` if `(x, y)` lands inside the back-chevron hit zone
/// of a Nightwatch `header` drawn at `header_rect`. Zone is wider
/// and taller than the visible chevron so finger pads don't have to
/// land precisely.
pub fn header_icon_hit(x: u16, y: u16, header_rect: Rectangle) -> bool {
    let px = x as i32;
    let py = y as i32;
    let hx = header_rect.top_left.x;
    let hy = header_rect.top_left.y;
    let hh = header_rect.size.height as i32;
    px >= hx && px < hx + HEADER_ICON_HIT_W
        && py >= hy - HEADER_ICON_HIT_V_SLACK
        && py < hy + hh + HEADER_ICON_HIT_V_SLACK
}

// -- status_bar --------------------------------------------------------------

/// Draw the top status bar: `HH:MM` on the left, then signal /
/// bluetooth / battery% on the right. Everything drawn in `tint`
/// (signal default, cyan on Quick Access, yellow on Notifications, ...).
///
/// A 1 px hairline in `tint` runs along the bottom so the bar reads
/// as separated from screen content below. `x_inset` is the design
/// inset for the left/right content; the bar sits at the very top of
/// the corner arcs, so both sides additionally widen to whatever the
/// device's safe area demands at the text row (no-op on zero-inset
/// boards). The bar itself spans full screen width so the hairline
/// reaches edge to edge.
pub fn status_bar<D: BlendTarget>(
    display: &mut D,
    y: i32,
    time: &crate::data::TimeData,
    battery_pct: Option<u8>,
    tint: Color,
    x_inset: i32,
    safe: &crate::data::SafeArea,
) {
    use core::fmt::Write;
    let screen_w = theme::SCREEN_W as i32;
    let h = STATUS_BAR_H;
    let cy = y + h / 2;
    let panel_h = theme::SCREEN_H as i32;
    // Text row center: text top is y + 3, caption ink is ~10 px.
    let ink_mid = y + 8;
    // Angle margin beyond the straight-on corner model: the case lip
    // sweeps inward at wrist viewing angles, so the status bar needs
    // more clearance than left_pad's 2 px. Calibrated on-wrist via
    // the 2026-08-16 ruler flash. No-op on zero-inset boards.
    const STATUS_ANGLE_MARGIN: i32 = 8;
    let left_x =
        x_inset.max(safe.left_inset_at(ink_mid, panel_h) + STATUS_ANGLE_MARGIN);
    let right_x = screen_w
        - x_inset.max(safe.right_inset_at(ink_mid, panel_h) + STATUS_ANGLE_MARGIN);

    let font = fonts::caption();
    fonts::draw_at(
        display, &font, crate::ui::fmt::hm(time.hour, time.minute).as_str(),
        left_x, y + 3,
        tint,
    );

    let gap = 5i32;
    let icon_r = 4i32;
    let mut buf: heapless::String<8> = heapless::String::new();
    if let Some(pct) = battery_pct {
        let _ = write!(buf, "{}%", pct);
    } else {
        let _ = buf.push_str("--");
    }
    let pct_w = fonts::measure_width(&font, buf.as_str());

    // Low battery takes over the readout's color - THE system-wide
    // low-battery indicator on every status-bar screen (the clock
    // face mirrors it in its telemetry strip). A bezel ring was
    // tried and dropped: no arc can hug the watch's case aperture
    // without corner content crossing it.
    let pct_color = match battery_pct {
        Some(p) if p < 10 => theme::DANGER,
        Some(p) if p < 20 => theme::WARN,
        _ => tint,
    };
    fonts::draw_at(
        display, &font, buf.as_str(),
        right_x - pct_w, y + 3,
        pct_color,
    );

    let bt_cx = right_x - pct_w - gap - icon_r;
    glyphs::bluetooth_small(display, bt_cx, cy, icon_r, tint);

    let sig_cx = bt_cx - icon_r * 2 - gap;
    glyphs::signal_small(display, sig_cx, cy, icon_r, tint);

    Line::new(
        Point::new(0, y + h - 1),
        Point::new(screen_w - 1, y + h - 1),
    ).into_styled(PrimitiveStyle::with_stroke(tint, 1))
    .draw(display).ok();
}

// -- home_indicator ----------------------------------------------------------

/// Draw the bottom home-indicator bar - a short, thin signal-colored
/// line centered horizontally at `y`. Every full-screen app / overlay
/// uses this as a passive "base of the screen" marker.
pub fn home_indicator<D: BlendTarget>(
    display: &mut D,
    y: i32,
    tint: Color,
) {
    let cx = theme::SCREEN_W as i32 / 2;
    Rectangle::new(
        Point::new(cx - HOME_INDICATOR_W / 2, y),
        Size::new(HOME_INDICATOR_W as u32, HOME_INDICATOR_H as u32),
    )
    .into_styled(PrimitiveStyle::with_fill(tint))
    .draw(display).ok();
}

// -- Shared app chrome -------------------------------------------------------
//
// THE full-app chrome path - every full-app screen (settings,
// stopwatch, timer, alarm, ...) draws its status bar + header + home
// indicator through `draw_app_chrome`, nothing else. Overlays with
// deliberately different chrome (app drawer, quick access,
// notifications) compose the primitives directly, but every
// edge-adjacent placement must go through the safe-area helpers
// (`data.safe_area.*_inset_at`, `corner_safe_header_rect`). One
// path per visual concept - a cross-cutting change (like the
// safe-area seam) must never have to chase per-screen copies again.

/// Overlay chrome - the counterpart of [`draw_app_chrome`] for the
/// overlay screens (app drawer, quick access): tinted status bar,
/// centered title with a muted centered caption beneath it on the
/// shared [`overlay_title_y`] band, and the standard home
/// indicator. No chevron header, no hairline - that's the overlays'
/// deliberate look. Centered because overlays are transient sheets,
/// not apps: no back chevron to anchor a left edge, and the center
/// is immune to the case corner arcs on every board.
pub fn draw_overlay_chrome<D: BlendTarget>(
    display: &mut D,
    data: &SystemData,
    title: &str,
    caption: &str,
    tint: Color,
    ctx: &crate::ui::types::RenderCtx,
) {
    let top = data.safe_area.top;
    if ctx.intersects_y(APP_STATUS_Y + top, APP_STATUS_Y + top + STATUS_BAR_H) {
        status_bar(
            display,
            APP_STATUS_Y + top,
            &data.time,
            data.power.battery_percent,
            tint,
            APP_STATUS_X_INSET,
            &data.safe_area,
        );
    }
    // Gate spans the full line boxes: title ink starts at
    // title_y - 8, the caption's line box (ascent 10 + descent 3
    // from title_y + 22) ends at title_y + 35.
    let title_y = overlay_title_y(&data.safe_area);
    if ctx.intersects_y(title_y - 10, title_y + 36) {
        let cx = theme::SCREEN_W as i32 / 2;
        fonts::draw_centered(
            display, &fonts::value(), title,
            cx, title_y - 8,
            tint,
        );
        fonts::draw_centered(
            display, &fonts::caption(), caption,
            cx, title_y + 22,
            theme::FG_MUTED,
        );
    }
    let bar_y = app_home_bar_y(&data.safe_area);
    if ctx.intersects_y(bar_y, bar_y + HOME_INDICATOR_H) {
        home_indicator(display, bar_y, theme::ACCENT);
    }
}

/// Y of the top status bar in standard app chrome (before the
/// per-device top inset is applied).
pub const APP_STATUS_Y: i32 = 0;

// The rest of the top stack DERIVES from the device's safe area:
// the case lip is a raised wall, so its occlusion moves with viewing
// angle - the declared insets are comfort values for real angles,
// and everything below the status bar follows them. Boards with a
// zero safe area get exactly the legacy constants.

/// Top of the app header bar: status bar (shifted by the top inset)
/// plus an 8 px gap.
pub fn app_header_top(safe: &crate::data::SafeArea) -> i32 {
    APP_STATUS_Y + safe.top + STATUS_BAR_H + 8
}

/// First content row below the app header.
pub fn app_content_top(safe: &crate::data::SafeArea) -> i32 {
    app_header_top(safe) + HEADER_H + 8
}

/// Y of the bottom home-indicator bar, lifted by the bottom inset.
pub fn app_home_bar_y(safe: &crate::data::SafeArea) -> i32 {
    theme::SCREEN_H as i32 - 18 - safe.bottom
}

/// Full-width scroll viewport from `top` down to 4 px above the
/// home-indicator bar - the shared shape of every scrolling list's
/// clip rect (alarm list, notifications, settings index/motion).
pub fn viewport_to_home_bar(
    top: i32,
    safe: &crate::data::SafeArea,
) -> Rectangle {
    let bot = app_home_bar_y(safe) - 4;
    Rectangle::new(
        Point::new(0, top),
        Size::new(theme::SCREEN_W as u32, (bot - top) as u32),
    )
}

/// Anchor row of an overlay's title band (drawer, quick access):
/// title ink top sits at `overlay_title_y - 8`, the right caption's
/// top at `overlay_title_y`.
pub fn overlay_title_y(safe: &crate::data::SafeArea) -> i32 {
    APP_STATUS_Y + safe.top + STATUS_BAR_H + 26
}

/// Horizontal inset for status-bar content. Picked to keep the time
/// glyph and battery glyphs clear of the bezel arc at the status
/// bar's y-band.
pub const APP_STATUS_X_INSET: i32 = 85;

/// Header rect used by [`draw_app_chrome`] and back-chevron hit
/// testing. Full screen width; the header widget pads its own
/// content away from the bezel arc internally.
pub fn app_header_rect(safe: &crate::data::SafeArea) -> Rectangle {
    Rectangle::new(
        Point::new(0, app_header_top(safe)),
        Size::new(theme::SCREEN_W as u32, HEADER_H as u32),
    )
}

/// Draw the standard app chrome: top status bar tinted by `accent`
/// (live `HH:MM` + battery% read from `data`), Nightwatch header
/// with `title` + `telemetry` text, bottom signal-red home indicator.
///
/// The home indicator is *always* signal-red regardless of the
/// per-screen `accent`, matching the design spec's rule that it's a
/// system-level "base of the screen" marker, not a per-app element.
pub fn draw_app_chrome<D: BlendTarget>(
    display: &mut D,
    data: &SystemData,
    title: &str,
    telemetry: &str,
    accent: Color,
    ctx: &crate::ui::types::RenderCtx,
) {
    // The three pieces sit at fixed y-positions; skip each one's
    // setup work (string format, glyph lookup) when this tile's
    // y-range can't contain it - the driver would reject every
    // write per-pixel anyway.
    let top = data.safe_area.top;
    if ctx.intersects_y(APP_STATUS_Y + top, APP_STATUS_Y + top + STATUS_BAR_H) {
        status_bar(
            display,
            APP_STATUS_Y + top,
            &data.time,
            data.power.battery_percent,
            accent,
            APP_STATUS_X_INSET,
            &data.safe_area,
        );
    }
    let hdr_top = app_header_top(&data.safe_area);
    if ctx.intersects_y(hdr_top, hdr_top + HEADER_H) {
        header(
            display,
            corner_safe_header_rect(app_header_rect(&data.safe_area), &data.safe_area),
            title, telemetry, accent,
        );
    }
    let bar_y = app_home_bar_y(&data.safe_area);
    if ctx.intersects_y(bar_y, bar_y + HOME_INDICATOR_H) {
        home_indicator(display, bar_y, theme::ACCENT);
    }
}

/// Hit test for the back chevron of the standard app chrome.
pub fn app_chrome_back_hit(x: u16, y: u16, safe: &crate::data::SafeArea) -> bool {
    header_icon_hit(x, y, app_header_rect(safe))
}

