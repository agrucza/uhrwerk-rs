//! Battery sub-view: live percent/voltage with a charge ring, the
//! history sparkline drawn from the flash event log, and the
//! UPTIME / ACTIVE / SLEEPS diagnostic panels.

use embedded_graphics::{
    geometry::{Point, Size},
    primitives::{Line, PrimitiveStyle, Rectangle, StyledDrawable},
};
use crate::ui::types::BlendTarget;
use heapless::String;
use core::fmt::Write;

use crate::ui::{fmt, fonts, theme};
use crate::ui::types::{Action, RenderCtx, SystemData, SystemEvent};
use crate::ui::widgets::{chamfered_panel, ring_gauge, tag_label, NOTCH};

use super::{draw_header, header_back_hit, rows_top, SettingsScreen, SettingsView};

/// Inline value on the settings index's BATTERY row: the live charge
/// percent, or "--" before the first reading.
pub(super) fn index_value(data: &SystemData) -> String<20> {
    let mut buf = String::new();
    match data.power.battery_percent {
        Some(pct) => { let _ = write!(buf, "{}%", pct); }
        None      => { let _ = buf.push_str("--"); }
    }
    buf
}

impl SettingsScreen {
    pub(super) fn render_battery<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "BATTERY", theme::ACCENT, ctx);

        // Top: chamfered tag-labeled BATTERY panel with live
        // percent/voltage centered inside.
        let panel_w = theme::SCREEN_W as i32 - 56;
        let panel_x = (theme::SCREEN_W as i32 - panel_w) / 2;
        let panel_y = rows_top(&data.safe_area) + 18;
        let panel_h = 60i32;
        let panel_rect = Rectangle::new(
            Point::new(panel_x, panel_y),
            Size::new(panel_w as u32, panel_h as u32),
        );
        chamfered_panel(display, panel_rect, NOTCH, theme::ACCENT, 1);
        tag_label(
            display,
            panel_rect.top_left.x,
            panel_rect.top_left.y,
            "NOW",
            theme::ACCENT,
            NOTCH,
        );

        // Charge ring on the panel's left: sweep = battery percent,
        // color follows the same health palette as the battery icon.
        // Sized/positioned to clear the NOW tag in the TL corner.
        let ring_r = 18i32;
        let ring_cx = panel_x + 52;
        let ring_cy = panel_y + panel_h / 2 + 4;
        let pct = data.power.battery_percent;
        ring_gauge(
            display, ctx,
            ring_cx, ring_cy, ring_r, 5,
            pct.unwrap_or(0) as u32, 100,
            crate::ui::primitives::battery_color(pct.unwrap_or(100)),
            Some(theme::SURFACE_3),
        );

        let mut val: String<20> = String::new();
        match (pct, data.power.battery_voltage_mv) {
            (Some(p), Some(mv)) => {
                let _ = write!(val, "{}% / {}.{:02}V", p, mv / 1000, (mv % 1000) / 10);
            }
            (Some(p), None) => { let _ = write!(val, "{}%", p); }
            _               => { let _ = val.push_str("--"); }
        }
        // Value text centers in the panel area right of the ring.
        let text_rect = Rectangle::new(
            Point::new(panel_x + 80, panel_y),
            Size::new((panel_w - 88) as u32, panel_h as u32),
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(),
            val.as_str(), text_rect, theme::FG,
        );

        // Sparkline: full screen width, edge-to-edge, no card around.
        let graph_y = panel_y + panel_h + 14;
        let graph_h = 96i32;
        let graph_rect = Rectangle::new(
            Point::new(0, graph_y),
            Size::new(theme::SCREEN_W as u32, graph_h as u32),
        );
        draw_battery_sparkline(display, graph_rect, &data.battery_history);

        // Below the sparkline: UPTIME (wall-time since power-on, from
        // the SoC RTC counter - survives light sleep) and below that
        // ACTIVE (embassy time since boot - pauses during light
        // sleep). Together they let the user read off duty cycle:
        // active / uptime ~= fraction of time the chip was awake.
        let uptime_y = graph_y + graph_h + 14;
        let uptime_rect = Rectangle::new(
            Point::new(panel_x, uptime_y),
            Size::new(panel_w as u32, panel_h as u32),
        );
        chamfered_panel(display, uptime_rect, NOTCH, theme::INFO, 1);
        tag_label(
            display,
            uptime_rect.top_left.x,
            uptime_rect.top_left.y,
            "UPTIME",
            theme::INFO,
            NOTCH,
        );
        let up_buf = fmt::hms(data.uptime_secs as u64);
        fonts::draw_centered_in_rect(
            display, &fonts::value(),
            up_buf.as_str(), uptime_rect, theme::FG,
        );

        let active_y = uptime_y + panel_h + 12;
        let active_rect = Rectangle::new(
            Point::new(panel_x, active_y),
            Size::new(panel_w as u32, panel_h as u32),
        );
        chamfered_panel(display, active_rect, NOTCH, theme::INFO, 1);
        tag_label(
            display,
            active_rect.top_left.x,
            active_rect.top_left.y,
            "ACTIVE",
            theme::INFO,
            NOTCH,
        );
        let act_buf = fmt::hms(data.active_secs as u64);
        fonts::draw_centered_in_rect(
            display, &fonts::value(),
            act_buf.as_str(), active_rect, theme::FG,
        );

        // SLEEPS: count of completed light-sleep cycles. Diagnostic -
        // paced by the ~5 s heartbeat when really sleeping (~12/min); a
        // far higher rate vs UPTIME means the CPU is instant-waking
        // instead of gating off. 4th panel below ACTIVE, same geometry;
        // scroll down to see it.
        let slept_y = active_y + panel_h + 12;
        let slept_rect = Rectangle::new(
            Point::new(panel_x, slept_y),
            Size::new(panel_w as u32, panel_h as u32),
        );
        chamfered_panel(display, slept_rect, NOTCH, theme::INFO, 1);
        tag_label(
            display,
            slept_rect.top_left.x,
            slept_rect.top_left.y,
            "SLEEPS",
            theme::INFO,
            NOTCH,
        );
        let mut slept_buf: String<16> = String::new();
        let _ = write!(slept_buf, "{}", data.sleep_cycles);
        fonts::draw_centered_in_rect(
            display, &fonts::value(),
            slept_buf.as_str(), slept_rect, theme::FG,
        );
    }

    pub(super) fn battery_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y, &data.safe_area) => {
                self.view = SettingsView::Index;
                Action::Redraw
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Index;
                Action::Redraw
            }
            // Any live snapshot refresh or new sample should repaint.
            SystemEvent::PowerUpdated { .. }
            | SystemEvent::BatteryChanged { .. }
            | SystemEvent::TimeUpdated { .. } => Action::Redraw,
            _ => Action::None,
        }
    }
}

