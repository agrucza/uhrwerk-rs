//! NFC screen - shows the last card an NFC scan identified.
//!
//! Display-only for now: it renders whatever `data.last_nfc` holds,
//! which the Model fills from `SystemEvent::NfcProbe`. There is no
//! scan trigger from this screen yet - a card appears when the boot
//! probe (or, later, always-on tap detection) identifies one. When
//! that lands, a tap will wake the display straight onto this screen.
//!
//! Layout: standard app chrome ("NFC" title), a single chamfered
//! panel tagged `CARD` with the family/technology label, the UID as
//! spaced hex, and - for a Type A card - its ATQA/SAK. With no card
//! seen this boot, a centered "PRESENT A CARD" caption.
//!
//! Accent: info-cyan - reads as "data / comms", distinct from the
//! other apps at a glance.

use core::fmt::Write;
use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};
use heapless::String;

use crate::events::SystemEvent;
use crate::nfc::CardIdentity;
use crate::ui::theme::Color;
use crate::ui::types::BlendTarget;
use crate::ui::types::{Action, RenderCtx, Screen, SystemData};
use crate::ui::{fonts, layout, theme};
use crate::ui::widgets::{
    app_chrome_back_hit, app_content_top, chamfered_panel, draw_app_chrome, tag_label,
    NOTCH, TAG_LABEL_H,
};

/// Per-screen accent. NFC reads as "data / contactless comms".
const ACCENT: Color = theme::INFO;

/// Static system-code shown in the header's right-telemetry slot.
const TELEMETRY: &str = "NFC.0001";

/// Side margin, matching the settings sub-views and the stopwatch
/// readout so content lines up across screens.
const SIDE_MARGIN: i32 = layout::VSTACK_SIDE_MARGIN;

/// Card panel height - room for the family line, technology, the
/// `UID` label + hex row, and the Type A ATQA/SAK line.
const PANEL_H: i32 = 210;

/// Format identifier bytes as spaced uppercase hex ("A4 AA 64 35").
/// A Type A UID is at most 10 bytes -> 29 chars, well inside the cap.
fn id_hex(bytes: &[u8]) -> String<48> {
    let mut s: String<48> = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            let _ = s.push(' ');
        }
        let _ = write!(s, "{:02X}", b);
    }
    s
}

pub struct NfcScreen;

impl NfcScreen {
    pub fn new() -> Self {
        Self
    }
}

impl Screen for NfcScreen {
    fn render<D: BlendTarget>(&self, display: &mut D, data: &SystemData, ctx: &RenderCtx) {
        draw_app_chrome(display, data, "NFC", TELEMETRY, ACCENT, ctx);

        let safe = &data.safe_area;
        let content_top = app_content_top(safe);

        // Three states: a recognized card renders the panel below; the
        // other two are a single centered caption.
        let card = match &data.last_nfc {
            crate::nfc::NfcScan::Card(c) => c,
            other => {
                let cy = content_top + (theme::SCREEN_H as i32 - content_top) / 2 - 20;
                let cx = theme::SCREEN_W as i32 / 2;
                match other {
                    crate::nfc::NfcScan::Unrecognized => {
                        // A card was there but couldn't be read.
                        fonts::draw_centered(
                            display,
                            &fonts::headline(),
                            "CARD NOT RECOGNIZED",
                            cx,
                            cy,
                            theme::WARN,
                        );
                        fonts::draw_centered(
                            display,
                            &fonts::caption(),
                            "unsupported or unreadable",
                            cx,
                            cy + 40,
                            theme::FG_MUTED,
                        );
                    }
                    _ => {
                        // No scan yet this boot.
                        fonts::draw_centered(
                            display,
                            &fonts::headline(),
                            "PRESENT A CARD",
                            cx,
                            cy,
                            theme::FG_MUTED,
                        );
                    }
                }
                return;
            }
        };

        // -- Card panel --------------------------------------------------
        let panel = Rectangle::new(
            Point::new(SIDE_MARGIN, content_top + 8),
            Size::new(
                (theme::SCREEN_W as i32 - SIDE_MARGIN * 2) as u32,
                PANEL_H as u32,
            ),
        );
        chamfered_panel(display, panel, NOTCH, ACCENT, 1);
        tag_label(display, panel.top_left.x, panel.top_left.y, "CARD", ACCENT, NOTCH);

        let x = panel.top_left.x + 16;
        let mut y = panel.top_left.y + TAG_LABEL_H + 14;

        // Family / technology headline (e.g. "MIFARE Classic 1K").
        fonts::draw_at(display, &fonts::headline(), card.label(), x, y, theme::FG);
        y += 42;

        // RF technology family beneath it.
        fonts::draw_at(
            display,
            &fonts::caption(),
            card.technology().label(),
            x,
            y,
            theme::FG_MUTED,
        );
        y += 30;

        // UID row: accent label then the spaced hex.
        fonts::draw_at(display, &fonts::label(), "UID", x, y, ACCENT);
        y += 22;
        let hex = id_hex(card.id_bytes());
        fonts::draw_at(display, &fonts::body(), hex.as_str(), x, y, theme::FG);
        y += 34;

        // Type A carries ATQA + SAK; other technologies have no
        // equivalent single-line fingerprint yet.
        if let CardIdentity::Iso14443a(a) = card {
            let mut extra: String<40> = String::new();
            let _ = write!(
                extra,
                "ATQA {:02X} {:02X}   SAK {:02X}",
                a.atqa[0], a.atqa[1], a.sak,
            );
            fonts::draw_at(display, &fonts::caption(), extra.as_str(), x, y, theme::FG_MUTED);
        }
    }

    fn on_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::PowerButtonLong => Action::Shutdown,
            // Header back chevron: pop the nav stack.
            SystemEvent::Tap { x, y } if app_chrome_back_hit(*x, *y, &data.safe_area) => {
                Action::Back
            }
            _ => Action::None,
        }
    }
}
