//! On-screen QWERTY keyboard - free-text entry (passphrases first).
//!
//! A stateful widget in the `Wheel`/`Picker` family: the host
//! sub-view owns an instance, forwards events, and reacts to the
//! returned [`KeyboardResult`]. Rendering fills the content band
//! below the header (the host draws its own header); geometry
//! derives from `theme::` so all key rows sit inside the
//! corner-safe band.
//!
//! Layers: lowercase / uppercase (sticky shift, double-tap latches
//! caps lock) / two symbol pages that together cover every printable
//! ASCII character - a WPA2 passphrase may contain any of them, so
//! full coverage is a correctness requirement, not polish.
//!
//! Password masking: the newest character stays readable until the
//! next keypress or the next 1 Hz `TimeUpdated` tick, then collapses
//! to a dot; the SHOW/HIDE action reveals the whole buffer. DONE
//! renders ghosted while the buffer is empty and drops the tap (the
//! disabled-button rule: visual state and hit-test always agree).

use embedded_graphics::{
    geometry::{Point, Size},
    prelude::Primitive,
    primitives::{PrimitiveStyle, Rectangle},
    Drawable,
};
use crate::ui::types::BlendTarget;
use heapless::String;

use crate::events::SystemEvent;
use crate::ui::{fonts, theme};

// -- geometry ----------------------------------------------------------------

/// Text field: full width at the top of the corner-safe band.
const FIELD_Y: i32 = theme::CONTENT_TOP + 6;
const FIELD_H: i32 = 48;

/// Five rows of keys fill the rest of the band bottom-up.
const ROW_H: i32 = 46;
const ROW_PITCH: i32 = 48;
const KEY_GAP: i32 = 2;
const ROWS_TOP: i32 = theme::CONTENT_BOTTOM - 5 * ROW_PITCH + KEY_GAP;

/// Width of the shift / backspace keys flanking the third row.
const SPECIAL_W: i32 = 57;
/// Width of the layer-switch key on the space row.
const LAYER_W: i32 = 100;
/// Action row: CANCEL | SHOW/HIDE | DONE.
const ACTION_W: i32 = 134;
const EYE_W: i32 = (theme::SCREEN_W as i32) - 2 * ACTION_W - 2 * KEY_GAP;

// -- layers ------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layer {
    Lower,
    Upper,
    Sym1,
    Sym2,
}

/// The three character rows of a layer. Every entry is a single
/// character; specials (shift/backspace/layer/space) are separate.
fn layer_rows(layer: Layer) -> [&'static [char]; 3] {
    match layer {
        Layer::Lower => [
            &['q', 'w', 'e', 'r', 't', 'y', 'u', 'i', 'o', 'p'],
            &['a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l'],
            &['z', 'x', 'c', 'v', 'b', 'n', 'm'],
        ],
        Layer::Upper => [
            &['Q', 'W', 'E', 'R', 'T', 'Y', 'U', 'I', 'O', 'P'],
            &['A', 'S', 'D', 'F', 'G', 'H', 'J', 'K', 'L'],
            &['Z', 'X', 'C', 'V', 'B', 'N', 'M'],
        ],
        // Together with Sym2: all 32 printable-ASCII symbols plus
        // the digits. Duplicates across pages are fine; absences are
        // not (passphrase coverage).
        Layer::Sym1 => [
            &['1', '2', '3', '4', '5', '6', '7', '8', '9', '0'],
            &['-', '/', ':', ';', '(', ')', '$', '&', '@', '"'],
            &['.', ',', '?', '!', '\'', '*', '+'],
        ],
        Layer::Sym2 => [
            &['[', ']', '{', '}', '#', '%', '^', '=', '*', '+'],
            &['_', '\\', '|', '~', '<', '>', '`'],
            &['.', ',', '?', '!', '\'', '"', ';'],
        ],
    }
}

impl Layer {
    fn is_letters(self) -> bool {
        matches!(self, Layer::Lower | Layer::Upper)
    }

