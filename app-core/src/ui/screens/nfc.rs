//! NFC screen - the on-watch card library.
//!
//! Internal state machine, like Settings: one [`NfcScreen`] holds an
//! [`NfcView`] that is the scrollable card [`List`](NfcView::List), the
//! [`Detail`](NfcView::Detail) of one selected card, or that card's
//! [`EditLabel`](NfcView::EditLabel) keyboard. Tapping a list row opens
//! that card's detail; the header back chevron steps back one view,
//! and backing out of the list pops the nav stack.
//!
//! * **List** - a scrollable stack of the cards a scan has saved
//!   ([`SystemData::card_library`]), newest first. Every row is three
//!   lines: the card's name (its custom label, else its family), the
//!   family + id bytes (just the id when the family is already line
//!   one), and last seen + dump state. Empty until the first scan
//!   ("NO CARDS YET"); a scan that could not be identified shows the
//!   transient "CARD NOT RECOGNIZED" while the library is still empty.
//! * **Detail** - a chamfered `CARD` panel with the name, family,
//!   the UID as spaced hex, and the Type A ATQA/SAK, the record's
//!   timestamps and dump status, and the bottom tiles: LABEL, the
//!   DUMP control for a MIFARE Classic, and REMOVE.
//! * **EditLabel** - the on-screen keyboard in text mode, seeded with
//!   the current custom label. DONE stores the text (empty clears it,
//!   back to the family name); CANCEL or the header back discards.
//!
//! Accent: info-cyan - reads as "data / comms", distinct from the
//! other apps at a glance.

use core::fmt::Write;
use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};
use heapless::{String, Vec};

use crate::card_library::{CardMeta, LABEL_MAX};
use crate::data::TimeData;
use crate::events::SystemEvent;
use crate::nfc::CardIdentity;
use crate::ui::layout::{rect_hit, ScrollState};
use crate::ui::theme::Color;
use crate::ui::types::BlendTarget;
use crate::ui::types::{Action, RenderCtx, Screen, SystemData};
use crate::ui::{fonts, glyphs, layout, theme};
use crate::ui::widgets::{
    app_chrome_back_hit, app_content_top, chamfered_button, chamfered_panel, draw_app_chrome,
    handle_scroll_drag, render_scrolled, row_lines, row_lines_h, tag_label,
    viewport_to_home_bar, ButtonVariant, Keyboard, KeyboardResult, RowControl, NOTCH,
    SCROLLBAR_GUTTER, TAG_LABEL_H,
};

/// Per-screen accent. NFC reads as "data / contactless comms".
const ACCENT: Color = theme::INFO;

/// Brighter accent for the just-scanned row's icon + chevron.
const ACCENT_HOT: Color = theme::INFO_HOT;

/// Alpha of the translucent accent wash behind the just-scanned row
/// (out of 255) - a subtle tint, not a solid fill, so the label stays
/// readable on top.
const HIGHLIGHT_ALPHA: u8 = 48;

/// Side margin, matching the settings sub-views and the stopwatch
/// readout so content lines up across screens.
const SIDE_MARGIN: i32 = layout::VSTACK_SIDE_MARGIN;

/// Card panel height - room for the family line, technology, the
/// `UID` label + hex row, and the Type A ATQA/SAK line.
const PANEL_H: i32 = 210;

/// A card's identity id bytes (UID / PUPI / IDm), owned - the detail
/// view's stable handle on its selected card, resolved against the
/// library each frame so a re-scan or removal can't dangle an index.
type CardId = Vec<u8, 10>;

/// Which view the NFC screen is showing.
enum NfcView {
    /// The scrollable card library.
    List,
    /// One selected card, addressed by its id bytes.
    Detail(CardId),
    /// The label keyboard for one card; DONE/CANCEL return to its
    /// detail.
    EditLabel(CardId),
}

