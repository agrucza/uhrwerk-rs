//! Wi-Fi sub-views.
//!
//! WIFI: the single stored network + session status, SCAN / CONNECT /
//! FORGET. NETWORKS (WifiScan): the scan list, auto-refreshing pass
//! after pass while open (merged, stable order); tapping an entry
//! opens the passphrase keyboard (open networks commit directly). The
//! keyboard's DONE stores the credentials and kicks a sync session -
//! the join is the verification.

use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use heapless::String;
use core::fmt::Write;

use crate::ui::{fmt, fonts, glyphs, layout, theme, widgets};
use crate::ui::layout::rect_hit;
use crate::ui::types::{Action, RenderCtx, SystemData, SystemEvent};
use crate::ui::widgets::{
    chamfered_button, chamfered_panel, handle_scroll_drag, render_scrolled, row,
    tag_label, ButtonVariant, KeyboardResult, RowControl, NOTCH, ROW_H,
};

use super::{draw_header, header_back_hit, leaf_top_y, row_rect, rows_top, SettingsScreen, SettingsView};

/// Layout of the WIFI view - single source for render and hit-test.
struct WifiSlots {
    network_panel: Rectangle,
    scan_btn: Rectangle,
    connect_btn: Rectangle,
    forget_btn: Rectangle,
}

fn wifi_slots(safe: &crate::data::SafeArea) -> WifiSlots {
    let mut s = layout::VStack::new(leaf_top_y(safe));
    let network_panel = s.slot(96);
    s.gap(18);
    let (scan_btn, connect_btn) = s.pair(44, 12);
    s.gap(14);
    let forget_btn = s.slot(44);
    WifiSlots { network_panel, scan_btn, connect_btn, forget_btn }
}

/// Status line under the stored network name: what the radio is
/// doing now, or how the last session ended.
fn wifi_status_line(data: &SystemData) -> String<28> {
    use crate::data::{WifiFailure, WifiState};
    let mut line: String<28> = String::new();
    match data.wifi {
        // The merged list, not the last pass's own count - that is
        // what the NETWORKS view shows.
        WifiState::Scanned { .. } => {
            let _ = write!(line, "{} NETWORKS FOUND", data.wifi_scan.len());
        }
        WifiState::Synced { hour, minute } => {
            let _ = write!(line, "SYNCED {}", fmt::hm(hour, minute).as_str());
        }
        other => {
            let _ = line.push_str(match other {
                WifiState::Idle if data.config.wifi.is_set() => "READY",
                WifiState::Idle => "SCAN TO ADD A NETWORK",
                WifiState::Scanning => "SCANNING",
                WifiState::Connecting => "CONNECTING",
                WifiState::Failed(WifiFailure::RadioInit) => "RADIO FAILED",
                WifiState::Failed(WifiFailure::ScanFailed) => "SCAN FAILED",
                WifiState::Failed(WifiFailure::NoAp) => "NETWORK NOT FOUND",
                WifiState::Failed(WifiFailure::AuthFailed) => "WRONG PASSPHRASE",
                WifiState::Failed(WifiFailure::ConnectFailed) => "CONNECT FAILED",
                WifiState::Failed(WifiFailure::NoLease) => "NO DHCP LEASE",
                WifiState::Failed(WifiFailure::NoNtp) => "NO TIME SERVER",
                WifiState::Failed(WifiFailure::Timeout) => "TIMED OUT",
                WifiState::Scanned { .. } | WifiState::Synced { .. } => "",
            });
        }
    }
    line
}

/// Viewport of the NETWORKS list: header hairline to home bar.
fn wifi_scan_viewport(safe: &crate::data::SafeArea) -> Rectangle {
    widgets::viewport_to_home_bar(rows_top(safe), safe)
}

fn wifi_scan_content_h(data: &SystemData) -> i32 {
    data.wifi_scan.len() as i32 * ROW_H
}

