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
//! * **Detail** - a scrollable column: the chamfered `CARD` panel
//!   (name, family, UID, Type A ATQA/SAK), the last-seen line, and
//!   for a MIFARE Classic the SECTORS grid and the KEYS list, both
//!   rendered from the one summary slot in `SystemData` when it
//!   belongs to this card. Grid cells are colour-coded by state and
//!   tappable; a status line under the grid names the tapped sector
//!   and its key. Fixed bottom tiles: LABEL, DUMP / REMOVE DUMP, and
//!   REMOVE. Other technologies show the panel and last seen only
//!   until their reads exist.
//! * **EditLabel** - the on-screen keyboard in text mode, seeded with
//!   the current custom label. DONE stores the text (empty clears it,
//!   back to the family name); CANCEL or the header back discards.
//!
//! Accent: info-cyan - reads as "data / comms", distinct from the
//! other apps at a glance.

use core::fmt::Write;
use embedded_graphics::{
    geometry::{Point, Size},
    prelude::Primitive,
    primitives::{PrimitiveStyle, Rectangle},
    Drawable,
};
use heapless::{String, Vec};

use crate::card_library::{CardMeta, SectorInfo, LABEL_MAX};
use crate::data::{SafeArea, TimeData};
use crate::events::SystemEvent;
use crate::nfc::{CardIdentity, SectorState};
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

/// Card panel height - room for the name, the family/technology
/// caption, the UID line, and the Type A ATQA/SAK line (tag 29 +
/// 42 + 30 + 28 + 10 of caption ink + 11 bottom pad).
const PANEL_H: i32 = 150;

/// Sector grid: square cells in lines of 16 (a 1K is one line, a 4K
/// is 16 + 16 + 8). 16 cells at a 22 px pitch span 346 px, inside the
/// 354 px panel width.
const GRID_CELL: i32 = 16;
const GRID_PITCH: i32 = 22;
const GRID_PER_LINE: usize = 16;

/// Pitch of the KEYS lines (body font).
const KEY_LINE_H: i32 = 24;

/// Space below the last line of the detail column. Small: a 1K with
/// one key must end inside the viewport so it never scrolls.
const COLUMN_END_PAD: i32 = 4;

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
    /// Vertical scroll of the detail body (panel, grid, keys).
    detail_scroll: ScrollState,
    /// The sector cell the user tapped in the detail grid; the status
    /// line under the grid names it. Reset when a card opens.
    selected_sector: Option<u8>,
}

