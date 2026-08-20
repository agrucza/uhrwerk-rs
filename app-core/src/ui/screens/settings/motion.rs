//! MOTION sub-view.
//!
//! Live IMU + temperature readouts at the top, self-test panels with
//! RUN buttons below. Stacked tall enough to need smooth scrolling.

use embedded_graphics::{
    geometry::{Point, Size},
    primitives::Rectangle,
};
use crate::ui::types::BlendTarget;
use crate::ui::theme::Color;
use core::fmt::Write;

use crate::ui::{fonts, layout, theme, widgets};
use crate::ui::types::{
    Action, RenderCtx, SelfTestId, SelfTestResult, SystemData, SystemEvent,
};
use crate::ui::widgets::{
    chamfered_button, chamfered_panel, handle_scroll_drag, render_scrolled, tag_label,
    ButtonVariant, NOTCH, TAG_LABEL_H,
};

use super::{draw_header, header_back_hit, leaf_top_y, SettingsScreen, SettingsView};

// -- Self-test list ----------------------------------------------------------

struct ImuTestRow {
    label: &'static str,
    id: SelfTestId,
    unit: &'static str,
}

const IMU_TESTS: &[ImuTestRow] = &[
    ImuTestRow {
        label: "ACCEL SELF-TEST",
        id: SelfTestId::ImuAccel,
        unit: "mg",
    },
    ImuTestRow {
        label: "GYRO SELF-TEST",
        id: SelfTestId::ImuGyro,
        unit: "dps",
    },
];

/// Tag-labels for the live readout panels (3 accel axes, 3 gyro
/// axes, 1 environment temperature, plus the raw step-counter total
/// on boards with the steps capability - the layout drops the last
/// entry elsewhere).
const MOTION_LABELS: [&str; 8] = [
    "ACCEL X", "ACCEL Y", "ACCEL Z",
    "GYRO X",  "GYRO Y",  "GYRO Z",
    "TEMP",    "STEPS",
];

/// Height of one live readout panel.
const MOTION_READOUT_H: i32 = 56;
/// Vertical gap between adjacent live readout panels.
const MOTION_READOUT_GAP: i32 = 6;
/// Vertical break between the readouts band and the self-test band.
const MOTION_SECTION_GAP: i32 = 16;

/// IMU `MotionUpdated` arrives at ~20 Hz; only redraw on every Nth
/// sample to keep the MOTION sub-view legible without bottlenecking
/// the render loop. 4 → ~5 Hz redraw cadence.
const MOTION_REDRAW_DIVIDER: u8 = 4;

/// Per-axis change threshold (raw i16 units) below which a fresh
/// `MotionUpdated` is treated as "no visible change" and skipped.
/// Suppresses sensor noise on a still device.
const MOTION_DIFF_THRESHOLD: i32 = 16;

/// Per-temperature change threshold. `temp_raw` is in 1/256 °C so 64
/// raw ≈ 0.25 °C, well above sensor self-heat noise.
const MOTION_TEMP_THRESHOLD: i32 = 64;

/// Self-test panel + button geometry inside the MOTION sub-view.
const MOTION_TEST_PANEL_H: i32 = 80;
const MOTION_TEST_PANEL_BTN_GAP: i32 = 8;
const MOTION_TEST_BUTTON_H: i32 = 36;
const MOTION_INTER_TEST_GAP: i32 = 16;

struct MotionLayout {
    /// One rect per `MOTION_LABELS` entry; the STEPS slot is a
    /// zero-size placeholder when the board lacks the capability.
    readouts: [Rectangle; 8],
    /// How many leading `readouts` entries are real (7 or 8).
    readout_count: usize,
    /// `(panel, button)` pairs in `IMU_TESTS` order.
    tests: [(Rectangle, Rectangle); 2],
    /// Total content height; passed to the smooth-scroll helpers.
    content_h: i32,
}

fn motion_layout(scroll: i32, has_steps: bool, safe: &crate::data::SafeArea) -> MotionLayout {
    let mut s = layout::VStack::new(leaf_top_y(safe) - scroll);

    let r0 = s.slot(MOTION_READOUT_H); s.gap(MOTION_READOUT_GAP);
    let r1 = s.slot(MOTION_READOUT_H); s.gap(MOTION_READOUT_GAP);
    let r2 = s.slot(MOTION_READOUT_H); s.gap(MOTION_READOUT_GAP);
    let r3 = s.slot(MOTION_READOUT_H); s.gap(MOTION_READOUT_GAP);
    let r4 = s.slot(MOTION_READOUT_H); s.gap(MOTION_READOUT_GAP);
    let r5 = s.slot(MOTION_READOUT_H); s.gap(MOTION_READOUT_GAP);
    let r6 = s.slot(MOTION_READOUT_H);
    let r7 = if has_steps {
        s.gap(MOTION_READOUT_GAP);
        s.slot(MOTION_READOUT_H)
    } else {
        Rectangle::zero()
    };
    s.gap(MOTION_SECTION_GAP);

    let p0 = s.slot(MOTION_TEST_PANEL_H);
    s.gap(MOTION_TEST_PANEL_BTN_GAP);
    let b0 = s.slot(MOTION_TEST_BUTTON_H);
    s.gap(MOTION_INTER_TEST_GAP);
    let p1 = s.slot(MOTION_TEST_PANEL_H);
    s.gap(MOTION_TEST_PANEL_BTN_GAP);
    let b1 = s.slot(MOTION_TEST_BUTTON_H);

    let content_h = s.cursor_y() + scroll - leaf_top_y(safe);
    MotionLayout {
        readouts: [r0, r1, r2, r3, r4, r5, r6, r7],
        readout_count: if has_steps { 8 } else { 7 },
        tests: [(p0, b0), (p1, b1)],
        content_h,
    }
}