pub struct NfcScreen {
    view: NfcView,
    /// Vertical scroll of the list view.
    scroll: ScrollState,
    /// Detail REMOVE is a two-tap confirm: the first tap sets this,
    /// turning the button into "CONFIRM?"; a second tap deletes.
    /// Reset when leaving the detail or opening a different card so a
    /// pending confirm never carries across cards.
    confirm_remove: bool,
    /// The label editor, seeded with the card's current label on
    /// every open.
    keyboard: Keyboard,
}

impl NfcScreen {
    pub fn new() -> Self {
        Self {
            view: NfcView::List,
            scroll: ScrollState::new(),
            confirm_remove: false,
            keyboard: Keyboard::plain_text(LABEL_MAX, "CARD LABEL"),
        }
    }

    // -- List view -----------------------------------------------------------

    fn render_list<D: BlendTarget>(&self, display: &mut D, data: &SystemData, ctx: &RenderCtx) {
        let lib = &data.card_library;
        let content_top = app_content_top(&data.safe_area);

        // Empty library: a centered caption. If the last scan failed to
        // identify (and there is nothing to list yet), show that
        // instead so the "not recognized" feedback survives.
        if lib.is_empty() {
            let cx = theme::SCREEN_W as i32 / 2;
            let cy = content_top + (theme::SCREEN_H as i32 - content_top) / 2 - 20;
            if matches!(data.last_nfc, crate::nfc::NfcScan::Unrecognized) {
                fonts::draw_centered(
                    display, &fonts::headline(), "CARD NOT RECOGNIZED", cx, cy, theme::WARN,
                );
                fonts::draw_centered(
                    display, &fonts::caption(), "unsupported or unreadable",
                    cx, cy + 40, theme::FG_MUTED,
                );
            } else {
                fonts::draw_centered(
                    display, &fonts::headline(), "NO CARDS YET", cx, cy, theme::FG_MUTED,
                );
                fonts::draw_centered(
                    display, &fonts::caption(), "present a card to save it",
                    cx, cy + 40, theme::FG_MUTED,
                );
            }
            return;
        }

        let viewport = list_viewport(&data.safe_area);
        let content_h = lib.cards.len() as i32 * card_row_h();
        render_scrolled(
            display, self.scroll.offset(), viewport, content_h, ACCENT, ctx,
            |clip, scroll| {
                for (i, meta) in lib.cards.iter().enumerate() {
                    let rect = list_row_rect(i, scroll, &data.safe_area);
                    let y0 = rect.top_left.y;
                    let y1 = y0 + rect.size.height as i32;
                    if !ctx.intersects_y(y0, y1) {
                        continue;
                    }
                    let hit = data.nfc_highlight.as_deref()
                        == Some(meta.identity.id_bytes());
                    if hit {
                        // A translucent accent wash plus a solid left bar
                        // mark the just-scanned card. Drawn before the row
                        // so the icon/label/chevron sit on top. The row's
                        // own bottom hairline stays visible (h - 1).
                        let x = rect.top_left.x;
                        let y = rect.top_left.y;
                        let w = rect.size.width as i32;
                        let h = rect.size.height as i32 - 1;
                        clip.fill_blend(x, y, w, h, ACCENT, HIGHLIGHT_ALPHA);
                        clip.fill_blend(x, y, 3, h, ACCENT, 255);
                    }
                    let accent = if hit { ACCENT_HOT } else { ACCENT };
                    let id_line = row_id_line(meta);
                    let seen_line = row_seen_line(meta);
                    row_lines(
                        clip, rect,
                        |d, cx, cy, c| glyphs::chip(d, cx, cy, 8, c),
                        accent,
                        card_name(meta),
                        &[id_line.as_str(), seen_line.as_str()],
                        RowControl::Chevron(accent),
                    );
                }
            },
        );
    }