impl NfcScreen {
    pub fn new() -> Self {
        Self {
            view: NfcView::List,
            scroll: ScrollState::new(),
            confirm_remove: false,
            keyboard: Keyboard::plain_text(LABEL_MAX, "CARD LABEL"),
            detail_scroll: ScrollState::new(),
            selected_sector: None,
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
                        self.detail_scroll = ScrollState::new();
                        self.selected_sector = None;
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
        let card = &meta.identity;
        let lay = detail_layout(meta, &data.safe_area, key_lines(data, card.id_bytes()));
        let viewport = detail_viewport(&data.safe_area);
        // The summary slot, only when it is this card's.
        let summary = data
            .nfc_summary
            .as_ref()
            .filter(|s| s.id.as_slice() == card.id_bytes())
            .map(|s| &s.classic);
        let selected = self.selected_sector;

        // -- Scrollable body -------------------------------------------
        // Everything above the bottom tiles scrolls as one column, so
        // a 4K's three grid lines and a long key list fit on any card.
        render_scrolled(
            display, self.detail_scroll.offset(), viewport, lay.content_h, ACCENT, ctx,
            |clip, scroll| {
                let sy = |y: i32| y - scroll;

                // -- Card panel: name, family, UID, ATQA/SAK --------
                let panel = Rectangle::new(
                    Point::new(SIDE_MARGIN, sy(lay.panel_top)),
                    Size::new(
                        (theme::SCREEN_W as i32 - SIDE_MARGIN * 2) as u32,
                        PANEL_H as u32,
                    ),
                );
                chamfered_panel(clip, panel, NOTCH, ACCENT, 1);
                tag_label(clip, panel.top_left.x, panel.top_left.y, "CARD", ACCENT, NOTCH);
                let x = panel.top_left.x + 16;
                let mut y = panel.top_left.y + TAG_LABEL_H + 14;
                // Headline: the custom label when set, else the family.
                // The caption beneath carries what the headline
                // displaced: the family under a custom label, the RF
                // technology under a family headline.
                fonts::draw_at(clip, &fonts::headline(), card_name(meta), x, y, theme::FG);
                y += 42;
                let sub = if meta.label.is_empty() {
                    card.technology().label()
                } else {
                    card.label()
                };
                fonts::draw_at(clip, &fonts::caption(), sub, x, y, theme::FG_MUTED);
                y += 30;
                let mut line: String<48> = String::new();
                let _ = write!(line, "UID  {}", id_hex(card.id_bytes()));
                fonts::draw_at(clip, &fonts::body(), line.as_str(), x, y, theme::FG);
                y += 28;
                // Type A carries ATQA + SAK; other technologies have
                // no equivalent single-line fingerprint yet.
                if let CardIdentity::Iso14443a(a) = card {
                    line.clear();
                    let _ = write!(
                        line, "ATQA {:02X} {:02X}   SAK {:02X}", a.atqa[0], a.atqa[1], a.sak,
                    );
                    fonts::draw_at(clip, &fonts::caption(), line.as_str(), x, y, theme::FG_MUTED);
                }

                // -- Last seen ----------------------------------------
                line.clear();
                let _ = write!(line, "LAST SEEN  {}", fmt_stamp(&meta.last_seen));
                fonts::draw_at(
                    clip, &fonts::caption(), line.as_str(),
                    SIDE_MARGIN + 4, sy(lay.seen_y), theme::FG_MUTED,
                );

                // -- Classic: SECTORS grid + KEYS ---------------------
                let Some(sectors_total) = lay.sectors_total else {
                    return;
                };
                let gx = SIDE_MARGIN;
                tag_label(clip, gx, sy(lay.sectors_tag_y), "SECTORS", ACCENT, NOTCH);
                // Completeness beside the tag: blocks read / total,
                // from the record (no summary needed).
                if meta.has_dump {
                    let total = match card {
                        CardIdentity::Iso14443a(a) => a.kind.classic_blocks(),
                        _ => 0,
                    };
                    line.clear();
                    let _ = write!(line, "{}/{} blk", meta.dump_blocks_read, total);
                    fonts::draw_right(
                        clip, &fonts::caption(), line.as_str(),
                        theme::SCREEN_W as i32 - SIDE_MARGIN, sy(lay.sectors_tag_y) + 2,
                        theme::FG_MUTED,
                    );
                }
                for i in 0..sectors_total as usize {
                    let cell = grid_cell_rect(i, lay.grid_top, scroll);
                    let info = summary.and_then(|s| s.sector(i));
                    draw_sector_cell(clip, cell, info, selected == Some(i as u8));
                }
                // Status line: the tapped sector, or the hint.
                line.clear();
                let status_color = match selected {
                    Some(s) => {
                        sector_status_line(&mut line, s, summary.and_then(|m| m.sector(s as usize)));
                        theme::FG
                    }
                    None => {
                        let _ = if summary.is_some() {
                            write!(line, "TAP A SECTOR")
                        } else if meta.has_dump {
                            write!(line, "LOADING")
                        } else {
                            write!(line, "NO DUMP")
                        };
                        theme::FG_MUTED
                    }
                };
                fonts::draw_at(
                    clip, &fonts::caption(), line.as_str(), gx + 4, sy(lay.status_y), status_color,
                );

                tag_label(clip, gx, sy(lay.keys_tag_y), "KEYS", ACCENT, NOTCH);
                match summary {
                    Some(s) if !s.keys.is_empty() => {
                        for (k, key) in s.keys.iter().enumerate() {
                            line.clear();
                            for b in key {
                                let _ = write!(line, "{:02X}", b);
                            }
                            let _ = write!(line, "  x{}", s.key_sector_count(k));
                            fonts::draw_at(
                                clip, &fonts::body(), line.as_str(),
                                gx + 4, sy(lay.keys_top) + k as i32 * KEY_LINE_H, theme::FG,
                            );
                        }
                    }
                    _ => {
                        let text = if meta.has_dump && summary.is_some() { "NONE" } else { "-" };
                        fonts::draw_at(
                            clip, &fonts::body(), text, gx + 4, sy(lay.keys_top), theme::FG_MUTED,
                        );
                    }
                }
            },
        );

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
            // Drag scrolls the body. The bottom tiles sit outside the
            // viewport, so a drag never starts on them.
            SystemEvent::TouchPressed { .. } | SystemEvent::TouchReleased => {
                let (viewport_h, content_h) = match data.card_library.get_by_id(&id) {
                    Some(m) => (
                        detail_viewport(&data.safe_area).size.height as i32,
                        detail_layout(m, &data.safe_area, key_lines(data, &id)).content_h,
                    ),
                    None => return Action::None,
                };
                if handle_scroll_drag(&mut self.detail_scroll, event, viewport_h, content_h) {
                    Action::Redraw
                } else {
                    Action::None
                }
            }
            SystemEvent::Tap { x, y } => {
                let [label_tile, dump_tile, remove_tile] = layout::bottom_tile_row::<3>();
                // A sector cell: select it (the status line names it).
                // Same rect source as the render, shifted by the scroll.
                if let Some(m) = data.card_library.get_by_id(&id) {
                    let lay = detail_layout(m, &data.safe_area, key_lines(data, &id));
                    let pt = Point::new(*x as i32, *y as i32);
                    if let Some(total) = lay.sectors_total {
                        if detail_viewport(&data.safe_area).contains(pt) {
                            let scroll = self.detail_scroll.offset();
                            for i in 0..total as usize {
                                if grid_cell_rect(i, lay.grid_top, scroll).contains(pt) {
                                    self.confirm_remove = false;
                                    self.selected_sector = Some(i as u8);
                                    return Action::Redraw;
                                }
                            }
                        }
                    }
                }
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
        self.detail_scroll = ScrollState::new();
        self.selected_sector = None;
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

// -- Detail layout (one source for render AND hit-test) ----------------------

/// Y positions of the detail column, in unscrolled screen coordinates
/// (subtract the scroll offset to draw). `sectors_total` is `None` for
/// a non-Classic card, whose column ends after the last-seen line.
struct DetailLayout {
    panel_top: i32,
    seen_y: i32,
    sectors_total: Option<u8>,
    sectors_tag_y: i32,
    grid_top: i32,
    status_y: i32,
    keys_tag_y: i32,
    keys_top: i32,
    /// Total column height, for the scroll range.
    content_h: i32,
}

/// How many KEYS lines the column shows for this card: one per key in
/// the summary slot when it belongs to the card, else one (the "-" /
/// "NONE" placeholder).
fn key_lines(data: &SystemData, id: &[u8]) -> usize {
    data.nfc_summary
        .as_ref()
        .filter(|s| s.id.as_slice() == id)
        .map(|s| s.classic.keys.len())
        .unwrap_or(0)
        .max(1)
}

/// `key_lines` is the number of KEYS rows to reserve (see
/// [`key_lines`]); the column ends right after them.
fn detail_layout(meta: &CardMeta, safe: &SafeArea, key_lines: usize) -> DetailLayout {
    let top = app_content_top(safe);
    let panel_top = top + 8;
    let seen_y = panel_top + PANEL_H + 14;
    let sectors_total = match &meta.identity {
        CardIdentity::Iso14443a(a) if a.kind.is_mifare_classic() => {
            Some(a.kind.classic_sectors())
        }
        _ => None,
    };
    let Some(total) = sectors_total else {
        return DetailLayout {
            panel_top,
            seen_y,
            sectors_total: None,
            sectors_tag_y: 0,
            grid_top: 0,
            status_y: 0,
            keys_tag_y: 0,
            keys_top: 0,
            content_h: seen_y + 14 + COLUMN_END_PAD - top,
        };
    };
    let sectors_tag_y = seen_y + 26;
    let grid_top = sectors_tag_y + TAG_LABEL_H + 10;
    let lines = (total as usize).div_ceil(GRID_PER_LINE) as i32;
    let status_y = grid_top + lines * GRID_PITCH + 4;
    let keys_tag_y = status_y + 26;
    let keys_top = keys_tag_y + TAG_LABEL_H + 10;
    let keys_h = key_lines.max(1) as i32 * KEY_LINE_H;
    DetailLayout {
        panel_top,
        seen_y,
        sectors_total,
        sectors_tag_y,
        grid_top,
        status_y,
        keys_tag_y,
        keys_top,
        content_h: keys_top + keys_h + COLUMN_END_PAD - top,
    }
}

/// The detail body's scroll viewport: content top down to just above
/// the bottom tiles.
fn detail_viewport(safe: &SafeArea) -> Rectangle {
    let top = app_content_top(safe);
    Rectangle::new(
        Point::new(0, top),
        Size::new(theme::SCREEN_W as u32, (layout::BOTTOM_TILE_Y - 8 - top).max(0) as u32),
    )
}

/// Rect of sector cell `i`, shifted by the scroll offset.
fn grid_cell_rect(i: usize, grid_top: i32, scroll: i32) -> Rectangle {
    let col = (i % GRID_PER_LINE) as i32;
    let row = (i / GRID_PER_LINE) as i32;
    Rectangle::new(
        Point::new(SIDE_MARGIN + col * GRID_PITCH, grid_top + row * GRID_PITCH - scroll),
        Size::new(GRID_CELL as u32, GRID_CELL as u32),
    )
}

/// One grid cell. Fill = opened (green, "B" when key B opened it;
/// amber when a block read failed), red outline = locked, dim outline
/// = not swept or no summary. The selected cell gets an accent ring.
fn draw_sector_cell<D: BlendTarget>(
    d: &mut D, cell: Rectangle, info: Option<SectorInfo>, selected: bool,
) {
    let x = cell.top_left.x;
    let y = cell.top_left.y;
    let w = cell.size.width as i32;
    let h = cell.size.height as i32;
    match info {
        Some(SectorInfo { state: SectorState::Read, key, access }) => {
            d.fill_blend(x, y, w, h, theme::OK, 255);
            if matches!(key, Some((_, false))) {
                fonts::draw_centered_in_rect(d, &fonts::caption(), "B", cell, theme::BG);
            }
            // Corner mark: the sector is not in transport configuration
            // (cyan), or its trailer's access bits are malformed (amber).
            let mark = match access {
                Some(Ok(a)) if !a.is_transport() => Some(ACCENT_HOT),
                Some(Err(())) => Some(theme::WARN),
                _ => None,
            };
            if let Some(c) = mark {
                d.fill_blend(x + w - 5, y + 1, 4, 4, c, 255);
            }
        }
        Some(SectorInfo { state: SectorState::Partial, .. }) => {
            d.fill_blend(x, y, w, h, theme::WARN, 255);
        }
        Some(SectorInfo { state: SectorState::Locked, .. }) => {
            cell.into_styled(PrimitiveStyle::with_stroke(theme::DANGER, 1)).draw(d).ok();
        }
        None => {
            cell.into_styled(PrimitiveStyle::with_stroke(theme::FG_DIM, 1)).draw(d).ok();
        }
    }
    if selected {
        Rectangle::new(
            Point::new(x - 2, y - 2),
            Size::new((w + 4) as u32, (h + 4) as u32),
        )
        .into_styled(PrimitiveStyle::with_stroke(ACCENT, 2))
        .draw(d)
        .ok();
    }
}

/// The status line for a tapped sector: "S07  READ  A FFFFFFFFFFFF",
/// "S03  LOCKED", "S05  PARTIAL  B A0A1A2A3A4A5", "S09  NOT SWEPT".
fn sector_status_line(out: &mut String<48>, sector: u8, info: Option<SectorInfo>) {
    let _ = write!(out, "S{:02}  ", sector);
    let Some(info) = info else {
        let _ = write!(out, "NOT SWEPT");
        return;
    };
    let _ = match info.state {
        SectorState::Read => write!(out, "READ"),
        SectorState::Partial => write!(out, "PARTIAL"),
        SectorState::Locked => write!(out, "LOCKED"),
    };
    if let Some((key, is_a)) = info.key {
        let _ = write!(out, "  {} ", if is_a { "A" } else { "B" });
        for b in &key {
            let _ = write!(out, "{:02X}", b);
        }
    }
    // The trailer's access conditions: TRANSPORT for a factory card,
    // else the data rights (read/write by which key; "mixed" when the
    // three data groups differ) and whether key B is exposed.
    match info.access {
        Some(Ok(a)) if a.is_transport() => {
            let _ = write!(out, "  TRANSPORT");
        }
        Some(Ok(a)) => {
            if a.data_uniform() {
                let r = a.data_rights(0);
                let _ = write!(out, "  r{} w{}", r.read.short(), r.write.short());
            } else {
                let _ = write!(out, "  mixed");
            }
            if a.key_b_readable() {
                let _ = write!(out, " kB!");
            }
        }
        Some(Err(())) => {
            let _ = write!(out, "  BAD ACC");
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nfc::{CardKind, TypeAInfo, Uid};

    fn meta(kind: CardKind) -> CardMeta {
        let mut uid: Uid = Uid::new();
        uid.extend_from_slice(&[0x98, 0xD4, 0xDB, 0x3D]).unwrap();
        CardMeta {
            identity: CardIdentity::Iso14443a(TypeAInfo {
                uid, atqa: [0x04, 0x00], sak: 0x08, kind,
            }),
            label: String::new(),
            first_seen: TimeData::default(),
            last_seen: TimeData::default(),
            has_dump: true,
            dump_blocks_read: 64,
            dump_sectors_read: 16,
            seq: 1,
        }
    }

    #[test]
    fn grid_fits_the_panel_width_for_every_classic() {
        let safe = SafeArea::default();
        for kind in [CardKind::MifareClassicMini, CardKind::MifareClassic1K, CardKind::MifareClassic4K] {
            let lay = detail_layout(&meta(kind), &safe, 1);
            let total = lay.sectors_total.unwrap() as usize;
            for i in 0..total {
                let r = grid_cell_rect(i, lay.grid_top, 0);
                let right = r.top_left.x + r.size.width as i32;
                assert!(right <= theme::SCREEN_W as i32 - SIDE_MARGIN, "{kind:?} cell {i}");
            }
            // The 4K needs three lines; the 1K one.
            let lines = (lay.status_y - 4 - lay.grid_top) / GRID_PITCH;
            assert_eq!(lines as usize, total.div_ceil(GRID_PER_LINE));
        }
    }

    #[test]
    fn column_ends_after_the_last_key() {
        let safe = SafeArea::default();
        let viewport_h = detail_viewport(&safe).size.height as i32;
        // A 1K with one key fits without scrolling, and the column
        // ends right after that key line (no reserved empty space).
        let one_k = detail_layout(&meta(CardKind::MifareClassic1K), &safe, 1);
        assert!(one_k.content_h <= viewport_h);
        assert_eq!(
            one_k.content_h,
            one_k.keys_top + KEY_LINE_H + COLUMN_END_PAD - app_content_top(&safe),
        );
        // More keys, longer column, one line each.
        let three = detail_layout(&meta(CardKind::MifareClassic1K), &safe, 3);
        assert_eq!(three.content_h - one_k.content_h, 2 * KEY_LINE_H);
        // A 4K with many keys scrolls.
        let four_k = detail_layout(&meta(CardKind::MifareClassic4K), &safe, 6);
        assert!(four_k.content_h > viewport_h);
        // Non-Classic: no grid, short column.
        let desfire = detail_layout(&meta(CardKind::MifareDesfire), &safe, 1);
        assert!(desfire.sectors_total.is_none());
        assert!(desfire.content_h < viewport_h);
    }

    #[test]
    fn status_line_formats() {
        let mut s: String<48> = String::new();
        sector_status_line(&mut s, 7, Some(SectorInfo {
            state: SectorState::Read, key: Some(([0xFF; 6], true)), access: None,
        }));
        assert_eq!(s.as_str(), "S07  READ  A FFFFFFFFFFFF");
        s.clear();
        sector_status_line(&mut s, 7, Some(SectorInfo {
            state: SectorState::Read,
            key: Some(([0xFF; 6], true)),
            access: Some(Ok(crate::nfc::ACCESS_TRANSPORT)),
        }));
        assert_eq!(s.as_str(), "S07  READ  A FFFFFFFFFFFF  TRANSPORT");
        s.clear();
        // Personalised: data 100 (read A|B, write B), trailer 011.
        let custom = crate::nfc::SectorAccess { groups: [0b100, 0b100, 0b100, 0b011] };
        sector_status_line(&mut s, 8, Some(SectorInfo {
            state: SectorState::Read, key: Some(([0xFF; 6], true)), access: Some(Ok(custom)),
        }));
        assert_eq!(s.as_str(), "S08  READ  A FFFFFFFFFFFF  rAB wB");
        assert!(s.len() <= 44);
        s.clear();
        sector_status_line(&mut s, 3, Some(SectorInfo {
            state: SectorState::Locked, key: None, access: None,
        }));
        assert_eq!(s.as_str(), "S03  LOCKED");
        s.clear();
        sector_status_line(&mut s, 9, None);
        assert_eq!(s.as_str(), "S09  NOT SWEPT");
    }
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
