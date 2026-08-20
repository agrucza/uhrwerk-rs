//! MIC TEST sub-view: the live capture level meter plus the
//! speaker-side TONES / LOOP tests.

use embedded_graphics::{
    geometry::{Point, Size},
    prelude::Primitive,
    primitives::{PrimitiveStyle, Rectangle},
    Drawable,
};
use crate::ui::types::BlendTarget;
use heapless::String;
use core::fmt::Write;

use crate::ui::{fonts, layout, theme};
use crate::ui::layout::rect_hit;
use crate::ui::types::{Action, RenderCtx, SystemData, SystemEvent};
use crate::ui::widgets::{
    chamfered_button, chamfered_panel, tag_label, ButtonVariant, NOTCH,
};

use super::{draw_header, header_back_hit, leaf_top_y, SettingsScreen, SettingsView};

/// MicTest sub-view layout: (level panel, TONES button, LOOP button).
/// Shared by render and hit-testing so the tap targets always match
/// what's drawn.
fn mic_test_slots(safe: &crate::data::SafeArea) -> (Rectangle, Rectangle, Rectangle) {
    let mut s = layout::VStack::new(leaf_top_y(safe));
    let panel = s.slot(96);
    s.gap(18);
    let (tones, loop_b) = s.pair(36, 12);
    (panel, tones, loop_b)
}

impl SettingsScreen {
    /// Live microphone level meter plus the speaker-side tests. The
    /// bar's fill tracks `data.mic_level` (0..=255), updated from
    /// `SystemEvent::MicLevel` while capture or the LOOP test runs.
    /// TONES plays the 440/1000/880 Hz sweep once (the meter restarts
    /// itself off `TonesDone`); LOOP toggles the record-then-playback
    /// "parrot" test, which replays ~1.0 s mic snippets through the
    /// speaker. Together they prove the ES7210 RX and ES8311 TX paths
    /// on hardware before any networking is involved.
    pub(super) fn render_mic_test<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "MIC TEST", theme::ACCENT, ctx);

        let (panel, tones_rect, loop_rect) = mic_test_slots(&data.safe_area);
        chamfered_panel(display, panel, NOTCH, theme::BORDER, 1);
        tag_label(
            display, panel.top_left.x, panel.top_left.y,
            "LEVEL", theme::BORDER, NOTCH,
        );

        // Track + GREEN fill whose width tracks the live level.
        let inset: i32 = 14;
        let bar = Rectangle::new(
            Point::new(panel.top_left.x + inset, panel.top_left.y + 34),
            Size::new(panel.size.width - (inset as u32) * 2, 26),
        );
        bar.into_styled(PrimitiveStyle::with_fill(theme::SURFACE)).draw(display).ok();
        let fill_w = bar.size.width * data.mic_level as u32 / 255;
        if fill_w > 0 {
            Rectangle::new(bar.top_left, Size::new(fill_w, bar.size.height))
                .into_styled(PrimitiveStyle::with_fill(theme::OK))
                .draw(display)
                .ok();
        }
        bar.into_styled(PrimitiveStyle::with_stroke(theme::FG, 1)).draw(display).ok();

        // Numeric percent under the bar, so a glance confirms capture.
        let pct = (data.mic_level as u32 * 100) / 255;
        let mut buf: String<8> = String::new();
        let _ = write!(buf, "{}%", pct);
        let label_rect = Rectangle::new(
            Point::new(panel.top_left.x, panel.top_left.y + 66),
            Size::new(panel.size.width, 24),
        );
        fonts::draw_centered_in_rect(
            display, &fonts::value(), buf.as_str(), label_rect, theme::FG_DIM,
        );

        // Speaker-side tests: momentary TONES sweep + LOOP toggle,
        // Primary-filled while loopback is live.
        chamfered_button(
            display, tones_rect, "TONES",
            ButtonVariant::Ghost, theme::BORDER,
        );
        if self.mic_loopback {
            chamfered_button(
                display, loop_rect, "LOOP ON",
                ButtonVariant::Primary, theme::ACCENT,
            );
        } else {
            chamfered_button(
                display, loop_rect, "LOOP",
                ButtonVariant::Ghost, theme::BORDER,
            );
        }
    }

    /// Mic-test back / swipe-right both leave to the Index and emit
    /// StopMicTest, which ends whichever audio mode is active.
    /// (Leaving Settings by any other path is caught by the model's
    /// `mic_test` safety net.) TONES / LOOP taps drive the speaker
    /// tests; `TonesDone` restarts the meter the sweep paused.
    pub(super) fn mic_test_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y, &data.safe_area) => {
                self.view = SettingsView::Index;
                self.mic_loopback = false;
                Action::StopMicTest
            }
            SystemEvent::Tap { x, y } => {
                let (_, tones_rect, loop_rect) = mic_test_slots(&data.safe_area);
                if rect_hit(tones_rect, *x, *y) {
                    return Action::PlayToneTest;
                }
                if rect_hit(loop_rect, *x, *y) {
                    self.mic_loopback = !self.mic_loopback;
                    return if self.mic_loopback {
                        Action::StartLoopbackTest
                    } else {
                        Action::StartMicTest
                    };
                }
                Action::None
            }
            SystemEvent::TonesDone => {
                // Sweep finished: restart whichever meter mode the
                // LOOP toggle says was active.
                if self.mic_loopback {
                    Action::StartLoopbackTest
                } else {
                    Action::StartMicTest
                }
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Index;
                self.mic_loopback = false;
                Action::StopMicTest
            }
            _ => Action::None,
        }
    }
}