    fn list_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            // Header back chevron: pop the nav stack (leave the app).
            SystemEvent::Tap { x, y } if app_chrome_back_hit(*x, *y, &data.safe_area) => {
                data.nfc_highlight = None;
                Action::Back
            }
            SystemEvent::Tap { x, y } => {
                if data.card_library.is_empty() {
                    return Action::None;
                }
                let viewport = list_viewport(&data.safe_area);
                let pt = Point::new(*x as i32, *y as i32);
                if !viewport.contains(pt) {
                    return Action::None;
                }
                let scroll = self.scroll.offset();
                // Index and rect mirror the render loop exactly, or draw
                // and hit-test drift apart.
                for (i, meta) in data.card_library.cards.iter().enumerate() {
                    let rect = list_row_rect(i, scroll, &data.safe_area);
                    if rect.contains(pt) {
                        let mut id: CardId = Vec::new();
                        let _ = id.extend_from_slice(meta.identity.id_bytes());
                        self.view = NfcView::Detail(id.clone());
                        self.confirm_remove = false;
                        data.nfc_highlight = None;
                        // The Model loads this card's sector summary
                        // (if it has a dump) into the RAM slot.
                        return Action::OpenNfcCard { id };
                    }
                }
                Action::None
            }
            SystemEvent::TouchPressed { .. } | SystemEvent::TouchReleased => {
                let viewport_h = list_viewport(&data.safe_area).size.height as i32;
                let content_h = data.card_library.cards.len() as i32 * card_row_h();
                if handle_scroll_drag(&mut self.scroll, event, viewport_h, content_h) {
                    // Engaging the list dismisses the just-scanned mark.
                    data.nfc_highlight = None;
                    Action::Redraw
                } else {
                    Action::None
                }
            }
            _ => Action::None,
        }
    }

    // -- Detail view ---------------------------------------------------------

    fn render_detail<D: BlendTarget>(
        &self, display: &mut D, data: &SystemData, meta: &CardMeta, ctx: &RenderCtx,
    ) {
        let _ = ctx;
        let card = &meta.identity;
        let content_top = app_content_top(&data.safe_area);

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

        // Headline: the custom label when set, else the family (e.g.
        // "MIFARE Classic 1K"). The caption beneath then carries what
        // the headline displaced: the family under a custom label, the
        // RF technology under a family headline.
        fonts::draw_at(display, &fonts::headline(), card_name(meta), x, y, theme::FG);
        y += 42;
        let sub = if meta.label.is_empty() { card.technology().label() } else { card.label() };
        fonts::draw_at(display, &fonts::caption(), sub, x, y, theme::FG_MUTED);
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

        // -- Record metadata (below the panel) ---------------------------
        // The stored record's provenance: when this card was first saved
        // and last seen, and - for a dumpable card - whether a dump is
        // held and how complete it is ("N/total blk  N/total sec"). The
        // totals come from the card kind, so no dump file is loaded to
        // render this.
        let mx = panel.top_left.x + 4;
        let mut my = panel.top_left.y + PANEL_H + 22;
        let mut line: String<40> = String::new();
        let _ = write!(line, "FIRST SEEN  {}", fmt_stamp(&meta.first_seen));
        fonts::draw_at(display, &fonts::caption(), line.as_str(), mx, my, theme::FG_MUTED);
        my += 28;
        line.clear();
        let _ = write!(line, "LAST SEEN   {}", fmt_stamp(&meta.last_seen));
        fonts::draw_at(display, &fonts::caption(), line.as_str(), mx, my, theme::FG_MUTED);
        my += 28;
        if is_dumpable(card) {
            if meta.has_dump {
                let (bt, st) = match card {
                    CardIdentity::Iso14443a(a) => {
                        (a.kind.classic_blocks(), a.kind.classic_sectors())
                    }
                    _ => (0, 0),
                };
                line.clear();
                let _ = write!(
                    line, "DUMP STORED  {}/{} blk  {}/{} sec",
                    meta.dump_blocks_read, bt, meta.dump_sectors_read, st,
                );
                fonts::draw_at(display, &fonts::caption(), line.as_str(), mx, my, theme::OK);
            } else {
                fonts::draw_at(display, &fonts::caption(), "NO DUMP", mx, my, theme::FG_MUTED);
            }
        }

        // -- Controls (bottom): LABEL, DUMP, REMOVE ----------------------
        // Three fixed tiles. Left is LABEL (ghost, cyan): opens the
        // label keyboard. Middle is the dump control for a dumpable
        // card: "DUMP" (cyan) with no dump yet, toggling to "REMOVE
        // DUMP" (amber) once one is stored, and the "present card"
        // prompt while armed. Right is REMOVE for the whole card,
        // signal-red (theme::DANGER) so it reads as destructive and
        // distinct from both, with a two-tap confirm (label flips to
        // "CONFIRM?").
        let [label_tile, dump_tile, remove_tile] = layout::bottom_tile_row::<3>();
        chamfered_button(display, label_tile, "LABEL", ButtonVariant::Ghost, ACCENT);
        let dump_cx = dump_tile.top_left.x + dump_tile.size.width as i32 / 2;
        let dump_cy = dump_tile.top_left.y + dump_tile.size.height as i32 / 2 - 8;
        if data.nfc_dump_armed {
            fonts::draw_centered(
                display, &fonts::label(), "PRESENT CARD", dump_cx, dump_cy, theme::WARN,
            );
        } else if is_dumpable(card) {
            if meta.has_dump {
                chamfered_button(
                    display, dump_tile, "REMOVE DUMP", ButtonVariant::Primary, theme::WARN,
                );
            } else {
                chamfered_button(display, dump_tile, "DUMP", ButtonVariant::Primary, ACCENT);
            }
        }

        let remove_label = if self.confirm_remove { "CONFIRM?" } else { "REMOVE" };
        chamfered_button(display, remove_tile, remove_label, ButtonVariant::Primary, theme::DANGER);
    }

    fn detail_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        // The selected card's id; resolve its identity from the library.
        let id: CardId = match &self.view {
            NfcView::Detail(id) => id.clone(),
            NfcView::List | NfcView::EditLabel(_) => return Action::None,
        };
        match event {
            // Header back chevron: return to the list, dropping any
            // pending remove-confirm.
            SystemEvent::Tap { x, y } if app_chrome_back_hit(*x, *y, &data.safe_area) => {
                self.confirm_remove = false;
                self.view = NfcView::List;
                Action::Redraw
            }
            SystemEvent::Tap { x, y } => {
                let [label_tile, dump_tile, remove_tile] = layout::bottom_tile_row::<3>();
                // REMOVE (always present): first tap arms the confirm,
                // second tap on it deletes and returns to the list.
                if rect_hit(remove_tile, *x, *y) {
                    if self.confirm_remove {
                        self.confirm_remove = false;
                        self.view = NfcView::List;
                        return Action::RemoveNfcCard { id };
                    }
                    self.confirm_remove = true;
                    return Action::Redraw;
                }
                // Any other tap cancels a pending confirm before it is
                // evaluated for the LABEL / DUMP buttons.
                let was_confirming = self.confirm_remove;
                self.confirm_remove = false;
                // LABEL: open the keyboard on the card's current label.
                if rect_hit(label_tile, *x, *y) {
                    let current = data
                        .card_library
                        .get_by_id(&id)
                        .map(|m| m.label.as_str())
                        .unwrap_or("");
                    self.keyboard.seed(current);
                    self.view = NfcView::EditLabel(id);
                    return Action::Redraw;
                }
                // DUMP tile: for a dumpable card that isn't armed, arm a
                // dump when none is stored, or remove the stored dump
                // when one is (the tile shows REMOVE DUMP then).
                let (dumpable, has_dump) = match data.card_library.get_by_id(&id) {
                    Some(m) => (is_dumpable(&m.identity), m.has_dump),
                    None => (false, false),
                };
                if dumpable && !data.nfc_dump_armed && rect_hit(dump_tile, *x, *y) {
                    return if has_dump {
                        Action::RemoveNfcDump { id }
                    } else {
                        Action::ArmNfcDump
                    };
                }
                // A stray tap that only dismissed the confirm still needs
                // a redraw to restore the REMOVE label.
                if was_confirming {
                    Action::Redraw
                } else {
                    Action::None
                }
            }
            _ => Action::None,
        }
    }

    // -- Edit-label view -----------------------------------------------------

    fn render_edit_label<D: BlendTarget>(&self, display: &mut D) {
        // The keyboard owns the whole content band below the app
        // chrome (its field starts under the header).
        self.keyboard.render(display);
    }

    fn edit_label_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        let id: CardId = match &self.view {
            NfcView::EditLabel(id) => id.clone(),
            _ => return Action::None,
        };
        // Header back = cancel: nothing stored.
        if let SystemEvent::Tap { x, y } = event {
            if app_chrome_back_hit(*x, *y, &data.safe_area) {
                self.view = NfcView::Detail(id);
                return Action::Redraw;
            }
        }
        match self.keyboard.handle_event(event) {
            KeyboardResult::Changed => Action::Redraw,
            KeyboardResult::Done => {
                // Empty text clears the label (the family name shows
                // again). Over-long text cannot happen: the keyboard is
                // capped at LABEL_MAX.
                let mut label: String<LABEL_MAX> = String::new();
                let _ = label.push_str(self.keyboard.text());
                self.view = NfcView::Detail(id.clone());
                Action::SetNfcLabel { id, label }
            }
            KeyboardResult::Cancelled => {
                self.view = NfcView::Detail(id);
                Action::Redraw
            }
            KeyboardResult::None => Action::None,
        }
    }
}

