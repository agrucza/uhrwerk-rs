//! Notifications overlay - global ALERTS list.
//!
//! Reached via left-edge swipe-right (`Swipe { dir: Right, region:
//! Left }`); the model routes that gesture through
//! [`ActiveScreen::open_notifications`] and pushes the pre-overlay
//! screen onto the nav stack, so closing returns wherever the user
//! came from. Closes on swipe-left from anywhere or on header
//! chevron tap.
//!
//! ## Row interactions
//!
//! - Tap or swipe-right on any row: dismiss it. Alarm rows emit
//!   [`Action::DismissAlarm`] (stops buzz, clears alerting state);
//!   timer rows emit [`Action::StopBuzz`].
//! - Swipe-left on an alarm row: emits [`Action::SnoozeAlarm`]
//!   (stops buzz, programs RTC for now + 10 min).
//! - Tap on the `+10MIN` hint label inside an alarm row: same as
//!   swipe-left, a discoverable tap-target for the snooze action.
//! - Swipe-left starting outside any row: closes the overlay.

use core::fmt::Write;

use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;

use crate::events::{SwipeDir, SystemEvent};
use crate::ui::{fonts, layout, theme};
use crate::ui::types::{
    Action, Notification, NotificationSeverity, NotificationSource, RenderCtx, Screen,
    SystemData,
};
use crate::ui::widgets::{
    app_chrome_back_hit, app_header_rect, chamfered_panel, handle_scroll_drag, header,
    render_scrolled, status_bar, APP_CONTENT_TOP, APP_HOME_BAR_Y, SCROLLBAR_GUTTER,
};

/// Per-screen accent. Yellow == warning per spec ("ALERTS" header
/// is yellow-tinted).
const ACCENT: Color = theme::ALERT;

/// Side margin matching the alarm list rows.
const SIDE_MARGIN: i32 = 14;

/// Height of one notification row. Sized for three text lines
/// (title 11 px + subtitle 10 px + timestamp 9 px) plus padding.
const ROW_H: i32 = 56;

/// Vertical gap between adjacent rows.
const ROW_GAP: i32 = 6;

/// Vertical step between row top edges.
const ROW_STEP: i32 = ROW_H + ROW_GAP;

/// Top padding inside the scrollable list viewport so the first row
/// doesn't touch the header hairline.
const LIST_TOP_PAD: i32 = 4;

/// Notch carved off TL+BR of each notification row, per spec.
const ROW_NOTCH: i32 = 8;

/// Left-edge gutter inside the row reserved for the severity badge.
const BADGE_GUTTER: i32 = 34;

/// Inset of the badge glyph from the row's top-left corner.
const BADGE_X_INSET: i32 = 8;
const BADGE_Y_INSET: i32 = 6;

/// Y offsets of the row's three text lines from the row top.
const TITLE_Y: i32 = 6;
const SUBTITLE_Y: i32 = 22;
const TIMESTAMP_Y: i32 = 40;

/// Snooze hint label rendered in the right edge of alarm rows.
/// Tap-target plus a visual cue that snooze is the alternative to
/// dismiss. Compact so it doesn't crowd the row's text content.
const SNOOZE_HINT: &str = "+10MIN";

/// Width reserved for the snooze hint inside an alarm row's right
/// edge. Sized for `SNOOZE_HINT` at caption font with a margin so
/// taps near the label edge still register but the rect doesn't
/// eat the row's text area.
const SNOOZE_HINT_W: i32 = 56;

pub struct NotificationsScreen {
    /// Vertical scroll state for the row list.
    list_scroll: layout::ScrollState,
}

impl NotificationsScreen {
    pub fn new() -> Self {
        Self {
            list_scroll: layout::ScrollState::new(),
        }
    }
}

impl Screen for NotificationsScreen {
    fn render<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        // Header telemetry shows the live count, capped at xNN
        // (heapless::String<8> covers `x99` plus a comfort margin).
        let mut tele: heapless::String<8> = heapless::String::new();
        let _ = write!(tele, "x{:02}", data.notifications.entries.len().min(99));
        // Status bar + header only - no home indicator. Notifications
        // closes via swipe-left, so the bottom-of-screen swipe-up
        // indicator that other apps show would mislead the user.
        let mut time_buf: heapless::String<8> = heapless::String::new();
        let _ = write!(
            time_buf, "{:02}:{:02}", data.time.hour, data.time.minute,
        );
        status_bar(
            display,
            0,
            time_buf.as_str(),
            data.power.battery_percent,
            ACCENT,
            85,
        );
        header(display, app_header_rect(), "ALERTS", tele.as_str(), ACCENT);