/// Visible viewport for MOTION's scrollable area: from the row of
/// section content (leaf_top_y) down to just above the home bar.
fn motion_viewport_rect(safe: &crate::data::SafeArea) -> Rectangle {
    widgets::viewport_to_home_bar(leaf_top_y(safe), safe)
}

/// True when at least one axis or the temperature changed by more
/// than the per-channel noise threshold. Used to gate redraws so a
/// motionless device doesn't trigger frames on every IMU sample.
fn motion_changed(prev: &crate::data::MotionData, curr: &crate::data::MotionData) -> bool {
    let pairs = [
        (prev.accel_x, curr.accel_x, MOTION_DIFF_THRESHOLD),
        (prev.accel_y, curr.accel_y, MOTION_DIFF_THRESHOLD),
        (prev.accel_z, curr.accel_z, MOTION_DIFF_THRESHOLD),
        (prev.gyro_x,  curr.gyro_x,  MOTION_DIFF_THRESHOLD),
        (prev.gyro_y,  curr.gyro_y,  MOTION_DIFF_THRESHOLD),
        (prev.gyro_z,  curr.gyro_z,  MOTION_DIFF_THRESHOLD),
        (prev.temp_raw, curr.temp_raw, MOTION_TEMP_THRESHOLD),
    ];
    pairs.iter().any(|(p, c, t)| ((*p as i32) - (*c as i32)).abs() >= *t)
        || prev.steps != curr.steps
}

/// Format the live value for the readout at `idx`, into `buf`.
fn motion_value(idx: usize, data: &SystemData, buf: &mut heapless::String<12>) {
    use core::fmt::Write;
    let m = &data.motion;
    let _ = match idx {
        0 => write!(buf, "{}", m.accel_x),
        1 => write!(buf, "{}", m.accel_y),
        2 => write!(buf, "{}", m.accel_z),
        3 => write!(buf, "{}", m.gyro_x),
        4 => write!(buf, "{}", m.gyro_y),
        5 => write!(buf, "{}", m.gyro_z),
        6 => write!(buf, "{}°C", m.temp_raw / 256),
        // Raw hub total (diagnostic) - the daily figure lives on the
        // clock face. "--" until the first step event arrives.
        7 => match m.steps {
            Some(s) => write!(buf, "{}", s),
            None => write!(buf, "--"),
        },
        _ => Ok(()),
    };
}

fn draw_motion_panel<D: BlendTarget>(
    display: &mut D, rect: Rectangle, tag: &str, value: &str,
) {
    chamfered_panel(display, rect, NOTCH, theme::INFO, 1);
    tag_label(
        display,
        rect.top_left.x, rect.top_left.y,
        tag, theme::INFO, NOTCH,
    );
    let inner = Rectangle::new(
        Point::new(rect.top_left.x, rect.top_left.y + TAG_LABEL_H),
        Size::new(rect.size.width, rect.size.height - TAG_LABEL_H as u32),
    );
    fonts::draw_centered_in_rect(display, &fonts::value(), value, inner, theme::FG);
}

/// Pick the accent color (panel border + tag + result text) for a
/// given IMU test result. Visualises run state at a glance:
/// steel = inactive, signal = running, green = pass, danger =
/// fail/error.
fn imu_result_accent(result: &SelfTestResult) -> Color {
    match result {
        SelfTestResult::NotRun => theme::BORDER,
        SelfTestResult::Running => theme::ACCENT,
        SelfTestResult::PassAxes3(_) => theme::OK,
        SelfTestResult::FailAxes3(_) | SelfTestResult::Error(_) => theme::DANGER,
    }
}