impl Screen for NfcScreen {
    fn on_mount(&mut self, _data: &SystemData) {
        // Opening the app - or a scan switching to it - always lands on
        // the list, scrolled to the top (newest card first). A plain
        // wake does not re-mount, so it keeps whatever view was open.
        self.view = NfcView::List;
        self.scroll = ScrollState::new();
        self.confirm_remove = false;
    }

    fn render<D: BlendTarget>(&self, display: &mut D, data: &SystemData, ctx: &RenderCtx) {
        // Header telemetry reflects the active view: "NFC.LIST" on the
        // list, "NFC.<n>" on a card detail where n is the card's 1-based
        // position in the list (newest = 0001), "NFC.LABEL" in the label
        // editor. A detail or editor whose card has vanished falls back
        // to the list, so its label does too.
        let mut telem: String<12> = String::new();
        match &self.view {
            NfcView::Detail(id) => {
                match data.card_library.cards.iter().position(|c| c.identity.id_bytes() == id.as_slice()) {
                    Some(i) => {
                        let _ = write!(telem, "NFC.{:04}", i + 1);
                    }
                    None => {
                        let _ = write!(telem, "NFC.LIST");
                    }
                }
            }
            NfcView::EditLabel(id) if data.card_library.get_by_id(id).is_some() => {
                let _ = write!(telem, "NFC.LABEL");
            }
            NfcView::EditLabel(_) | NfcView::List => {
                let _ = write!(telem, "NFC.LIST");
            }
        }
        draw_app_chrome(display, data, "NFC", telem.as_str(), ACCENT, ctx);

        // Detail / editor only when their card still exists; otherwise
        // fall back to the list (a removed or never-found card can't be
        // shown).
        match &self.view {
            NfcView::Detail(id) => {
                if let Some(meta) = data.card_library.get_by_id(id) {
                    self.render_detail(display, data, meta, ctx);
                    return;
                }
            }
            NfcView::EditLabel(id) => {
                if data.card_library.get_by_id(id).is_some() {
                    self.render_edit_label(display);
                    return;
                }
            }
            NfcView::List => {}
        }
        self.render_list(display, data, ctx);
    }