    /// Label of the third-row left special: shift on letter layers,
    /// symbol-page toggle on symbol layers.
    fn shift_label(self) -> &'static str {
        match self {
            Layer::Lower | Layer::Upper => "^",
            Layer::Sym1 => "#+=",
            Layer::Sym2 => "?12",
        }
    }

    /// Label of the layer key on the space row.
    fn layer_label(self) -> &'static str {
        if self.is_letters() { "?123" } else { "abc" }
    }
}

// -- results -----------------------------------------------------------------

/// What the host should do after an event was consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardResult {
    /// Event wasn't for the keyboard (host may handle it itself).
    None,
    /// Visible state changed - redraw.
    Changed,
    /// DONE tapped with a non-empty buffer.
    Done,
    /// CANCEL tapped.
    Cancelled,
}

// -- widget ------------------------------------------------------------------

pub struct Keyboard {
    buffer: String<64>,
    /// Semantic length cap (<= 64), set by the host: 63 for WPA2
    /// passphrases, 32 for SSIDs.
    max_len: usize,
    layer: Layer,
    /// Sticky shift: one uppercase character, then back to lower.
    /// Double-tapping shift latches caps lock until tapped again.
    caps_lock: bool,
    /// SHOW/HIDE state - true renders the buffer in the clear.
    reveal: bool,
    /// Newest character still readable (masks on the next
    /// `TimeUpdated` tick or keypress).
    tail_visible: bool,
}

impl Keyboard {
    pub fn new(max_len: usize) -> Self {
        Self {
            buffer: String::new(),
            max_len: max_len.min(64),
            layer: Layer::Lower,
            caps_lock: false,
            reveal: false,
            tail_visible: false,
        }
    }

    /// Reset all entry state and seed the buffer (returning hosts
    /// re-open with the previous value).
    pub fn seed(&mut self, text: &str) {
        self.buffer.clear();
        let _ = self.buffer.push_str(text);
        self.layer = Layer::Lower;
        self.caps_lock = false;
        self.reveal = false;
        self.tail_visible = false;
    }

    pub fn text(&self) -> &str {
        self.buffer.as_str()
    }

    // -- events --------------------------------------------------------------

    pub fn handle_event(&mut self, event: &SystemEvent) -> KeyboardResult {
        match event {
            // 1 Hz mask tick: collapse the readable tail character.
            SystemEvent::TimeUpdated { .. } => {
                if self.tail_visible {
                    self.tail_visible = false;
                    KeyboardResult::Changed
                } else {
                    KeyboardResult::None
                }
            }
            SystemEvent::Tap { x, y } => self.tap(*x as i32, *y as i32),
            _ => KeyboardResult::None,
        }
    }

    fn tap(&mut self, x: i32, y: i32) -> KeyboardResult {
        // Character rows share one geometry source with render.
        let rows = layer_rows(self.layer);
        for (row_idx, row) in rows.iter().enumerate() {
            for (key_idx, ch) in row.iter().enumerate() {
                if contains(key_rect(self.layer, row_idx, key_idx), x, y) {
                    return self.type_char(*ch);
                }
            }
        }

        if contains(shift_rect(), x, y) {
            return match self.layer {
                Layer::Lower => {
                    self.layer = Layer::Upper;
                    self.caps_lock = false;
                    KeyboardResult::Changed
                }
                // Second tap on shift while already uppercase =
                // caps lock; a third releases it.
                Layer::Upper => {
                    if self.caps_lock {
                        self.caps_lock = false;
                        self.layer = Layer::Lower;
                    } else {
                        self.caps_lock = true;
                    }
                    KeyboardResult::Changed
                }
                Layer::Sym1 => {
                    self.layer = Layer::Sym2;
                    KeyboardResult::Changed
                }
                Layer::Sym2 => {
                    self.layer = Layer::Sym1;
                    KeyboardResult::Changed
                }
            };
        }

        if contains(backspace_rect(), x, y) {
            self.tail_visible = false;
            return if self.buffer.pop().is_some() {
                KeyboardResult::Changed
            } else {
                KeyboardResult::None
            };
        }

        if contains(layer_key_rect(), x, y) {
            self.layer = if self.layer.is_letters() {
                Layer::Sym1
            } else {
                Layer::Lower
            };
            self.caps_lock = false;
            return KeyboardResult::Changed;
        }

        if contains(space_rect(), x, y) {
            return self.type_char(' ');
        }

        if contains(cancel_rect(), x, y) {
            return KeyboardResult::Cancelled;
        }
        if contains(eye_rect(), x, y) {
            self.reveal = !self.reveal;
            return KeyboardResult::Changed;
        }
        if contains(done_rect(), x, y) {
            // Ghosted while empty - the tap must die here too.
            return if self.buffer.is_empty() {
                KeyboardResult::None
            } else {
                KeyboardResult::Done
            };
        }

        KeyboardResult::None
    }

