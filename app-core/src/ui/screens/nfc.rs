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
//!   ("NO CARDS YET"). A scan that could not be identified, or one
//!   that could not be stored because the library is full, lands here
//!   with a banner above the rows until the list is used.
//!
//! An identified scan lands on that card's **Detail**: the Model
//! re-mounts the screen with `nfc_open_card` set, and the mount opens
//! it. The list keeps the highlight for when the user backs out.
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

/// Type 2 page grid: denser cells, 24 per line at a 13 px pitch (312
/// px), so an NTAG216's 231 pages take ten lines.
const PAGE_GRID_CELL: i32 = 10;
const PAGE_GRID_PITCH: i32 = 13;
const PAGE_GRID_PER_LINE: usize = 24;

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
    /// The list's scan banner ("CARD NOT RECOGNIZED" / "LIBRARY
    /// FULL") was dismissed by a list interaction. A scan re-mounts
    /// the screen, which brings the banner back.
    banner_dismissed: bool,
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
            banner_dismissed: false,
        }
    }

    /// The banner the list shows above its rows for the last scan,
    /// if any and not yet dismissed.
    fn banner(&self, data: &SystemData) -> Option<ListBanner> {
        if self.banner_dismissed {
            return None;
        }
        if data.nfc_library_full {
            if let crate::nfc::NfcScan::Card(c) = &data.last_nfc {
                return Some(ListBanner::Full(c.clone()));
            }
        }
        if matches!(data.last_nfc, crate::nfc::NfcScan::Unrecognized) {
            return Some(ListBanner::Unrecognized);
        }
        None
    }

    // -- List view -----------------------------------------------------------

    fn render_list<D: BlendTarget>(&self, display: &mut D, data: &SystemData, ctx: &RenderCtx) {
        let lib = &data.card_library;
        let content_top = app_content_top(&data.safe_area);
        let banner = self.banner(data);
        let offset = banner.as_ref().map_or(0, |_| BANNER_H);

        // Empty library: a centered caption (under the scan banner,
        // if one is up).
        if lib.is_empty() {
            if let Some(b) = &banner {
                draw_banner(display, content_top, b);
            }
            let cx = theme::SCREEN_W as i32 / 2;
            let cy = content_top + offset + (theme::SCREEN_H as i32 - content_top - offset) / 2 - 20;
            fonts::draw_centered(
                display, &fonts::headline(), "NO CARDS YET", cx, cy, theme::FG_MUTED,
            );
            fonts::draw_centered(
                display, &fonts::caption(), "present a card to save it",
                cx, cy + 40, theme::FG_MUTED,
            );
            return;
        }

        // The banner sits above the rows and scrolls with them: it is
        // part of the column, so a long list still reaches the top.
        let viewport = list_viewport(&data.safe_area);
        let content_h = offset + lib.cards.len() as i32 * card_row_h();
        render_scrolled(
            display, self.scroll.offset(), viewport, content_h, ACCENT, ctx,
            |clip, scroll| {
                if let Some(b) = &banner {
                    draw_banner(clip, content_top - scroll, b);
                }
                for (i, meta) in lib.cards.iter().enumerate() {
                    let rect = list_row_rect(i, scroll, offset, &data.safe_area);
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
                let offset = self.banner(data).map_or(0, |_| BANNER_H);
                // Index and rect mirror the render loop exactly, or draw
                // and hit-test drift apart.
                for (i, meta) in data.card_library.cards.iter().enumerate() {
                    let rect = list_row_rect(i, scroll, offset, &data.safe_area);
                    if rect.contains(pt) {
                        let mut id: CardId = Vec::new();
                        let _ = id.extend_from_slice(meta.identity.id_bytes());
                        self.view = NfcView::Detail(id.clone());
                        self.confirm_remove = false;
                        self.detail_scroll = ScrollState::new();
                        self.selected_sector = None;
                        self.banner_dismissed = true;
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
                let offset = self.banner(data).map_or(0, |_| BANNER_H);
                let content_h = offset + data.card_library.cards.len() as i32 * card_row_h();
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
        let lay = detail_layout(
            meta, &data.safe_area, key_lines(data, card.id_bytes()), type2_pages(data, meta),
        );
        let viewport = detail_viewport(&data.safe_area);
        // The summary slot, only when it is this card's.
        let slot = data
            .nfc_summary
            .as_ref()
            .filter(|s| s.id.as_slice() == card.id_bytes())
            .map(|s| &s.summary);
        let summary = slot.and_then(|s| s.classic());
        let type2 = slot.and_then(|s| s.type2());
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
                // A Type 2 card with a summary names its exact chip.
                let mut sub_buf: String<32> = String::new();
                let sub: &str = match type2 {
                    Some(t2) => {
                        let _ = write!(sub_buf, "{}  {} B", t2.chip.label(), t2.chip.user_bytes());
                        sub_buf.as_str()
                    }
                    None if meta.label.is_empty() => card.technology().label(),
                    None => card.label(),
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

                // -- Technology section ---------------------------------
                let Some(grid) = lay.grid else {
                    return;
                };
                let gx = SIDE_MARGIN;
                let (tag, unit) = match grid {
                    Grid::Sectors(_) => ("SECTORS", "blk"),
                    Grid::Pages(_) => ("PAGES", "pg"),
                };
                tag_label(clip, gx, sy(lay.sectors_tag_y), tag, ACCENT, NOTCH);
                // Completeness beside the tag: units read / total,
                // from the record (no summary needed).
                if meta.has_dump {
                    line.clear();
                    let _ = write!(line, "{}/{} {}", meta.dump_blocks_read, dump_total(meta), unit);
                    fonts::draw_right(
                        clip, &fonts::caption(), line.as_str(),
                        theme::SCREEN_W as i32 - SIDE_MARGIN, sy(lay.sectors_tag_y) + 2,
                        theme::FG_MUTED,
                    );
                }

                // -- Type 2: PAGES grid + status + PASSWORD ------------
                if let Grid::Pages(n) = grid {
                    for i in 0..n as usize {
                        let cell = grid.cell_rect(i, lay.grid_top, scroll);
                        draw_page_cell(clip, cell, type2.and_then(|t| t.page(i)), selected == Some(i as u8));
                    }
                    line.clear();
                    let status_color = match selected {
                        Some(p) => {
                            page_status_line(&mut line, p, type2.and_then(|t| t.page(p as usize)));
                            theme::FG
                        }
                        None => {
                            let _ = if type2.is_some() {
                                write!(line, "TAP A PAGE")
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
                    if let Some(t2) = type2 {
                        line.clear();
                        password_line(&mut line, t2);
                        fonts::draw_at(
                            clip, &fonts::caption(), line.as_str(), gx + 4, sy(lay.pwd_y), theme::FG_MUTED,
                        );
                    }
                    return;
                }

                // -- Classic: SECTORS grid + status + KEYS -------------
                for i in 0..grid.count() {
                    let cell = grid.cell_rect(i, lay.grid_top, scroll);
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
                        detail_layout(m, &data.safe_area, key_lines(data, &id), type2_pages(data, m))
                            .content_h,
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
                    let lay = detail_layout(
                        m, &data.safe_area, key_lines(data, &id), type2_pages(data, m),
                    );
                    let pt = Point::new(*x as i32, *y as i32);
                    if let Some(grid) = lay.grid {
                        if detail_viewport(&data.safe_area).contains(pt) {
                            let scroll = self.detail_scroll.offset();
                            for i in 0..grid.count() {
                                if grid.cell_rect(i, lay.grid_top, scroll).contains(pt) {
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
    fn on_mount(&mut self, data: &SystemData) {
        // An identified scan re-mounts the screen with the card to
        // open in `nfc_open_card`: land on that card's detail. Anything
        // else (app-drawer open, unrecognized scan, library full) lands
        // on the list, scrolled to the top. A plain wake does not
        // re-mount, so it keeps whatever view was open.
        self.view = match &data.nfc_open_card {
            Some(id) if data.card_library.get_by_id(id).is_some() => NfcView::Detail(id.clone()),
            _ => NfcView::List,
        };
        self.scroll = ScrollState::new();
        self.confirm_remove = false;
        self.detail_scroll = ScrollState::new();
        self.selected_sector = None;
        self.banner_dismissed = false;
    }

    fn render<D: BlendTarget>(&self, display: &mut D, data: &SystemData, ctx: &RenderCtx) {
        // Header telemetry reflects the active view: "CARDS n/cap" on
        // the list (stored / capacity), "CARD i/n" on a detail where i
        // is the card's 1-based position (newest = 1), "LABEL" in the
        // label editor. A detail or editor whose card has vanished
        // falls back to the list, so its label does too.
        let count = data.card_library.len();
        let mut telem: String<12> = String::new();
        match &self.view {
            NfcView::Detail(id) => {
                match data.card_library.cards.iter().position(|c| c.identity.id_bytes() == id.as_slice()) {
                    Some(i) => {
                        let _ = write!(telem, "CARD {}/{}", i + 1, count);
                    }
                    None => {
                        let _ = write!(telem, "CARDS {}/{}", count, crate::card_library::MAX_CARDS);
                    }
                }
            }
            NfcView::EditLabel(id) if data.card_library.get_by_id(id).is_some() => {
                let _ = write!(telem, "LABEL");
            }
            NfcView::EditLabel(_) | NfcView::List => {
                let _ = write!(telem, "CARDS {}/{}", count, crate::card_library::MAX_CARDS);
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
    matches!(
        card,
        CardIdentity::Iso14443a(a)
            if a.kind.is_mifare_classic() || a.kind == crate::nfc::CardKind::MifareUltralight
    )
}

/// The record's completeness denominator: `dump_total` once a dump
/// has stored it; for a Classic record dumped before that field
/// existed, the kind's block count.
fn dump_total(meta: &CardMeta) -> u16 {
    if meta.dump_total != 0 {
        return meta.dump_total;
    }
    match &meta.identity {
        CardIdentity::Iso14443a(a) => a.kind.classic_blocks(),
        _ => 0,
    }
}

// -- Detail layout (one source for render AND hit-test) ----------------------

/// Y positions of the detail column, in unscrolled screen coordinates
/// (subtract the scroll offset to draw). `sectors_total` is `None` for
/// a non-Classic card, whose column ends after the last-seen line.
struct DetailLayout {
    panel_top: i32,
    seen_y: i32,
    /// The technology section's grid, `None` for a card without one.
    grid: Option<Grid>,
    sectors_tag_y: i32,
    grid_top: i32,
    status_y: i32,
    keys_tag_y: i32,
    keys_top: i32,
    /// Type 2 only: the PASSWORD line.
    pwd_y: i32,
    /// Total column height, for the scroll range.
    content_h: i32,
}

/// The cell grid of the technology section: Classic sectors (16 per
/// line, 16 px cells) or Type 2 pages (24 per line, 10 px cells, so an
/// NTAG216's 231 pages take ten lines).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grid {
    Sectors(u8),
    Pages(u8),
}

impl Grid {
    fn count(self) -> usize {
        match self {
            Grid::Sectors(n) | Grid::Pages(n) => n as usize,
        }
    }

    fn per_line(self) -> usize {
        match self {
            Grid::Sectors(_) => GRID_PER_LINE,
            Grid::Pages(_) => PAGE_GRID_PER_LINE,
        }
    }

    fn pitch(self) -> i32 {
        match self {
            Grid::Sectors(_) => GRID_PITCH,
            Grid::Pages(_) => PAGE_GRID_PITCH,
        }
    }

    fn cell(self) -> i32 {
        match self {
            Grid::Sectors(_) => GRID_CELL,
            Grid::Pages(_) => PAGE_GRID_CELL,
        }
    }

    fn lines(self) -> i32 {
        self.count().div_ceil(self.per_line()) as i32
    }

    /// Rect of cell `i`, shifted by the scroll offset.
    fn cell_rect(self, i: usize, grid_top: i32, scroll: i32) -> Rectangle {
        let col = (i % self.per_line()) as i32;
        let row = (i / self.per_line()) as i32;
        Rectangle::new(
            Point::new(SIDE_MARGIN + col * self.pitch(), grid_top + row * self.pitch() - scroll),
            Size::new(self.cell() as u32, self.cell() as u32),
        )
    }
}

/// How many KEYS lines the column shows for this card: one per key in
/// the summary slot when it belongs to the card, else one (the "-" /
/// "NONE" placeholder).
fn key_lines(data: &SystemData, id: &[u8]) -> usize {
    data.nfc_summary
        .as_ref()
        .filter(|s| s.id.as_slice() == id)
        .and_then(|s| s.summary.classic())
        .map(|c| c.keys.len())
        .unwrap_or(0)
        .max(1)
}

/// `key_lines` is the number of KEYS rows to reserve (see
/// [`key_lines`]); the column ends right after them.
/// `key_lines` is the number of KEYS rows to reserve (Classic, see
/// [`key_lines`]); `pages` the Type 2 page count to lay out (see
/// [`type2_pages`]). The column ends right after its last section.
fn detail_layout(meta: &CardMeta, safe: &SafeArea, key_lines: usize, pages: u8) -> DetailLayout {
    let top = app_content_top(safe);
    let panel_top = top + 8;
    let seen_y = panel_top + PANEL_H + 14;
    let grid = match &meta.identity {
        CardIdentity::Iso14443a(a) if a.kind.is_mifare_classic() => {
            Some(Grid::Sectors(a.kind.classic_sectors()))
        }
        CardIdentity::Iso14443a(a) if a.kind == crate::nfc::CardKind::MifareUltralight => {
            Some(Grid::Pages(pages))
        }
        _ => None,
    };
    let Some(grid) = grid else {
        return DetailLayout {
            panel_top,
            seen_y,
            grid: None,
            sectors_tag_y: 0,
            grid_top: 0,
            status_y: 0,
            keys_tag_y: 0,
            keys_top: 0,
            pwd_y: 0,
            content_h: seen_y + 14 + COLUMN_END_PAD - top,
        };
    };
    let sectors_tag_y = seen_y + 26;
    let grid_top = sectors_tag_y + TAG_LABEL_H + 10;
    let status_y = grid_top + grid.lines() * grid.pitch() + 4;
    match grid {
        Grid::Sectors(_) => {
            let keys_tag_y = status_y + 26;
            let keys_top = keys_tag_y + TAG_LABEL_H + 10;
            let keys_h = key_lines.max(1) as i32 * KEY_LINE_H;
            DetailLayout {
                panel_top,
                seen_y,
                grid: Some(grid),
                sectors_tag_y,
                grid_top,
                status_y,
                keys_tag_y,
                keys_top,
                pwd_y: 0,
                content_h: keys_top + keys_h + COLUMN_END_PAD - top,
            }
        }
        Grid::Pages(_) => {
            let pwd_y = status_y + 22;
            DetailLayout {
                panel_top,
                seen_y,
                grid: Some(grid),
                sectors_tag_y,
                grid_top,
                status_y,
                keys_tag_y: 0,
                keys_top: 0,
                pwd_y,
                content_h: pwd_y + 14 + COLUMN_END_PAD - top,
            }
        }
    }
}

/// The Type 2 page count to lay out: the summary's chip when the slot
/// is this card's, else the record's dump total (a dump was stored,
/// the summary is still loading), else 0 (no grid).
fn type2_pages(data: &SystemData, meta: &CardMeta) -> u8 {
    data.nfc_summary
        .as_ref()
        .filter(|s| s.id.as_slice() == meta.identity.id_bytes())
        .and_then(|s| s.summary.type2())
        .map(|t| t.chip.pages())
        .unwrap_or(meta.dump_total.min(u8::MAX as u16) as u8)
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

/// One page cell of a Type 2 grid: green fill read, green with a dark
/// centre dot read but write-locked, red outline behind the password,
/// dim outline not read. The selected cell gets an accent ring.
fn draw_page_cell<D: BlendTarget>(
    d: &mut D, cell: Rectangle, state: Option<crate::type2::PageState>, selected: bool,
) {
    use crate::type2::PageState;
    let x = cell.top_left.x;
    let y = cell.top_left.y;
    let w = cell.size.width as i32;
    let h = cell.size.height as i32;
    match state {
        Some(PageState::Read) => d.fill_blend(x, y, w, h, theme::OK, 255),
        Some(PageState::Locked) => {
            d.fill_blend(x, y, w, h, theme::OK, 255);
            d.fill_blend(x + w / 2 - 1, y + h / 2 - 1, 2, 2, theme::BG, 255);
        }
        Some(PageState::Protected) => {
            cell.into_styled(PrimitiveStyle::with_stroke(theme::DANGER, 1)).draw(d).ok();
        }
        Some(PageState::NotRead) | None => {
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

/// The status line for a tapped page: "P004  READ", "P004  READ
/// LOCKED", "P225  PROTECTED", "P010  NOT READ".
fn page_status_line(out: &mut String<48>, page: u8, state: Option<crate::type2::PageState>) {
    use crate::type2::PageState;
    let _ = write!(out, "P{:03}  ", page);
    let _ = match state {
        Some(PageState::Read) => write!(out, "READ"),
        Some(PageState::Locked) => write!(out, "READ  LOCKED"),
        Some(PageState::Protected) => write!(out, "PROTECTED"),
        Some(PageState::NotRead) | None => write!(out, "NOT READ"),
    };
}

/// The PASSWORD line of a Type 2 detail, from the summary's config.
fn password_line(out: &mut String<48>, t2: &crate::type2::Type2Summary) {
    match t2.config {
        Some(c) if c.protects(t2.chip.pages()) => {
            let _ = write!(
                out, "PASSWORD  from P{:03}, {}",
                c.auth0, if c.prot { "read+write" } else { "write" },
            );
            if c.authlim != 0 {
                let _ = write!(out, ", {} tries", c.authlim);
            }
        }
        Some(_) => {
            let _ = write!(out, "PASSWORD  none");
        }
        None => {
            let _ = write!(out, "PASSWORD  config unreadable");
        }
    }
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
            dump_total: 64,
            seq: 1,
        }
    }

    #[test]
    fn grid_fits_the_panel_width_for_every_classic() {
        let safe = SafeArea::default();
        for kind in [CardKind::MifareClassicMini, CardKind::MifareClassic1K, CardKind::MifareClassic4K] {
            let lay = detail_layout(&meta(kind), &safe, 1, 0);
            let grid = lay.grid.unwrap();
            for i in 0..grid.count() {
                let r = grid.cell_rect(i, lay.grid_top, 0);
                let right = r.top_left.x + r.size.width as i32;
                assert!(right <= theme::SCREEN_W as i32 - SIDE_MARGIN, "{kind:?} cell {i}");
            }
            // The 4K needs three lines; the 1K one.
            let lines = (lay.status_y - 4 - lay.grid_top) / GRID_PITCH;
            assert_eq!(lines, grid.lines());
        }
    }

    #[test]
    fn page_grid_fits_and_ntag216_takes_ten_lines() {
        let safe = SafeArea::default();
        // An NTAG216 (231 pages) at 24 per line: ten lines, all cells
        // inside the panel width, PASSWORD line last.
        let lay = detail_layout(&meta(CardKind::MifareUltralight), &safe, 1, 231);
        let grid = lay.grid.unwrap();
        assert_eq!(grid, Grid::Pages(231));
        assert_eq!(grid.lines(), 10);
        for i in 0..231 {
            let r = grid.cell_rect(i, lay.grid_top, 0);
            let right = r.top_left.x + r.size.width as i32;
            assert!(right <= theme::SCREEN_W as i32 - SIDE_MARGIN, "page cell {i}");
        }
        assert!(lay.pwd_y > lay.status_y);
        assert_eq!(lay.content_h, lay.pwd_y + 14 + COLUMN_END_PAD - app_content_top(&safe));
        // No dump yet and no summary: an empty grid, short column.
        let none = detail_layout(&meta(CardKind::MifareUltralight), &safe, 1, 0);
        assert_eq!(none.grid, Some(Grid::Pages(0)));
        assert!(none.content_h < lay.content_h);
    }

    #[test]
    fn page_status_and_password_lines_format() {
        use crate::type2::{PageState, Type2Chip, Type2Config, Type2Summary};
        let mut s: String<48> = String::new();
        page_status_line(&mut s, 4, Some(PageState::Locked));
        assert_eq!(s.as_str(), "P004  READ  LOCKED");
        s.clear();
        page_status_line(&mut s, 225, Some(PageState::Protected));
        assert_eq!(s.as_str(), "P225  PROTECTED");
        s.clear();
        let mut t2 = Type2Summary::begin(Type2Chip::Ntag216);
        t2.config = Some(Type2Config { auth0: 0xE1, prot: true, cfglck: false, authlim: 3 });
        password_line(&mut s, &t2);
        assert_eq!(s.as_str(), "PASSWORD  from P225, read+write, 3 tries");
        s.clear();
        t2.config = Some(Type2Config { auth0: 0xFF, prot: false, cfglck: false, authlim: 0 });
        password_line(&mut s, &t2);
        assert_eq!(s.as_str(), "PASSWORD  none");
        s.clear();
        t2.config = None;
        password_line(&mut s, &t2);
        assert_eq!(s.as_str(), "PASSWORD  config unreadable");
    }

    #[test]
    fn column_ends_after_the_last_key() {
        let safe = SafeArea::default();
        let viewport_h = detail_viewport(&safe).size.height as i32;
        // A 1K with one key fits without scrolling, and the column
        // ends right after that key line (no reserved empty space).
        let one_k = detail_layout(&meta(CardKind::MifareClassic1K), &safe, 1, 0);
        assert!(one_k.content_h <= viewport_h);
        assert_eq!(
            one_k.content_h,
            one_k.keys_top + KEY_LINE_H + COLUMN_END_PAD - app_content_top(&safe),
        );
        // More keys, longer column, one line each.
        let three = detail_layout(&meta(CardKind::MifareClassic1K), &safe, 3, 0);
        assert_eq!(three.content_h - one_k.content_h, 2 * KEY_LINE_H);
        // A 4K with many keys scrolls.
        let four_k = detail_layout(&meta(CardKind::MifareClassic4K), &safe, 6, 0);
        assert!(four_k.content_h > viewport_h);
        // DESFire: no grid, short column.
        let desfire = detail_layout(&meta(CardKind::MifareDesfire), &safe, 1, 0);
        assert!(desfire.grid.is_none());
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

/// Height of the scan banner above the list rows: headline + caption.
const BANNER_H: i32 = 64;

/// What the list says about the last scan, above its rows.
enum ListBanner {
    /// The tap answered but no technology identified it.
    Unrecognized,
    /// Identified, but the library is at capacity: this card was not
    /// stored and has no row.
    Full(CardIdentity),
}

/// Draw the scan banner with its top edge at `top`.
fn draw_banner<D: BlendTarget>(d: &mut D, top: i32, banner: &ListBanner) {
    let x = SIDE_MARGIN + 4;
    match banner {
        ListBanner::Unrecognized => {
            fonts::draw_at(d, &fonts::headline(), "CARD NOT RECOGNIZED", x, top + 8, theme::WARN);
            fonts::draw_at(
                d, &fonts::caption(), "unsupported or unreadable", x, top + 42, theme::FG_MUTED,
            );
        }
        ListBanner::Full(card) => {
            fonts::draw_at(d, &fonts::headline(), "LIBRARY FULL", x, top + 8, theme::WARN);
            let mut line: String<48> = String::new();
            let _ = write!(line, "{} [{}] not stored", card.short_label(), id_hex(card.id_bytes()));
            fonts::draw_at(d, &fonts::caption(), line.as_str(), x, top + 42, theme::FG_MUTED);
        }
    }
}

/// Rect for the Nth list row, shifted by the scroll offset and pushed
/// down by `offset` (the banner's height when one is up). Width leaves
/// a scrollbar gutter on the right, like the settings rows.
fn list_row_rect(
    index: usize, scroll: i32, offset: i32, safe: &crate::data::SafeArea,
) -> Rectangle {
    let h = card_row_h();
    let y = app_content_top(safe) + offset + index as i32 * h - scroll;
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
    if is_dumpable(&meta.identity) {
        if meta.has_dump {
            let _ = write!(s, "  DUMP {}/{}", meta.dump_blocks_read, dump_total(meta));
        } else {
            let _ = write!(s, "  NO DUMP");
        }
    }
    s
}