impl SettingsScreen {
    pub(super) fn render_wifi<D: BlendTarget>(
        &self, display: &mut D, data: &SystemData, ctx: &RenderCtx,
    ) {
        draw_header(display, data, "WIFI", theme::ACCENT, ctx);
        let slots = wifi_slots(&data.safe_area);
        let stored = &data.config.wifi;
        let busy = data.wifi.is_busy();

        // Stored network + live status.
        chamfered_panel(display, slots.network_panel, NOTCH, theme::BORDER, 1);
        tag_label(
            display,
            slots.network_panel.top_left.x, slots.network_panel.top_left.y,
            "NETWORK", theme::BORDER, NOTCH,
        );
        let name_rect = Rectangle::new(
            Point::new(
                slots.network_panel.top_left.x,
                slots.network_panel.top_left.y + 28,
            ),
            Size::new(slots.network_panel.size.width, 32),
        );
        if stored.is_set() {
            fonts::draw_centered_in_rect(
                display, &fonts::value(), stored.ssid.as_str(), name_rect, theme::FG,
            );
        } else {
            fonts::draw_centered_in_rect(
                display, &fonts::value(), "NOT SET", name_rect, theme::FG_DIM,
            );
        }
        let status_rect = Rectangle::new(
            Point::new(
                slots.network_panel.top_left.x,
                slots.network_panel.top_left.y + 64,
            ),
            Size::new(slots.network_panel.size.width, 20),
        );
        let status_color = match data.wifi {
            crate::data::WifiState::Failed(_) => theme::ACCENT,
            crate::data::WifiState::Synced { .. } => theme::INFO,
            _ => theme::FG_MUTED,
        };
        fonts::draw_centered_in_rect(
            display, &fonts::caption(), wifi_status_line(data).as_str(),
            status_rect, status_color,
        );

        // SCAN opens the list (and kicks a session); CONNECT re-runs
        // the stored credentials. Both draw disabled while a session
        // runs, and wifi_event drops those taps too.
        if busy {
            chamfered_button(
                display, slots.scan_btn, "SCAN", ButtonVariant::Ghost, theme::BORDER,
            );
        } else {
            chamfered_button(
                display, slots.scan_btn, "SCAN", ButtonVariant::Primary, theme::ACCENT,
            );
        }
        // CONNECT stays live during a scan pass (the command queues
        // behind it); only a running sync disables it.
        let connecting = matches!(data.wifi, crate::data::WifiState::Connecting);
        let connect_label = if connecting { "CONNECTING" } else { "CONNECT" };
        if stored.is_set() && !connecting {
            chamfered_button(
                display, slots.connect_btn, connect_label,
                ButtonVariant::Primary, theme::INFO,
            );
        } else {
            chamfered_button(
                display, slots.connect_btn, connect_label,
                ButtonVariant::Ghost, theme::BORDER,
            );
        }
        if stored.is_set() {
            chamfered_button(
                display, slots.forget_btn, "FORGET NETWORK",
                ButtonVariant::Ghost, theme::BORDER,
            );
        }
    }