    fn type_char(&mut self, ch: char) -> KeyboardResult {
        if self.buffer.len() >= self.max_len {
            return KeyboardResult::None;
        }
        let _ = self.buffer.push(ch);
        self.tail_visible = true;
        // Sticky shift releases after one character unless latched.
        if self.layer == Layer::Upper && !self.caps_lock {
            self.layer = Layer::Lower;
        }
        KeyboardResult::Changed
    }

    // -- render --------------------------------------------------------------

    pub fn render<D: BlendTarget>(&self, display: &mut D) {
        self.render_field(display);

        let rows = layer_rows(self.layer);
        for (row_idx, row) in rows.iter().enumerate() {
            for (key_idx, ch) in row.iter().enumerate() {
                let mut label: String<4> = String::new();
                let _ = label.push(*ch);
                draw_key(
                    display,
                    key_rect(self.layer, row_idx, key_idx),
                    label.as_str(),
                    KeyStyle::Char,
                );
            }
        }

        // Shift shows latched caps as hot.
        let shift_style = if self.layer == Layer::Upper {
            if self.caps_lock { KeyStyle::Hot } else { KeyStyle::Active }
        } else {
            KeyStyle::Special
        };
        draw_key(display, shift_rect(), self.layer.shift_label(), shift_style);
        draw_key(display, backspace_rect(), "DEL", KeyStyle::Special);
        draw_key(
            display,
            layer_key_rect(),
            self.layer.layer_label(),
            KeyStyle::Special,
        );
        draw_key(display, space_rect(), "", KeyStyle::Char);

        draw_key(display, cancel_rect(), "CANCEL", KeyStyle::Special);
        let eye = if self.reveal { "HIDE" } else { "SHOW" };
        draw_key(display, eye_rect(), eye, KeyStyle::Special);
        let done_style = if self.buffer.is_empty() {
            KeyStyle::Ghost
        } else {
            KeyStyle::Hot
        };
        draw_key(display, done_rect(), "DONE", done_style);
    }

    fn render_field<D: BlendTarget>(&self, display: &mut D) {
        let rect = field_rect();
        Rectangle::new(rect.top_left, rect.size)
            .into_styled(PrimitiveStyle::with_fill(theme::BG))
            .draw(display)
            .ok();
        Rectangle::new(rect.top_left, rect.size)
            .into_styled(PrimitiveStyle::with_stroke(theme::FG_DIM, 1))
            .draw(display)
            .ok();

        if self.buffer.is_empty() {
            fonts::draw_centered_in_rect(
                display,
                &fonts::caption(),
                "ENTER PASSPHRASE",
                rect,
                theme::FG_DIM,
            );
            return;
        }

        // Masked: dots for everything except (optionally) the tail.
        let mut shown: String<64> = String::new();
        if self.reveal {
            let _ = shown.push_str(self.buffer.as_str());
        } else {
            let n = self.buffer.len();
            for i in 0..n {
                if i + 1 == n && self.tail_visible {
                    let tail = self.buffer.chars().last().unwrap_or('*');
                    let _ = shown.push(tail);
                } else {
                    let _ = shown.push('*');
                }
            }
        }
        // Overlong content: show the newest end (the part being
        // edited), prefixed to signal truncation.
        let text = tail_window(shown.as_str(), 24);
        fonts::draw_centered_in_rect(
            display,
            &fonts::value(),
            text,
            rect,
            theme::FG,
        );
    }
}

