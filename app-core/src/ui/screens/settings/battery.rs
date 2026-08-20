//! Battery sub-view: live percent/voltage with a charge ring, the
//! charger phase, the history sparkline drawn from the flash event
//! log, and the UPTIME / ACTIVE / SLEEPS diagnostic panels.
//!
//! Scrolls: the stack outgrew the viewport when CHARGE was added
//! (before that, SLEEPS ended exactly on the home-bar line of the
//! tightest board and a comment here claimed a scroll that did not
//! exist). Same smooth-scroll machinery the MOTION view uses.

use embedded_graphics::{
    geometry::{Point, Size},
    primitives::{Line, PrimitiveStyle, Rectangle, StyledDrawable},
};
use crate::ui::types::BlendTarget;
use heapless::String;
use core::fmt::Write;

use crate::ui::{fmt, fonts, theme, widgets};
use crate::ui::types::{Action, RenderCtx, SystemData, SystemEvent};
use crate::ui::widgets::{
    chamfered_panel, handle_scroll_drag, render_scrolled, ring_gauge, tag_label, NOTCH,
};

use super::{draw_header, header_back_hit, leaf_top_y, rows_top, SettingsScreen, SettingsView};

/// Height of the NOW / CHARGE / UPTIME / ACTIVE / SLEEPS panels.
const PANEL_H: i32 = 60;
/// Height of the history sparkline band.
const GRAPH_H: i32 = 96;

/// Every rect the battery view draws, already shifted by `scroll`.
/// Single source for render (the event side only needs `content_h`).
struct BatterySlots {
    now: Rectangle,
    charge: Rectangle,
    graph: Rectangle,
    uptime: Rectangle,
    active: Rectangle,
    slept: Rectangle,
    /// Total unscrolled height, for the scroll extents.
    content_h: i32,
}

fn battery_slots(scroll: i32, safe: &crate::data::SafeArea) -> BatterySlots {
    let top = leaf_top_y(safe) - scroll;
    let w = theme::SCREEN_W as i32 - 56;
    let x = (theme::SCREEN_W as i32 - w) / 2;
    let panel = |y: i32| {
        Rectangle::new(Point::new(x, y), Size::new(w as u32, PANEL_H as u32))
    };

    let now = panel(top);
    let charge = panel(top + PANEL_H + 12);
    let graph_y = charge.top_left.y + PANEL_H + 14;
    // The sparkline is deliberately edge-to-edge, not inset like the
    // panels.
    let graph = Rectangle::new(
        Point::new(0, graph_y),
        Size::new(theme::SCREEN_W as u32, GRAPH_H as u32),
    );
    let uptime = panel(graph_y + GRAPH_H + 14);
    let active = panel(uptime.top_left.y + PANEL_H + 12);
    let slept = panel(active.top_left.y + PANEL_H + 12);

    let content_h = slept.top_left.y + PANEL_H - top;
    BatterySlots { now, charge, graph, uptime, active, slept, content_h }
}

/// Viewport for the scrolled battery stack.
fn battery_viewport(safe: &crate::data::SafeArea) -> Rectangle {
    widgets::viewport_to_home_bar(rows_top(safe), safe)
}