    pub(super) fn wifi_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
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
            SystemEvent::Tap { x, y } => {
                let slots = wifi_slots(&data.safe_area);
                let stored = &data.config.wifi;
                let busy = data.wifi.is_busy();
                if rect_hit(slots.scan_btn, *x, *y) {
                    if busy {
                        return Action::None;
                    }
                    self.wifi_scroll = layout::ScrollState::new();
                    self.view = SettingsView::WifiScan;
                    return Action::WifiScan;
                }
                if rect_hit(slots.connect_btn, *x, *y) {
                    if !stored.is_set()
                        || matches!(data.wifi, crate::data::WifiState::Connecting)
                    {
                        return Action::None;
                    }
                    return Action::WifiConnect;
                }
                if rect_hit(slots.forget_btn, *x, *y) && stored.is_set() {
                    return Action::WifiForget;
                }
                // The network panel re-opens the passphrase keyboard
                // for the stored network (typo repair without a
                // rescan).
                if rect_hit(slots.network_panel, *x, *y) && stored.is_set() {
                    self.wifi_ssid_draft.clear();
                    let _ = self.wifi_ssid_draft.push_str(stored.ssid.as_str());
                    self.wifi_passphrase_keyboard.seed(stored.passphrase.as_str());
                    self.view = SettingsView::WifiPassphrase;
                    return Action::Redraw;
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    pub(super) fn render_wifi_scan<D: BlendTarget>(
        &self, display: &mut D, data: &SystemData, ctx: &RenderCtx,
    ) {
        use crate::data::WifiState;
        draw_header(display, data, "NETWORKS", theme::ACCENT, ctx);
        let viewport = wifi_scan_viewport(&data.safe_area);

        if data.wifi_scan.is_empty() {
            // Empty state: what the radio is doing, and that a tap
            // re-kicks once it is free.
            let (line, hint) = match data.wifi {
                WifiState::Scanning => ("SCANNING", ""),
                WifiState::Connecting => ("RADIO BUSY", "TAP TO RETRY"),
                WifiState::Scanned { .. } => ("NO NETWORKS FOUND", "TAP TO RESCAN"),
                WifiState::Failed(_) => ("SCAN FAILED", "TAP TO RETRY"),
                WifiState::Idle | WifiState::Synced { .. } => ("", "TAP TO SCAN"),
            };
            let cy = viewport.top_left.y + viewport.size.height as i32 / 2;
            let line_rect = Rectangle::new(
                Point::new(0, cy - 30),
                Size::new(theme::SCREEN_W as u32, 32),
            );
            fonts::draw_centered_in_rect(
                display, &fonts::value(), line, line_rect, theme::FG_DIM,
            );
            let hint_rect = Rectangle::new(
                Point::new(0, cy + 6),
                Size::new(theme::SCREEN_W as u32, 20),
            );
            fonts::draw_centered_in_rect(
                display, &fonts::caption(), hint, hint_rect, theme::FG_MUTED,
            );
            return;
        }

        render_scrolled(
            display, self.wifi_scroll.offset(),
            viewport, wifi_scan_content_h(data), theme::ACCENT, ctx,
            |clipped, scroll| {
                for (i, net) in data.wifi_scan.iter().enumerate() {
                    let rect = row_rect(i, scroll, &data.safe_area);
                    let row_y0 = rect.top_left.y;
                    let row_y1 = row_y0 + rect.size.height as i32;
                    if !ctx.intersects_y(row_y0, row_y1) {
                        continue;
                    }
                    // Signal glyph tinted by strength; dBm inline;
                    // a lock left of the value on secured networks.
                    let strong = net.rssi >= -67;
                    let icon_color = if strong { theme::INFO } else { theme::FG_MUTED };
                    let mut dbm: String<8> = String::new();
                    let _ = write!(dbm, "{}", net.rssi);
                    row(
                        clipped, rect,
                        |d, cx, cy, c| glyphs::signal_small(d, cx, cy, 5, c),
                        icon_color,
                        net.ssid.as_str(),
                        RowControl::Inline(dbm.as_str(), theme::FG_MUTED),
                    );
                    if net.secured {
                        let value_w = fonts::measure_width(&fonts::body(), dbm.as_str());
                        let lock_cx = rect.top_left.x + rect.size.width as i32
                            - widgets::bodies::ROW_PAD - value_w - 16;
                        let cy = rect.top_left.y + rect.size.height as i32 / 2;
                        glyphs::lock(clipped, lock_cx, cy, 6, theme::FG_MUTED);
                    }
                }
            },
        );
    }

    pub(super) fn wifi_scan_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y, &data.safe_area) => {
                self.view = SettingsView::Wifi;
                Action::Redraw
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Wifi;
                Action::Redraw
            }
            SystemEvent::Tap { x, y } => {
                let viewport = wifi_scan_viewport(&data.safe_area);
                let pt = Point::new(*x as i32, *y as i32);
                if !viewport.contains(pt) {
                    return Action::None;
                }
                if data.wifi_scan.is_empty() {
                    // Empty-state tap = (re)scan; the model refuses
                    // it while a session runs.
                    return Action::WifiScan;
                }
                let scroll = self.wifi_scroll.offset();
                for (i, net) in data.wifi_scan.iter().enumerate() {
                    if !row_rect(i, scroll, &data.safe_area).contains(pt) {
                        continue;
                    }
                    self.wifi_ssid_draft.clear();
                    let _ = self.wifi_ssid_draft.push_str(net.ssid.as_str());
                    if !net.secured {
                        // Open network: nothing to type - store and
                        // join right away.
                        self.view = SettingsView::Wifi;
                        return Action::SetWifiCredentials {
                            ssid: self.wifi_ssid_draft.clone(),
                            passphrase: String::new(),
                        };
                    }
                    // Re-picking the stored network pre-fills its
                    // passphrase; anything else starts blank.
                    let stored = &data.config.wifi;
                    let seed = if stored.ssid == net.ssid {
                        stored.passphrase.as_str()
                    } else {
                        ""
                    };
                    self.wifi_passphrase_keyboard.seed(seed);
                    self.view = SettingsView::WifiPassphrase;
                    return Action::Redraw;
                }
                Action::None
            }
            SystemEvent::TouchPressed { .. } | SystemEvent::TouchReleased => {
                let viewport_h = wifi_scan_viewport(&data.safe_area).size.height as i32;
                if handle_scroll_drag(
                    &mut self.wifi_scroll, event, viewport_h, wifi_scan_content_h(data),
                ) {
                    return Action::Redraw;
                }
                Action::None
            }
            // Auto-refresh while the list is open: every finished pass
            // kicks the next one, merging into the list (a single
            // pass misses beaconing APs; several converge). Leaving
            // the view simply stops re-kicking - the pass in flight
            // completes and the radio goes off. A failed pass is NOT
            // retried automatically (a hard radio fault would loop);
            // the empty-state tap covers that.
            SystemEvent::WifiStatusUpdated {
                state: crate::data::WifiState::Scanned { .. },
            } => Action::WifiRescan,
            _ => Action::None,
        }
    }

    pub(super) fn render_wifi_passphrase<D: BlendTarget>(
        &self, display: &mut D, data: &SystemData, ctx: &RenderCtx,
    ) {
        // The network being provisioned is the title; the keyboard
        // owns the whole content band, so there is no second line.
        // Long SSIDs are cut so the title stays clear of the header
        // telemetry.
        let mut title: String<20> = String::new();
        for (i, c) in self.wifi_ssid_draft.chars().enumerate() {
            if i == 14 {
                let _ = title.push_str("..");
                break;
            }
            let _ = title.push(c);
        }
        draw_header(display, data, title.as_str(), theme::ACCENT, ctx);
        self.wifi_passphrase_keyboard.render(display);
    }

    pub(super) fn wifi_passphrase_event(
        &mut self, event: &SystemEvent, data: &mut SystemData,
    ) -> Action {
        // Header back = cancel: nothing stored.
        if let SystemEvent::Tap { x, y } = event {
            if header_back_hit(*x, *y, &data.safe_area) {
                self.view = SettingsView::Wifi;
                return Action::Redraw;
            }
        }
        match self.wifi_passphrase_keyboard.handle_event(event) {
            KeyboardResult::Changed => Action::Redraw,
            KeyboardResult::Done => {
                let mut passphrase: String<{ crate::config::WifiConfig::PASSPHRASE_CAP }> =
                    String::new();
                let _ = passphrase.push_str(self.wifi_passphrase_keyboard.text());
                self.view = SettingsView::Wifi;
                Action::SetWifiCredentials {
                    ssid: self.wifi_ssid_draft.clone(),
                    passphrase,
                }
            }
            KeyboardResult::Cancelled => {
                self.view = SettingsView::Wifi;
                Action::Redraw
            }
            KeyboardResult::None => Action::None,
        }
    }
}