fn format_result(
    result: &SelfTestResult,
    unit: &'static str,
) -> (heapless::String<32>, Color, Option<Color>) {
    let mut buf: heapless::String<32> = heapless::String::new();
    match result {
        SelfTestResult::NotRun => {
            let _ = buf.push_str("--");
            (buf, theme::FG_DIM, None)
        }
        SelfTestResult::Running => {
            let _ = buf.push_str("RUNNING");
            (buf, theme::FG_MUTED, Some(theme::ACCENT))
        }
        SelfTestResult::PassAxes3(v) => {
            let _ = write!(&mut buf, "{} {} {} {}", v[0], v[1], v[2], unit);
            (buf, theme::FG, Some(theme::OK))
        }
        SelfTestResult::FailAxes3(v) => {
            let _ = write!(&mut buf, "{} {} {} {}", v[0], v[1], v[2], unit);
            (buf, theme::DANGER, Some(theme::DANGER))
        }
        SelfTestResult::Error(_) => {
            let _ = buf.push_str("ERROR");
            (buf, theme::DANGER, Some(theme::DANGER))
        }
    }
}

impl SettingsScreen {
    pub(super) fn render_imu<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        draw_header(display, data, "MOTION", theme::ACCENT, ctx);

        let scroll = self.imu_scroll.offset();
        let layout = motion_layout(scroll, data.capabilities.steps, &data.safe_area);

        render_scrolled(
            display, scroll, motion_viewport_rect(&data.safe_area), layout.content_h, theme::ACCENT, ctx,
            |clipped, _| {
                // Live readouts.
                let mut value_buf: heapless::String<12> = heapless::String::new();
                for i in 0..layout.readout_count {
                    value_buf.clear();
                    motion_value(i, data, &mut value_buf);
                    draw_motion_panel(
                        clipped, layout.readouts[i],
                        MOTION_LABELS[i], value_buf.as_str(),
                    );
                }

                // Self-tests: state panel + RUN button per test.
                for (i, test) in IMU_TESTS.iter().enumerate() {
                    let (panel_rect, button_rect) = layout.tests[i];
                    let result = data.self_tests[test.id as usize];
                    let (test_buf, _, _) = format_result(&result, test.unit);
                    let accent = imu_result_accent(&result);

                    chamfered_panel(clipped, panel_rect, NOTCH, accent, 1);
                    tag_label(
                        clipped,
                        panel_rect.top_left.x, panel_rect.top_left.y,
                        test.label, accent, NOTCH,
                    );
                    fonts::draw_centered_in_rect(
                        clipped, &fonts::value(),
                        test_buf.as_str(), panel_rect, accent,
                    );

                    // Ghost while running (drops re-taps); Primary
                    // when idle / finished.
                    let running = matches!(result, SelfTestResult::Running);
                    let variant = if running {
                        ButtonVariant::Ghost
                    } else {
                        ButtonVariant::Primary
                    };
                    chamfered_button(
                        clipped, button_rect, "RUN SELF-TEST",
                        variant, theme::ACCENT,
                    );
                }
            },
        );
    }

    pub(super) fn imu_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match event {
            // Live readouts: throttle the IMU's 20 Hz MotionUpdated to
            // ~5 Hz (every 4th sample) and additionally gate on
            // material change so a still device doesn't churn frames.
            SystemEvent::MotionUpdated { .. } => {
                self.motion_phase = (self.motion_phase + 1) % MOTION_REDRAW_DIVIDER;
                if self.motion_phase != 0 {
                    return Action::None;
                }
                let curr = data.motion;
                let changed = match self.motion_last {
                    None => true,
                    Some(prev) => motion_changed(&prev, &curr),
                };
                if !changed {
                    return Action::None;
                }
                self.motion_last = Some(curr);
                Action::Redraw
            }

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

            // Drag scroll for the live + self-tests stack.
            SystemEvent::TouchPressed { .. } | SystemEvent::TouchReleased => {
                let layout = motion_layout(0, data.capabilities.steps, &data.safe_area);
                let viewport_h = motion_viewport_rect(&data.safe_area).size.height as i32;
                if handle_scroll_drag(
                    &mut self.imu_scroll, event, viewport_h, layout.content_h,
                ) {
                    return Action::Redraw;
                }
                Action::None
            }

            // Self-test button tap.
            SystemEvent::Tap { x, y } => {
                let scroll = self.imu_scroll.offset();
                let layout = motion_layout(scroll, data.capabilities.steps, &data.safe_area);
                let pt = Point::new(*x as i32, *y as i32);
                for (i, test) in IMU_TESTS.iter().enumerate() {
                    let (_, button_rect) = layout.tests[i];
                    if !button_rect.contains(pt) { continue; }
                    // Mirror the Ghost-while-running visual by ignoring
                    // re-taps mid-run.
                    if matches!(data.self_tests[test.id as usize], SelfTestResult::Running) {
                        return Action::None;
                    }
                    return Action::RunSelfTest(test.id);
                }
                Action::None
            }
            SystemEvent::SelfTestUpdated { .. } => Action::Redraw,
            _ => Action::None,
        }
    }
}
