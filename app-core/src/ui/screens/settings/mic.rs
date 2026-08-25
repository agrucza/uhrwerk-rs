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

/// MicTest sub-view layout: (level panel, RECORD button, PLAY
/// button, TONES button). Shared by render and hit-testing so the
/// tap targets always match what's drawn.
fn mic_test_slots(
    safe: &crate::data::SafeArea,
) -> (Rectangle, Rectangle, Rectangle, Rectangle) {
    let mut s = layout::VStack::new(leaf_top_y(safe));
    let panel = s.slot(96);
    s.gap(18);
    let (record, play) = s.pair(36, 12);
    s.gap(12);
    let tones = s.slot(36);
    (panel, record, play, tones)
}

impl SettingsScreen {
    /// Live microphone level meter plus the speaker-side tests. The
    /// bar's fill tracks `data.mic_level` (0..=255), updated from
    /// `SystemEvent::MicLevel` while capture or a clip recording
    /// runs. RECORD captures one ~2 s clip (meter live, speaker
    /// muted); PLAY replays the stored clip once and stays Disabled
    /// until a clip exists; TONES plays the 440/1000/880 Hz sweep.
    /// Each one-shot restarts the meter off its Done event. Together
    /// they prove the mic RX and speaker TX paths on hardware before
    /// any networking is involved.
    pub(super) fn render_mic_test<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "MIC TEST", theme::ACCENT, ctx);

        let (panel, record_rect, play_rect, tones_rect) = mic_test_slots(&data.safe_area);
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

        // RECORD: Primary while a clip recording is in flight.
        if self.mic_recording {
            chamfered_button(
                display, record_rect, "REC...",
                ButtonVariant::Primary, theme::ACCENT,
            );
        } else {
            chamfered_button(
                display, record_rect, "RECORD",
                ButtonVariant::Ghost, theme::BORDER,
            );
        }
        // PLAY: Disabled (and tap-dropped below) until a clip
        // exists; Primary while replaying.
        if !data.has_recording {
            chamfered_button(
                display, play_rect, "PLAY",
                ButtonVariant::Disabled, theme::BORDER,
            );
        } else if self.mic_playing {
            chamfered_button(
                display, play_rect, "PLAYING",
                ButtonVariant::Primary, theme::ACCENT,
            );
        } else {
            chamfered_button(
                display, play_rect, "PLAY",
                ButtonVariant::Ghost, theme::BORDER,
            );
        }
        chamfered_button(
            display, tones_rect, "TONES",
            ButtonVariant::Ghost, theme::BORDER,
        );
    }

    /// Mic-test back / swipe-right both leave to the Index and emit
    /// StopMicTest, which ends whichever audio mode is active.
    /// (Leaving Settings by any other path is caught by the model's
    /// `mic_test` safety net.) RECORD / PLAY / TONES taps drive the
    /// one-shot sessions; each Done event restarts the meter.
    pub(super) fn mic_test_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            SystemEvent::Tap { x, y } if header_back_hit(*x, *y, &data.safe_area) => {
                self.view = SettingsView::Index;
                self.mic_recording = false;
                self.mic_playing = false;
                Action::StopMicTest
            }
            SystemEvent::Tap { x, y } => {
                let (_, record_rect, play_rect, tones_rect) =
                    mic_test_slots(&data.safe_area);
                if rect_hit(record_rect, *x, *y) {
                    // Ignore while a one-shot is already in flight;
                    // re-record is one tap after RecordingDone.
                    if !self.mic_recording && !self.mic_playing {
                        self.mic_recording = true;
                        return Action::RecordClipTest;
                    }
                    return Action::None;
                }
                if rect_hit(play_rect, *x, *y) {
                    // Disabled visual + tap drop are the same
                    // condition: no clip, no action.
                    if data.has_recording && !self.mic_recording && !self.mic_playing {
                        self.mic_playing = true;
                        return Action::PlayClipTest;
                    }
                    return Action::None;
                }
                if rect_hit(tones_rect, *x, *y) {
                    // The sweep interrupts an in-flight one-shot; its
                    // Done event will never come, so clear the flags.
                    self.mic_recording = false;
                    self.mic_playing = false;
                    return Action::PlayToneTest;
                }
                Action::None
            }
            // Each one-shot finished: clear its flag and bring the
            // level meter back.
            SystemEvent::TonesDone => Action::StartMicTest,
            SystemEvent::RecordingDone => {
                self.mic_recording = false;
                Action::StartMicTest
            }
            SystemEvent::PlaybackDone => {
                self.mic_playing = false;
                Action::StartMicTest
            }
            SystemEvent::Swipe {
                dir: crate::events::SwipeDir::Right,
                region: crate::events::SwipeRegion::Content,
                ..
            } => {
                self.view = SettingsView::Index;
                self.mic_recording = false;
                self.mic_playing = false;
                Action::StopMicTest
            }
            _ => Action::None,
        }
    }
}