/// Last `max_chars` of `s` (char-safe; ASCII here anyway).
fn tail_window(s: &str, max_chars: usize) -> &str {
    let count = s.chars().count();
    if count <= max_chars {
        return s;
    }
    let skip = count - max_chars;
    let (idx, _) = s.char_indices().nth(skip).unwrap_or((0, ' '));
    &s[idx..]
}

// -- key drawing -------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyStyle {
    /// Plain character key.
    Char,
    /// Function key (shift / DEL / layer / CANCEL / SHOW).
    Special,
    /// Shift while uppercase is armed.
    Active,
    /// Caps lock latched, or DONE ready.
    Hot,
    /// DONE with nothing to submit - rendered dim, tap dropped.
    Ghost,
}

fn draw_key<D: BlendTarget>(
    display: &mut D,
    rect: Rectangle,
    label: &str,
    style: KeyStyle,
) {
    let (fill, border, fg) = match style {
        KeyStyle::Char => (theme::INK_3, theme::STEEL, theme::FG),
        KeyStyle::Special => (theme::BG, theme::STEEL, theme::FG_MUTED),
        KeyStyle::Active => (theme::INK_3, theme::SIGNAL, theme::SIGNAL),
        KeyStyle::Hot => (theme::SIGNAL_DEEP, theme::SIGNAL, theme::SIGNAL),
        KeyStyle::Ghost => (theme::BG, theme::FG_DIM, theme::FG_DIM),
    };
    Rectangle::new(rect.top_left, rect.size)
        .into_styled(PrimitiveStyle::with_fill(fill))
        .draw(display)
        .ok();
    Rectangle::new(rect.top_left, rect.size)
        .into_styled(PrimitiveStyle::with_stroke(border, 1))
        .draw(display)
        .ok();
    if !label.is_empty() {
        fonts::draw_centered_in_rect(display, &fonts::body(), label, rect, fg);
    }
}

// -- rects (single source for render AND hit-test) ---------------------------

fn contains(rect: Rectangle, x: i32, y: i32) -> bool {
    rect.contains(Point::new(x, y))
}

fn row_y(row: usize) -> i32 {
    ROWS_TOP + row as i32 * ROW_PITCH
}

pub fn field_rect() -> Rectangle {
    Rectangle::new(
        Point::new(6, FIELD_Y),
        Size::new(theme::SCREEN_W as u32 - 12, FIELD_H as u32),
    )
}

/// Rect of character key `key_idx` in `row_idx` of `layer`. Rows
/// 0/1: evenly spread, centered. Row 2: centered between the shift
/// and backspace specials.
fn key_rect(layer: Layer, row_idx: usize, key_idx: usize) -> Rectangle {
    let n = layer_rows(layer)[row_idx].len() as i32;
    let (span_x, span_w) = if row_idx == 2 {
        (
            SPECIAL_W + KEY_GAP,
            (theme::SCREEN_W as i32) - 2 * (SPECIAL_W + KEY_GAP),
        )
    } else {
        (0, theme::SCREEN_W as i32)
    };
    let w = (span_w - (n - 1) * KEY_GAP) / n;
    let used = n * w + (n - 1) * KEY_GAP;
    let x0 = span_x + (span_w - used) / 2;
    Rectangle::new(
        Point::new(x0 + key_idx as i32 * (w + KEY_GAP), row_y(row_idx)),
        Size::new(w as u32, ROW_H as u32),
    )
}

