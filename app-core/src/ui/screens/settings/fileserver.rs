//! FILE SERVER sub-view: serve the log directory over HTTP on the
//! LAN for as long as this screen is open.
//!
//! Lifecycle is the MIC TEST pattern, and deliberately so: entering
//! starts the session, and every path out of the view stops it. The
//! session holds the radio and blocks hardware light sleep, so the
//! screen being open is what authorises that cost - there is no way
//! to wander off and leave a server running.
//!
//! The address and token are only ever shown here. They are generated
//! per session by the WiFi task, so this view is not a status
//! readout, it is the only place the user can learn how to reach
//! their watch.

use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use heapless::String;
use core::fmt::Write;

use crate::ui::{fonts, layout, theme};
use crate::ui::layout::rect_hit;
use crate::ui::types::{Action, RenderCtx, SystemData, SystemEvent};
use crate::ui::widgets::{
    chamfered_button, chamfered_panel, tag_label, ButtonVariant, NOTCH, TAG_LABEL_H,
};

use super::{draw_header, header_back_hit, leaf_top_y, SettingsScreen, SettingsView};

/// Layout: the address panel, the token panel, and STOP.
struct ServerSlots {
    address_panel: Rectangle,
    token_panel: Rectangle,
    stop_btn: Rectangle,
}

fn server_slots(safe: &crate::data::SafeArea) -> ServerSlots {
    let mut s = layout::VStack::new(leaf_top_y(safe));
    let address_panel = s.slot(96);
    s.gap(14);
    let token_panel = s.slot(84);
    s.gap(18);
    let stop_btn = s.slot(44);
    ServerSlots { address_panel, token_panel, stop_btn }
}

/// The settings STORAGE sub-index row exists only in builds with the
/// radio - there is nothing to serve over otherwise.
pub(super) fn index_visible(data: &SystemData) -> bool {
    data.capabilities.wifi
}

/// Inline value on that row: reachable, or why not.
pub(super) fn index_value(data: &SystemData) -> String<20> {
    let mut buf = String::new();
    let _ = buf.push_str(match data.wifi {
        crate::data::WifiState::Serving { .. } => "SERVING",
        _ if !data.config.wifi.is_set() => "NO NETWORK",
        _ => "",
    });
    buf
}

impl SettingsScreen {
    pub(super) fn render_file_server<D: BlendTarget>(
        &self, display: &mut D, data: &SystemData, ctx: &RenderCtx,
    ) {
        draw_header(display, data, "FILE SERVER", theme::ACCENT, ctx);
        let slots = server_slots(&data.safe_area);

        // Everything hangs off whether a session actually came up.
        let serving = match data.wifi {
            crate::data::WifiState::Serving { ip, port, token } => Some((ip, port, token)),
            _ => None,
        };

        // ADDRESS: the URL to type, or why there isn't one yet.
        let accent = if serving.is_some() { theme::OK } else { theme::BORDER };
        chamfered_panel(display, slots.address_panel, NOTCH, accent, 1);
        tag_label(
            display,
            slots.address_panel.top_left.x, slots.address_panel.top_left.y,
            "ADDRESS", accent, NOTCH,
        );
        let mut line: String<28> = String::new();
        match serving {
            Some((ip, port, _)) => {
                let _ = write!(
                    line, "{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], port,
                );
            }
            None if !data.config.wifi.is_set() => {
                let _ = line.push_str("NO NETWORK STORED");
            }
            // Distinguish "coming up" from "over": a session that
            // ended itself must not sit here reading CONNECTING
            // forever, or a stopped server looks like a hung one.
            None if matches!(data.wifi, crate::data::WifiState::Connecting) => {
                let _ = line.push_str("CONNECTING");
            }
            None => {
                let _ = line.push_str("STOPPED");
            }
        }
        let value_rect = Rectangle::new(
            Point::new(
                slots.address_panel.top_left.x,
                slots.address_panel.top_left.y + TAG_LABEL_H + 6,
            ),
            Size::new(slots.address_panel.size.width, 32),
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(), line.as_str(), value_rect,
            if serving.is_some() { theme::FG } else { theme::FG_DIM },
        );
        let hint_rect = Rectangle::new(
            Point::new(
                slots.address_panel.top_left.x,
                slots.address_panel.top_left.y + 64,
            ),
            Size::new(slots.address_panel.size.width, 20),
        );
        fonts::draw_centered_in_rect(
            display, &fonts::caption(),
            if serving.is_some() { "OPEN IN A BROWSER" } else { "" },
            hint_rect, theme::FG_MUTED,
        );

        // TOKEN: the path segment. Without it every request 404s, so
        // it is shown as prominently as the address.
        chamfered_panel(display, slots.token_panel, NOTCH, accent, 1);
        tag_label(
            display,
            slots.token_panel.top_left.x, slots.token_panel.top_left.y,
            "TOKEN", accent, NOTCH,
        );
        let mut token_line: String<16> = String::new();
        if let Some((_, _, token)) = serving {
            let _ = token_line.push('/');
            for c in token.iter() {
                let _ = token_line.push(*c as char);
            }
            let _ = token_line.push('/');
        } else {
            let _ = token_line.push_str("--");
        }
        let token_rect = Rectangle::new(
            Point::new(
                slots.token_panel.top_left.x,
                slots.token_panel.top_left.y + TAG_LABEL_H,
            ),
            Size::new(
                slots.token_panel.size.width,
                slots.token_panel.size.height - TAG_LABEL_H as u32,
            ),
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(), token_line.as_str(), token_rect,
            if serving.is_some() { theme::FG } else { theme::FG_DIM },
        );

        // The button follows the session, so a server that stopped
        // itself - a bad token, or the budget - can be restarted from
        // the same screen with a fresh token, rather than making the
        // user leave the view and come back to re-trigger the entry
        // hook.
        if serving.is_some() {
            chamfered_button(
                display, slots.stop_btn, "STOP",
                ButtonVariant::Primary, theme::ACCENT,
            );
        } else if data.config.wifi.is_set() {
            chamfered_button(
                display, slots.stop_btn, "START",
                ButtonVariant::Primary, theme::INFO,
            );
        } else {
            chamfered_button(
                display, slots.stop_btn, "START",
                ButtonVariant::Ghost, theme::BORDER,
            );
        }
    }

    pub(super) fn file_server_event(
        &mut self, event: &SystemEvent, data: &mut SystemData,
    ) -> Action {
        match event {
            // Every exit stops the session. Back, swipe and STOP all
            // land here; the model turns this into the stop signal
            // whether or not a session is actually up.
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y, &data.safe_area) => {
                self.view = SettingsView::Storage;
                Action::StopFileServer
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Storage;
                Action::StopFileServer
            }
            SystemEvent::Tap { x, y } => {
                let slots = server_slots(&data.safe_area);
                if rect_hit(slots.stop_btn, *x, *y) {
                    // Serving -> stop and leave. Stopped -> restart in
                    // place, staying on the screen that shows the new
                    // address and token.
                    return if matches!(data.wifi, crate::data::WifiState::Serving { .. }) {
                        self.view = SettingsView::Storage;
                        Action::StopFileServer
                    } else if data.config.wifi.is_set() {
                        Action::StartFileServer
                    } else {
                        Action::None
                    };
                }
                Action::None
            }
            // The address and token arrive with the session coming up,
            // so this view has nothing to show until this lands.
            SystemEvent::WifiStatusUpdated { .. } => Action::Redraw,
            _ => Action::None,
        }
    }
}