    fn on_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        if let SystemEvent::PowerButtonLong = event {
            return Action::Shutdown;
        }
        // Route to the active view. A detail or editor whose card has
        // vanished reverts to the list first.
        match &self.view {
            NfcView::Detail(id) if data.card_library.get_by_id(id).is_some() => {
                self.detail_event(event, data)
            }
            NfcView::EditLabel(id) if data.card_library.get_by_id(id).is_some() => {
                self.edit_label_event(event, data)
            }
            NfcView::Detail(_) | NfcView::EditLabel(_) => {
                self.view = NfcView::List;
                self.confirm_remove = false;
                self.list_event(event, data)
            }
            NfcView::List => self.list_event(event, data),
        }
    }
}

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

/// Format a record timestamp as "YYYY-MM-DD HH:MM:SS" for the detail
/// view's provenance lines. A never-set RTC yields a low year; the
/// value is shown as-is rather than hidden.
fn fmt_stamp(t: &TimeData) -> String<24> {
    let mut s: String<24> = String::new();
    let _ = write!(
        s, "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second,
    );
    s
}

/// Whether a card supports the dump sweep (MIFARE Classic families).
fn is_dumpable(card: &CardIdentity) -> bool {
    matches!(card, CardIdentity::Iso14443a(a) if a.kind.is_mifare_classic())
}