fn shift_rect() -> Rectangle {
    Rectangle::new(
        Point::new(0, row_y(2)),
        Size::new(SPECIAL_W as u32, ROW_H as u32),
    )
}

fn backspace_rect() -> Rectangle {
    Rectangle::new(
        Point::new((theme::SCREEN_W as i32) - SPECIAL_W, row_y(2)),
        Size::new(SPECIAL_W as u32, ROW_H as u32),
    )
}

fn layer_key_rect() -> Rectangle {
    Rectangle::new(
        Point::new(0, row_y(3)),
        Size::new(LAYER_W as u32, ROW_H as u32),
    )
}

fn space_rect() -> Rectangle {
    Rectangle::new(
        Point::new(LAYER_W + KEY_GAP, row_y(3)),
        Size::new(
            ((theme::SCREEN_W as i32) - LAYER_W - KEY_GAP) as u32,
            ROW_H as u32,
        ),
    )
}

fn cancel_rect() -> Rectangle {
    Rectangle::new(
        Point::new(0, row_y(4)),
        Size::new(ACTION_W as u32, ROW_H as u32),
    )
}

fn eye_rect() -> Rectangle {
    Rectangle::new(
        Point::new(ACTION_W + KEY_GAP, row_y(4)),
        Size::new(EYE_W as u32, ROW_H as u32),
    )
}

fn done_rect() -> Rectangle {
    Rectangle::new(
        Point::new((theme::SCREEN_W as i32) - ACTION_W, row_y(4)),
        Size::new(ACTION_W as u32, ROW_H as u32),
    )
}