/// Charger phase as a short uppercase label, plus the tint that says
/// whether it is worth waiting on. DONE is the one that matters for a
/// fuel-gauge anchor charge: it means the charger tapered to the
/// termination current, which is what re-anchors 100%.
fn charge_state(data: &SystemData) -> (&'static str, crate::ui::theme::Color) {
    use drivers::pmu::ChargerPhase;
    match data.power.charger_phase {
        ChargerPhase::TriCharge => ("TRICKLE", theme::WARN),
        ChargerPhase::PreCharge => ("PRE-CHARGE", theme::WARN),
        ChargerPhase::ConstantCurrent => ("CHARGING CC", theme::INFO),
        ChargerPhase::ConstantVoltage => ("CHARGING CV", theme::INFO),
        ChargerPhase::Done => ("CHARGED", theme::OK),
        ChargerPhase::NotCharging if data.power.vbus_good => ("NOT CHARGING", theme::FG_MUTED),
        ChargerPhase::NotCharging => ("ON BATTERY", theme::FG_MUTED),
    }
}

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

        let scroll = self.battery_scroll.offset();
        let slots = battery_slots(scroll, &data.safe_area);
        let pct = data.power.battery_percent;

        render_scrolled(
            display, scroll,
            battery_viewport(&data.safe_area), slots.content_h, theme::ACCENT, ctx,
            |clipped, _| {
                // NOW: chamfered tag-labeled panel with live
                // percent/voltage centered inside.
                chamfered_panel(clipped, slots.now, NOTCH, theme::ACCENT, 1);
                tag_label(
                    clipped,
                    slots.now.top_left.x, slots.now.top_left.y,
                    "NOW", theme::ACCENT, NOTCH,
                );

                // Charge ring on the panel's left: sweep = battery
                // percent, color follows the same health palette as the
                // battery icon. Sized/positioned to clear the NOW tag
                // in the TL corner.
                let ring_r = 18i32;
                let ring_cx = slots.now.top_left.x + 52;
                let ring_cy = slots.now.top_left.y + PANEL_H / 2 + 4;
                ring_gauge(
                    clipped, ctx,
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
                    Point::new(slots.now.top_left.x + 80, slots.now.top_left.y),
                    Size::new((slots.now.size.width as i32 - 88) as u32, PANEL_H as u32),
                );
                fonts::draw_centered_in_rect(
                    clipped, &fonts::value(),
                    val.as_str(), text_rect, theme::FG,
                );

                // CHARGE: what the charger is doing right now. Read
                // straight from the PMU's charger-phase bits, which the
                // power task already polls and event-publishes. CHARGED
                // is the one to wait for when anchoring the fuel gauge:
                // it means the current tapered to the termination
                // threshold, which is what makes 100% mean 100%.
                let (state, tint) = charge_state(data);
                chamfered_panel(clipped, slots.charge, NOTCH, tint, 1);
                tag_label(
                    clipped,
                    slots.charge.top_left.x, slots.charge.top_left.y,
                    "CHARGE", tint, NOTCH,
                );
                fonts::draw_centered_in_rect(
                    clipped, &fonts::value(), state, slots.charge, tint,
                );

                // Sparkline: full screen width, edge-to-edge, no card.
                draw_battery_sparkline(clipped, slots.graph, &data.battery_history);

                // Below the sparkline: UPTIME (wall-time since power-on,
                // from the SoC RTC counter - survives light sleep) and
                // below that ACTIVE (embassy time since boot - pauses
                // during light sleep). Together they let the user read
                // off duty cycle: active / uptime ~= fraction of time
                // the chip was awake.
                chamfered_panel(clipped, slots.uptime, NOTCH, theme::INFO, 1);
                tag_label(
                    clipped,
                    slots.uptime.top_left.x, slots.uptime.top_left.y,
                    "UPTIME", theme::INFO, NOTCH,
                );
                let up_buf = fmt::hms(data.uptime_secs as u64);
                fonts::draw_centered_in_rect(
                    clipped, &fonts::value(),
                    up_buf.as_str(), slots.uptime, theme::FG,
                );

                chamfered_panel(clipped, slots.active, NOTCH, theme::INFO, 1);
                tag_label(
                    clipped,
                    slots.active.top_left.x, slots.active.top_left.y,
                    "ACTIVE", theme::INFO, NOTCH,
                );
                let act_buf = fmt::hms(data.active_secs as u64);
                fonts::draw_centered_in_rect(
                    clipped, &fonts::value(),
                    act_buf.as_str(), slots.active, theme::FG,
                );

                // SLEEPS: count of completed light-sleep cycles.
                // Diagnostic - paced by the ~5 s heartbeat when really
                // sleeping (~12/min); a far higher rate vs UPTIME means
                // the CPU is instant-waking instead of gating off.
                chamfered_panel(clipped, slots.slept, NOTCH, theme::INFO, 1);
                tag_label(
                    clipped,
                    slots.slept.top_left.x, slots.slept.top_left.y,
                    "SLEEPS", theme::INFO, NOTCH,
                );
                let mut slept_buf: String<16> = String::new();
                let _ = write!(slept_buf, "{}", data.sleep_cycles);
                fonts::draw_centered_in_rect(
                    clipped, &fonts::value(),
                    slept_buf.as_str(), slots.slept, theme::FG,
                );
            },
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
            // Drag scroll for the panel stack.
            SystemEvent::TouchPressed { .. } | SystemEvent::TouchReleased => {
                let viewport_h = battery_viewport(&data.safe_area).size.height as i32;
                let content_h = battery_slots(0, &data.safe_area).content_h;
                if handle_scroll_drag(
                    &mut self.battery_scroll, event, viewport_h, content_h,
                ) {
                    return Action::Redraw;
                }
                Action::None
            }
            // Any live snapshot refresh or new sample should repaint -
            // including a charger-phase change, which the CHARGE panel
            // shows and which is the whole point of watching this view
            // while a cell anchors.
            SystemEvent::PowerUpdated { .. }
            | SystemEvent::BatteryChanged { .. }
            | SystemEvent::ChargerPhaseChanged { .. }
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