/// Scroll viewport for the list: from the content top down to the home
/// bar. Matches the settings index viewport.
fn list_viewport(safe: &crate::data::SafeArea) -> Rectangle {
    viewport_to_home_bar(app_content_top(safe), safe)
}

/// Height of one card row: a primary line plus two caption lines.
fn card_row_h() -> i32 {
    row_lines_h(2)
}

/// Rect for the Nth list row, shifted by the scroll offset. Width
/// leaves a scrollbar gutter on the right, like the settings rows.
fn list_row_rect(index: usize, scroll: i32, safe: &crate::data::SafeArea) -> Rectangle {
    let h = card_row_h();
    let y = app_content_top(safe) + index as i32 * h - scroll;
    Rectangle::new(
        Point::new(0, y),
        Size::new((theme::SCREEN_W as i32 - SCROLLBAR_GUTTER) as u32, h as u32),
    )
}

/// The card's display name: its custom label, else its family
/// ("MIFARE Classic 1K"). Row line one and the detail headline.
fn card_name(meta: &CardMeta) -> &str {
    if meta.label.is_empty() {
        meta.identity.label()
    } else {
        meta.label.as_str()
    }
}

/// Row line two: the id bytes in brackets, prefixed by the short
/// family when a custom label occupies line one ("Classic 1K [98 D4
/// DB 3D]"); just "[98 D4 DB 3D]" when the family is already the
/// name. Longest case: 12-char family + a 10-byte id = 44 chars.
fn row_id_line(meta: &CardMeta) -> String<48> {
    let mut s: String<48> = String::new();
    if !meta.label.is_empty() {
        let _ = write!(s, "{} ", meta.identity.short_label());
    }
    let _ = write!(s, "[{}]", id_hex(meta.identity.id_bytes()));
    s
}

/// Row line three: last seen, then the dump state for a dumpable card
/// ("DUMP 64/64" blocks captured / total, or "NO DUMP").
fn row_seen_line(meta: &CardMeta) -> String<40> {
    let mut s: String<40> = String::new();
    let _ = write!(s, "{}", fmt_stamp(&meta.last_seen));
    if let CardIdentity::Iso14443a(a) = &meta.identity {
        if a.kind.is_mifare_classic() {
            if meta.has_dump {
                let _ = write!(s, "  DUMP {}/{}", meta.dump_blocks_read, a.kind.classic_blocks());
            } else {
                let _ = write!(s, "  NO DUMP");
            }
        }
    }
    s
}