        if data.notifications.entries.is_empty() {
            // Empty state: centered caption inside the content band.
            let body = Rectangle::new(
                Point::new(0, APP_CONTENT_TOP),
                Size::new(
                    theme::SCREEN_W as u32,
                    (APP_HOME_BAR_Y - APP_CONTENT_TOP) as u32,
                ),
            );
            fonts::draw_centered_in_rect(
                display, &fonts::value(),
                "NO ALERTS", body, theme::FG_DIM,
            );
            return;
        }

        // Row list inside a clipped viewport, newest-first. Vec
        // pushes append at the end so we iterate in reverse for
        // the "newest on top" reading order.
        let entries = &data.notifications.entries;
        render_scrolled(
            display,
            self.list_scroll.offset(),
            list_viewport_rect(),
            list_content_h(entries.len()),
            ACCENT,
            ctx,
            |clipped, scroll| {
                for (row_idx, n) in entries.iter().rev().enumerate() {
                    render_row(clipped, n, row_idx, scroll);
                }
            },
        );
    }

    fn on_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::PowerButtonLong => Action::Shutdown,

            // Header chevron == back.
            SystemEvent::Tap { x, y } if app_chrome_back_hit(*x, *y) => Action::Back,

            // Swipe-left starting on an alarm row snoozes that row.
            // Anywhere else (header, gaps, timer rows) closes the
            // overlay, mirroring the left-edge swipe-right that
            // opened it.
            SystemEvent::Swipe {
                dir: SwipeDir::Left,
                start_y, ..
            } => {
                if let Some(vec_idx) = vec_index_at_y(
                    *start_y as i32, data, self.list_scroll.offset(),
                ) {
                    return dismiss_row(data, vec_idx, RowGesture::Snooze);
                }
                Action::Back
            }

            // Swipe-right starting on a row dismisses that row.
            // Anywhere else is a no-op.
            SystemEvent::Swipe {
                dir: SwipeDir::Right,
                start_y, ..
            } => {
                if let Some(vec_idx) = vec_index_at_y(
                    *start_y as i32, data, self.list_scroll.offset(),
                ) {
                    return dismiss_row(data, vec_idx, RowGesture::Dismiss);
                }
                Action::None
            }

            // Drag scroll on the list.
            SystemEvent::TouchPressed { .. } | SystemEvent::TouchReleased => {
                let viewport_h = list_viewport_rect().size.height as i32;
                let content_h = list_content_h(data.notifications.entries.len());
                if handle_scroll_drag(
                    &mut self.list_scroll, event, viewport_h, content_h,
                ) {
                    return Action::Redraw;
                }
                Action::None
            }

            // Tap on a row dismisses it. If the tap lands on the
            // SNOOZE hint label inside an alarm row, snooze instead.
            SystemEvent::Tap { x, y } => {
                let scroll = self.list_scroll.offset();
                let px = *x as i32;
                let py = *y as i32;
                if let Some(vec_idx) = vec_index_at_y(py, data, scroll) {
                    let entry = &data.notifications.entries[vec_idx];
                    let row_idx = data.notifications.entries.len() - 1 - vec_idx;
                    let row = row_rect(row_idx, scroll);
                    let on_snooze_hint = entry.source == NotificationSource::Alarm
                        && rect_hit(snooze_hint_rect(row), px, py);
                    let gesture = if on_snooze_hint {
                        RowGesture::Snooze
                    } else {
                        RowGesture::Dismiss
                    };
                    return dismiss_row(data, vec_idx, gesture);
                }
                Action::None
            }

            _ => Action::None,
        }
    }
}

#[derive(Clone, Copy)]
enum RowGesture {
    Dismiss,
    Snooze,
}

/// Remove the entry at `vec_idx` and return the action that should
/// fire as a result. Tap and swipe-right both dismiss; swipe-left
/// or the SNOOZE hint tap snooze (alarm only).
fn dismiss_row(data: &mut SystemData, vec_idx: usize, gesture: RowGesture) -> Action {
    let source = data.notifications.entries[vec_idx].source;
    data.notifications.dismiss(vec_idx);
    match (gesture, source) {
        (RowGesture::Snooze, NotificationSource::Alarm) => Action::SnoozeAlarm,
        (RowGesture::Dismiss, NotificationSource::Alarm) => Action::DismissAlarm,
        // Timer rows have no snooze concept - any dismissal just
        // stops the buzz.
        (_, NotificationSource::Timer) => Action::StopBuzz,
    }
}

fn rect_hit(rect: Rectangle, x: i32, y: i32) -> bool {
    let rx = rect.top_left.x;
    let ry = rect.top_left.y;
    x >= rx
        && x < rx + rect.size.width as i32
        && y >= ry
        && y < ry + rect.size.height as i32
}