// -- Battery-graph helpers ---------------------------------------------------

/// Render the battery history as an edge-to-edge polyline inside
/// `rect`. Draws faint horizontal gridlines at 25/50/75% and
/// connects consecutive samples with short segments in the battery
/// color. Empty history gets a centered "NO DATA" caption.
///
/// `rect` is the full sparkline area (no surrounding card); the
/// polyline insets by a small horizontal margin so endpoints don't
/// land at the screen edge but is otherwise full width.
fn draw_battery_sparkline<D: BlendTarget>(
    display: &mut D,
    rect: Rectangle,
    history: &crate::data::BatteryHistory,
) {
    // Small horizontal inset so the leftmost / rightmost samples
    // don't sit at the bezel arc; vertical inset is just visual
    // breathing room.
    const H_INSET: i32 = 24;
    const V_INSET: i32 = 6;
    let plot = Rectangle::new(
        Point::new(rect.top_left.x + H_INSET, rect.top_left.y + V_INSET),
        Size::new(
            (rect.size.width as i32 - 2 * H_INSET) as u32,
            (rect.size.height as i32 - 2 * V_INSET) as u32,
        ),
    );

    // Horizontal gridlines at 25 / 50 / 75 percent.
    let grid_style = PrimitiveStyle::with_stroke(theme::FG_DIM, 1);
    let left  = plot.top_left.x;
    let right = plot.top_left.x + plot.size.width as i32;
    for pct in [25, 50, 75] {
        let y = plot_y(pct, &plot);
        let _ = Line::new(Point::new(left, y), Point::new(right, y))
            .draw_styled(&grid_style, display);
    }

    // Empty state: centered caption.
    if history.is_empty() {
        fonts::draw_centered_in_rect(
            display, &fonts::body(), "NO DATA", plot, theme::FG_DIM,
        );
        return;
    }

    // Map each sample to a screen point, oldest on the left. When
    // there's only one sample the polyline has no segments - draw
    // a single-pixel dot via a length-1 Line so the view still
    // shows "there is data here."
    let n = history.len();
    let width = plot.size.width as i32;
    let sample_point = |i: usize, pct: u8| -> Point {
        let x = if n <= 1 {
            plot.top_left.x + width / 2
        } else {
            plot.top_left.x + (i as i32 * width) / (n as i32 - 1)
        };
        Point::new(x, plot_y(pct, &plot))
    };

    // Color each segment by the *lower* of its two endpoint
    // percents, so the line turns yellow / red the instant it drops
    // into a warning band. Matches the palette `battery_color` uses
    // for the battery icon elsewhere in the UI.
    let mut prev: Option<(Point, u8)> = None;
    for (i, sample) in history.iter().enumerate() {
        let p = sample_point(i, sample.percent);
        if let Some((q, prev_pct)) = prev {
            let color = crate::ui::primitives::battery_color(prev_pct.min(sample.percent));
            let stroke = PrimitiveStyle::with_stroke(color, 2);
            let _ = Line::new(q, p).draw_styled(&stroke, display);
        } else if n == 1 {
            // Single sample: draw it as a zero-length line so the
            // stroke still renders a small marker.
            let color = crate::ui::primitives::battery_color(sample.percent);
            let stroke = PrimitiveStyle::with_stroke(color, 2);
            let _ = Line::new(p, p).draw_styled(&stroke, display);
        }
        prev = Some((p, sample.percent));
    }
}

/// Map a battery percent (0-100, clamped) to the pixel Y inside
/// `plot`. 100% sits at the top edge, 0% at the bottom edge.
fn plot_y(percent: u8, plot: &Rectangle) -> i32 {
    let p = percent.min(100) as i32;
    let h = plot.size.height as i32;
    plot.top_left.y + (100 - p) * h / 100
}
