//! Storage sub-views.
//!
//! Two-level hierarchy:
//!
//!   Settings → Storage (sub-index) → { Flash | SD Card | Restore |
//!   Factory Reset }
//!
//! The sub-index mirrors the top-level settings index layout (one
//! row per leaf). Each leaf is a focused view for its single
//! concern. Back navigation from a leaf returns to the Storage
//! sub-index; back from the sub-index returns to the Settings index.

use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use heapless::String;
use core::fmt::Write;

use crate::ui::{fonts, layout, theme};
use crate::ui::types::{Action, RenderCtx, SystemData, SystemEvent};
use crate::ui::widgets::{
    chamfered_button, chamfered_panel, tag_label, ButtonVariant, NOTCH,
};

use super::index::{always, index_viewport_rect, render_rows, row_hit, IndexRow, RowIcon, RowKind};
use super::{draw_header, header_back_hit, leaf_top_y, rows_top, SettingsScreen, SettingsView};

// -- Storage sub-index rows --------------------------------------------------
//
// Same IndexRow pattern as the top-level settings index, one level
// deeper. Each row taps into a storage leaf view.

fn storage_flash_value(data: &SystemData) -> String<20> {
    let mut buf = String::new();
    let _ = write!(
        buf,
        "{} FILES / {} KB",
        data.storage.files,
        data.storage.total_bytes / 1024,
    );
    buf
}

fn storage_sd_value(data: &SystemData) -> String<20> {
    let mut buf = String::new();
    let _ = buf.push_str(if data.storage.sd_online { "ONLINE" } else { "NOT PRESENT" });
    buf
}

fn storage_reset_value(_data: &SystemData) -> String<20> {
    String::new()
}

fn storage_restore_value(data: &SystemData) -> String<20> {
    let mut buf = String::new();
    let _ = buf.push_str(if data.storage.sd_online { "" } else { "SD NOT PRESENT" });
    buf
}

const STORAGE_INDEX_ROWS: &[IndexRow] = &[
    IndexRow {
        label: "FLASH",
        icon: RowIcon::Flash,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::StorageFlash, value_fn: storage_flash_value },
    },
    IndexRow {
        label: "SD CARD",
        icon: RowIcon::SdCard,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::StorageSd, value_fn: storage_sd_value },
    },
    IndexRow {
        label: "RESTORE FROM SD",
        icon: RowIcon::Restore,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::StorageRestoreFlash, value_fn: storage_restore_value },
    },
    IndexRow {
        label: "FACTORY RESET",
        icon: RowIcon::Skull,
        visible: always,
        kind: RowKind::Navigate { target: SettingsView::StorageFactoryReset, value_fn: storage_reset_value },
    },
];

// -- Leaf layout helpers -----------------------------------------------------
//
// Each non-trivial leaf sub-view has a `*_slots()` function that
// returns ALL its rects via a [`layout::VStack`] cursor. Render and
// event handlers both call the same `*_slots()` function and
// destructure into named rects, so they're guaranteed to agree on
// geometry - no chance of the event-side hit-test rect drifting from
// the render-side draw rect.

/// Storage-SD sub-view rects: (status_panel, action_button).
fn storage_sd_slots(safe: &crate::data::SafeArea) -> (Rectangle, Rectangle) {
    let mut s = layout::VStack::new(leaf_top_y(safe));
    let panel = s.slot(100);
    s.gap(18);
    let button = s.slot(36);
    (panel, button)
}

/// Restore / Factory-Reset sub-view rects: (warning_panel, cancel,
/// primary). Same layout for both; they differ only in colors and
/// labels.
fn confirmation_slots(safe: &crate::data::SafeArea) -> (Rectangle, Rectangle, Rectangle) {
    let mut s = layout::VStack::new(leaf_top_y(safe));
    let panel = s.slot(100);
    s.gap(18);
    let (cancel, primary) = s.pair(36, 12);
    (panel, cancel, primary)
}

impl SettingsScreen {
    // -- Storage sub-index ---------------------------------------------------