/// Sub-rect inside an alarm row reserved for the SNOOZE hint
/// label. Right-aligned, vertically spanning the row.
fn snooze_hint_rect(row: Rectangle) -> Rectangle {
    let x = row.top_left.x + row.size.width as i32 - SNOOZE_HINT_W;
    Rectangle::new(
        Point::new(x, row.top_left.y),
        Size::new(SNOOZE_HINT_W as u32, row.size.height),
    )
}

// -- Row rendering -----------------------------------------------------------

fn render_row<D: BlendTarget>(
    display: &mut D, n: &Notification, row_idx: usize, scroll: i32,
) {
    let rect = row_rect(row_idx, scroll);
    let color = severity_color(n.severity);

    chamfered_panel(display, rect, ROW_NOTCH, color, 1);

    // Badge: severity-coloured glyph in the row's left gutter.
    fonts::draw_centered(
        display, &fonts::value(),
        n.source.badge(),
        rect.top_left.x + BADGE_X_INSET + 6,
        rect.top_left.y + BADGE_Y_INSET,
        color,
    );

    let text_x = rect.top_left.x + BADGE_GUTTER;

    // Title: source category, severity-coloured.
    fonts::draw_at(
        display, &fonts::caption(),
        n.source.title(),
        text_x,
        rect.top_left.y + TITLE_Y,
        color,
    );

    // Subtitle: free-form context, FG.
    fonts::draw_at(
        display, &fonts::caption(),
        n.subtitle.as_str(),
        text_x,
        rect.top_left.y + SUBTITLE_Y,
        theme::FG,
    );

    // SNOOZE hint on alarm rows only - vertically centred at the
    // right edge, accent-coloured, doubles as a tap target.
    if n.source == NotificationSource::Alarm {
        let hint_rect = snooze_hint_rect(rect);
        fonts::draw_centered_in_rect(
            display, &fonts::caption(),
            SNOOZE_HINT, hint_rect, color,
        );
    }

    // Timestamp: "»» HH:MM", FG_MUTED.
    let mut ts: heapless::String<12> = heapless::String::new();
    let _ = write!(ts, "»» {:02}:{:02}", n.ts_hour, n.ts_minute);
    fonts::draw_at(
        display, &fonts::caption(),
        ts.as_str(),
        text_x,
        rect.top_left.y + TIMESTAMP_Y,
        theme::FG_MUTED,
    );
}

// -- Layout helpers ----------------------------------------------------------

/// Severity → border / label / badge colour.
fn severity_color(sev: NotificationSeverity) -> Color {
    match sev {
        NotificationSeverity::Critical => theme::ACCENT,
        NotificationSeverity::Warning  => theme::WARN,
        NotificationSeverity::Ok       => theme::OK,
        NotificationSeverity::Info     => theme::INFO,
    }
}

/// Rect for the row at display index `row_idx` (0 = newest, top of
/// list), shifted by the scroll offset.
fn row_rect(row_idx: usize, scroll: i32) -> Rectangle {
    let y = APP_CONTENT_TOP + LIST_TOP_PAD + row_idx as i32 * ROW_STEP - scroll;
    Rectangle::new(
        Point::new(SIDE_MARGIN, y),
        Size::new(
            (theme::SCREEN_W as i32 - SIDE_MARGIN * 2 - SCROLLBAR_GUTTER) as u32,
            ROW_H as u32,
        ),
    )
}

/// Visible viewport rect for the row list. Spans from just below
/// the header hairline to just above the home indicator.
fn list_viewport_rect() -> Rectangle {
    let top = APP_CONTENT_TOP;
    let bot = APP_HOME_BAR_Y - 4;
    Rectangle::new(
        Point::new(0, top),
        Size::new(theme::SCREEN_W as u32, (bot - top) as u32),
    )
}

/// Total content height of the row list for `n` entries.
fn list_content_h(n: usize) -> i32 {
    LIST_TOP_PAD + n as i32 * ROW_STEP - ROW_GAP + LIST_TOP_PAD
}

/// Translate a screen-y to the underlying `entries` Vec index for
/// the row that y lands on. Accounts for the newest-first display
/// order (display row 0 == last vec entry).
fn vec_index_at_y(y: i32, data: &SystemData, scroll: i32) -> Option<usize> {
    let entries = &data.notifications.entries;
    if entries.is_empty() {
        return None;
    }
    let viewport = list_viewport_rect();
    if y < viewport.top_left.y
        || y >= viewport.top_left.y + viewport.size.height as i32
    {
        return None;
    }
    for row_idx in 0..entries.len() {
        let r = row_rect(row_idx, scroll);
        let ry = r.top_left.y;
        if y >= ry && y < ry + r.size.height as i32 {
            // Display row 0 = last vec entry (newest first).
            return Some(entries.len() - 1 - row_idx);
        }
    }
    None
}