// -- host tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tap_at(kb: &mut Keyboard, rect: Rectangle) -> KeyboardResult {
        let c = rect.top_left
            + Point::new(rect.size.width as i32 / 2, rect.size.height as i32 / 2);
        kb.handle_event(&SystemEvent::Tap {
            x: c.x as u16,
            y: c.y as u16,
        })
    }

    fn tap_char(kb: &mut Keyboard, ch: char) -> KeyboardResult {
        let rows = layer_rows(kb.layer);
        for (ri, row) in rows.iter().enumerate() {
            for (ki, c) in row.iter().enumerate() {
                if *c == ch {
                    return tap_at(kb, key_rect(kb.layer, ri, ki));
                }
            }
        }
        panic!("char {ch:?} not on current layer");
    }

    #[test]
    fn typing_appends_chars() {
        let mut kb = Keyboard::new(63);
        assert_eq!(tap_char(&mut kb, 'h'), KeyboardResult::Changed);
        assert_eq!(tap_char(&mut kb, 'i'), KeyboardResult::Changed);
        assert_eq!(kb.text(), "hi");
    }

    #[test]
    fn sticky_shift_uppercases_one_char() {
        let mut kb = Keyboard::new(63);
        tap_at(&mut kb, shift_rect());
        assert_eq!(kb.layer, Layer::Upper);
        tap_char(&mut kb, 'A');
        assert_eq!(kb.layer, Layer::Lower);
        tap_char(&mut kb, 'b');
        assert_eq!(kb.text(), "Ab");
    }

    #[test]
    fn double_shift_latches_caps() {
        let mut kb = Keyboard::new(63);
        tap_at(&mut kb, shift_rect());
        tap_at(&mut kb, shift_rect());
        assert!(kb.caps_lock);
        tap_char(&mut kb, 'A');
        tap_char(&mut kb, 'B');
        assert_eq!(kb.text(), "AB");
        assert_eq!(kb.layer, Layer::Upper);
        // Third tap releases.
        tap_at(&mut kb, shift_rect());
        assert!(!kb.caps_lock);
        assert_eq!(kb.layer, Layer::Lower);
    }

    #[test]
    fn symbol_pages_reachable_and_cover_ascii() {
        let mut kb = Keyboard::new(63);
        tap_at(&mut kb, layer_key_rect());
        assert_eq!(kb.layer, Layer::Sym1);
        tap_at(&mut kb, shift_rect());
        assert_eq!(kb.layer, Layer::Sym2);
        tap_at(&mut kb, layer_key_rect());
        assert_eq!(kb.layer, Layer::Lower);

        // Every printable ASCII character must be reachable on some
        // layer (passphrase coverage).
        for code in 0x20u8..=0x7e {
            let ch = code as char;
            if ch == ' ' {
                continue; // space key
            }
            let found = [Layer::Lower, Layer::Upper, Layer::Sym1, Layer::Sym2]
                .iter()
                .any(|l| layer_rows(*l).iter().any(|r| r.contains(&ch)));
            assert!(found, "{ch:?} unreachable");
        }
    }

    #[test]
    fn backspace_and_space() {
        let mut kb = Keyboard::new(63);
        tap_char(&mut kb, 'a');
        tap_at(&mut kb, space_rect());
        tap_char(&mut kb, 'b');
        assert_eq!(kb.text(), "a b");
        tap_at(&mut kb, backspace_rect());
        assert_eq!(kb.text(), "a ");
        // Empty pop is a no-op, not a change.
        tap_at(&mut kb, backspace_rect());
        tap_at(&mut kb, backspace_rect());
        assert_eq!(tap_at(&mut kb, backspace_rect()), KeyboardResult::None);
    }

    #[test]
    fn max_len_caps_input() {
        let mut kb = Keyboard::new(3);
        for _ in 0..5 {
            tap_char(&mut kb, 'x');
        }
        assert_eq!(kb.text(), "xxx");
    }

    #[test]
    fn done_ghosts_while_empty() {
        let mut kb = Keyboard::new(63);
        assert_eq!(tap_at(&mut kb, done_rect()), KeyboardResult::None);
        tap_char(&mut kb, 'a');
        assert_eq!(tap_at(&mut kb, done_rect()), KeyboardResult::Done);
    }

    #[test]
    fn cancel_always_fires() {
        let mut kb = Keyboard::new(63);
        assert_eq!(tap_at(&mut kb, cancel_rect()), KeyboardResult::Cancelled);
    }

    #[test]
    fn mask_tick_hides_tail() {
        let mut kb = Keyboard::new(63);
        tap_char(&mut kb, 'a');
        assert!(kb.tail_visible);
        let r = kb.handle_event(&SystemEvent::TimeUpdated {
            data: crate::data::TimeData::default(),
        });
        assert_eq!(r, KeyboardResult::Changed);
        assert!(!kb.tail_visible);
    }

    #[test]
    fn seed_restores_text_and_resets_modes() {
        let mut kb = Keyboard::new(63);
        tap_at(&mut kb, shift_rect());
        tap_at(&mut kb, shift_rect());
        kb.seed("old-pass");
        assert_eq!(kb.text(), "old-pass");
        assert!(!kb.caps_lock);
        assert_eq!(kb.layer, Layer::Lower);
    }

    #[test]
    fn key_rects_stay_inside_screen_and_band() {
        for layer in [Layer::Lower, Layer::Upper, Layer::Sym1, Layer::Sym2] {
            for (ri, row) in layer_rows(layer).iter().enumerate() {
                for ki in 0..row.len() {
                    let r = key_rect(layer, ri, ki);
                    let right =
                        r.top_left.x + r.size.width as i32;
                    let bottom =
                        r.top_left.y + r.size.height as i32;
                    assert!(r.top_left.x >= 0 && right <= theme::SCREEN_W as i32);
                    assert!(r.top_left.y >= theme::CONTENT_TOP);
                    assert!(bottom <= theme::CONTENT_BOTTOM);
                }
            }
        }
        let bottom_row = done_rect();
        assert!(
            bottom_row.top_left.y + bottom_row.size.height as i32
                <= theme::CONTENT_BOTTOM
        );
    }
}