    pub(super) fn render_storage_index<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "STORAGE", theme::ACCENT, ctx);
        // Storage sub-index doesn't scroll today (4 rows always
        // fit). Scroll = 0; if more storage rows land later,
        // give SettingsScreen a second `ScrollState` and viewport.
        render_rows(display, data, STORAGE_INDEX_ROWS, 0, ctx);
    }

    pub(super) fn storage_index_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
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
                if let Some(action) = row_hit(
                    *x, *y, STORAGE_INDEX_ROWS, data,
                    0, &index_viewport_rect(&data.safe_area),
                    &mut self.view,
                ) {
                    return action;
                }
                Action::None
            }
            SystemEvent::StorageUsageUpdated { .. } => Action::Redraw,
            _ => Action::None,
        }
    }

    // -- Flash leaf (read-only info) -----------------------------------------

    pub(super) fn render_storage_flash<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "FLASH", theme::ACCENT, ctx);

        // Chamfered HUD panel with a hanging FLASH tag ribbon - the
        // spec's "tag-labelled panel" idiom. Body carries the usage
        // numbers as mono-ish body text.
        let margin = 28i32;
        let panel_w = theme::SCREEN_W as i32 - margin * 2;
        let panel_h = 120i32;
        let panel_rect = Rectangle::new(
            Point::new(margin, rows_top(&data.safe_area) + 24),
            Size::new(panel_w as u32, panel_h as u32),
        );
        // Symmetric chamfered panel (Nightwatch default - TL + BR both cut).
        chamfered_panel(display, panel_rect, NOTCH, theme::ACCENT, 1);

        // Tag ribbon sits exactly at the panel's TL corner. Its own
        // TL chamfer of size NOTCH carves out the same triangular
        // area as the panel's TL chamfer so the two align pixel-
        // for-pixel.
        tag_label(
            display,
            panel_rect.top_left.x,
            panel_rect.top_left.y,
            "FLASH",
            theme::ACCENT,
            NOTCH,
        );

        // Interior: usage line centered vertically in the full panel
        // rect. The tag sits in the top-left corner and doesn't
        // interfere with a single centered line of body text.
        let mut buf: String<32> = String::new();
        let _ = write!(
            buf,
            "{} FILES / {} KB",
            data.storage.files,
            data.storage.total_bytes / 1024,
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(),
            buf.as_str(), panel_rect, theme::FG,
        );
    }

    pub(super) fn storage_flash_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y, &data.safe_area) => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::StorageUsageUpdated { .. } => Action::Redraw,
            _ => Action::None,
        }
    }

    // -- SD card leaf (status + tap to probe) --------------------------------

    pub(super) fn render_storage_sd<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "SD CARD", theme::ACCENT, ctx);

        // Status: chamfered tag-labelled panel. Border + tag tint
        // tracks online/offline (green/signal). Read-only - the
        // button below triggers the probe.
        let (status_rect, probe_rect) = storage_sd_slots(&data.safe_area);
        let (accent, status_text) = if data.storage.sd_online {
            (theme::OK, "ONLINE")
        } else {
            (theme::WARN, "NOT PRESENT")
        };
        chamfered_panel(display, status_rect, NOTCH, accent, 1);
        tag_label(
            display,
            status_rect.top_left.x,
            status_rect.top_left.y,
            "STATUS",
            accent,
            NOTCH,
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(),
            status_text, status_rect, accent,
        );

        // Probe action button (chamfered Primary), label depends on
        // state (initialize vs reprobe).
        let probe_text = if data.storage.sd_online { "REPROBE" } else { "INITIALIZE" };
        chamfered_button(
            display, probe_rect, probe_text,
            ButtonVariant::Primary, theme::ACCENT,
        );
    }

    pub(super) fn storage_sd_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y, &data.safe_area) => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::Tap { x, y } => {
                let pt = Point::new(*x as i32, *y as i32);
                let (_, probe_rect) = storage_sd_slots(&data.safe_area);
                if probe_rect.contains(pt) {
                    return Action::InitSd;
                }
                Action::None
            }
            SystemEvent::StorageUsageUpdated { .. } => Action::Redraw,
            _ => Action::None,
        }
    }

    // -- Restore-from-SD leaf (destructive, gated on SD online) --------------

    pub(super) fn render_storage_restore<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        // DANGER like its FACTORY RESET sibling: restoring overwrites
        // the live config and reboots.
        draw_header(display, data, "RESTORE FROM SD", theme::DANGER, ctx);

        // Warning panel: danger-bordered chamfered panel with a
        // RESTORE tag. Body explains what the action does.
        let (warn_rect, cancel_rect, primary_rect) = confirmation_slots(&data.safe_area);
        chamfered_panel(display, warn_rect, NOTCH, theme::DANGER, 1);
        tag_label(
            display,
            warn_rect.top_left.x,
            warn_rect.top_left.y,
            "RESTORE",
            theme::DANGER,
            NOTCH,
        );
        let body = if data.storage.sd_online {
            "FLASH CONFIG // REBOOT"
        } else {
            "SD NOT PRESENT"
        };
        let body_color = if data.storage.sd_online { theme::FG } else { theme::FG_DIM };
        fonts::draw_centered_in_rect(
            display, &fonts::body(),
            body, warn_rect, body_color,
        );

        // CANCEL / RESTORE buttons. Restore disabled (Ghost variant)
        // when SD isn't online.
        chamfered_button(
            display, cancel_rect, "CANCEL",
            ButtonVariant::Ghost, theme::BORDER,
        );
        if data.storage.sd_online {
            chamfered_button(
                display, primary_rect, "RESTORE",
                ButtonVariant::Primary, theme::DANGER,
            );
        } else {
            chamfered_button(
                display, primary_rect, "RESTORE",
                ButtonVariant::Ghost, theme::BORDER,
            );
        }
    }

    pub(super) fn storage_restore_event(
        &mut self,
        event: &SystemEvent,
        data: &mut SystemData,
    ) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y, &data.safe_area) => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::Tap { x, y } => {
                let pt = Point::new(*x as i32, *y as i32);
                let (_, cancel_rect, primary_rect) = confirmation_slots(&data.safe_area);
                if cancel_rect.contains(pt) {
                    self.view = SettingsView::Storage;
                    return Action::Redraw;
                }
                if primary_rect.contains(pt) && data.storage.sd_online {
                    // No bounce-back - the manager will software-
                    // reset shortly after this returns.
                    return Action::RestoreFromSd;
                }
                Action::None
            }
            SystemEvent::StorageUsageUpdated { .. } => Action::Redraw,
            _ => Action::None,
        }
    }

    // -- Factory reset leaf (destructive) ------------------------------------

    pub(super) fn render_storage_factory_reset<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "FACTORY RESET", theme::DANGER, ctx);

        // Warning panel: danger-tinted chamfered panel with PURGE
        // tag and irreversible-action copy.
        let (warn_rect, cancel_rect, primary_rect) = confirmation_slots(&data.safe_area);
        chamfered_panel(display, warn_rect, NOTCH, theme::DANGER, 1);
        tag_label(
            display,
            warn_rect.top_left.x,
            warn_rect.top_left.y,
            "PURGE",
            theme::DANGER,
            NOTCH,
        );
        fonts::draw_centered_in_rect(
            display, &fonts::body(),
            "WIPES CONFIG // LOGS", warn_rect, theme::FG,
        );

        // CANCEL (ghost) + PURGE (filled danger) button pair.
        chamfered_button(
            display, cancel_rect, "CANCEL",
            ButtonVariant::Ghost, theme::BORDER,
        );
        chamfered_button(
            display, primary_rect, "PURGE",
            ButtonVariant::Primary, theme::DANGER,
        );
    }

    pub(super) fn storage_factory_reset_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y, &data.safe_area) => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Storage;
                Action::Redraw
            }
            SystemEvent::Tap { x, y } => {
                let pt = Point::new(*x as i32, *y as i32);
                let (_, cancel_rect, primary_rect) = confirmation_slots(&data.safe_area);
                if cancel_rect.contains(pt) {
                    self.view = SettingsView::Storage;
                    return Action::Redraw;
                }
                if primary_rect.contains(pt) {
                    // Bounce back to Storage sub-index on confirm
                    // so the user sees the refreshed usage counts
                    // land naturally.
                    self.view = SettingsView::Storage;
                    return Action::FactoryReset;
                }
                Action::None
            }
            _ => Action::None,
        }
    }
}
