pub mod alarm;
pub mod app_drawer;
pub mod clock;
pub mod notifications;
pub mod quick_access;
pub mod settings;
pub mod stopwatch;
pub mod timer;

use crate::ui::types::BlendTarget;

use crate::events::SystemEvent;
use super::types::{Action, DirtyRegion, RenderCtx, Screen, ScreenId, SystemData};

/// Enum-based screen dispatch - avoids dynamic dispatch and heap allocation.
///
/// Add new screen variants here as they're created.
pub enum ActiveScreen {
    Clock(clock::ClockScreen),
    Stopwatch(stopwatch::StopwatchScreen),
    Timer(timer::TimerScreen),
    Alarm(alarm::AlarmScreen),
    Settings(settings::SettingsScreen),
    /// Pull-down Quick Access overlay. Reached via swipe-down-from-top.
    QuickAccess(quick_access::QuickAccessScreen),
    /// Pull-up App Drawer. Reached via swipe-up-from-bottom and via
    /// tapping the watch face.
    AppDrawer(app_drawer::AppDrawerScreen),
    /// Global ALERTS overlay. Reached via left-edge swipe-right.
    Notifications(notifications::NotificationsScreen),
}

impl ActiveScreen {
    /// Create a fresh screen for the given id.
    ///
    /// Note: `ScreenId::QuickAccess` and `ScreenId::AppDrawer` can't
    /// be constructed this way - both overlays need a `previous:
    /// ScreenId` context that plain id-based construction can't
    /// supply. Use `new_quick_access(previous)` or
    /// `new_app_drawer(previous)` instead.
    pub fn new(id: ScreenId) -> Self {
        match id {
            ScreenId::Clock => Self::Clock(clock::ClockScreen::new()),
            ScreenId::Stopwatch => Self::Stopwatch(stopwatch::StopwatchScreen::new()),
            ScreenId::Timer => Self::Timer(timer::TimerScreen::new()),
            ScreenId::Alarm => Self::Alarm(alarm::AlarmScreen::new()),
            ScreenId::Settings => Self::Settings(settings::SettingsScreen::new()),
            ScreenId::QuickAccess => {
                debug_assert!(false,
                    "use ActiveScreen::new_quick_access(previous) for QuickAccess");
                Self::Clock(clock::ClockScreen::new())
            }
            ScreenId::AppDrawer => {
                debug_assert!(false,
                    "use ActiveScreen::new_app_drawer(previous) for AppDrawer");
                Self::Clock(clock::ClockScreen::new())
            }
            ScreenId::Notifications => {
                Self::Notifications(notifications::NotificationsScreen::new())
            }
        }
    }

    /// Create the Quick Access overlay, remembering which screen it
    /// should return to on close.
    pub fn new_quick_access(previous: ScreenId) -> Self {
        Self::QuickAccess(quick_access::QuickAccessScreen::new(previous))
    }

    /// Create the App Drawer overlay, remembering which screen it
    /// should return to on close.
    pub fn new_app_drawer(previous: ScreenId) -> Self {
        Self::AppDrawer(app_drawer::AppDrawerScreen::new(previous))
    }

    pub fn render<D: BlendTarget>(
        &self,
        display: &mut D,
        data: &SystemData,
        ctx: &RenderCtx,
    ) {
        match self {
            Self::Clock(s) => s.render(display, data, ctx),
            Self::Stopwatch(s) => s.render(display, data, ctx),
            Self::Timer(s) => s.render(display, data, ctx),
            Self::Alarm(s) => s.render(display, data, ctx),
            Self::Settings(s) => s.render(display, data, ctx),
            Self::QuickAccess(s) => s.render(display, data, ctx),
            Self::AppDrawer(s) => s.render(display, data, ctx),
            Self::Notifications(s) => s.render(display, data, ctx),
        }
    }

    pub fn on_event(&mut self, event: &SystemEvent, data: &mut SystemData) -> Action {
        match self {
            Self::Clock(s) => s.on_event(event, data),
            Self::Stopwatch(s) => s.on_event(event, data),
            Self::Timer(s) => s.on_event(event, data),
            Self::Alarm(s) => s.on_event(event, data),
            Self::Settings(s) => s.on_event(event, data),
            Self::QuickAccess(s) => s.on_event(event, data),
            Self::AppDrawer(s) => s.on_event(event, data),
            Self::Notifications(s) => s.on_event(event, data),
        }
    }

    pub fn mount(&mut self, data: &SystemData) {
        match self {
            Self::Clock(s) => s.on_mount(data),
            Self::Stopwatch(s) => s.on_mount(data),
            Self::Timer(s) => s.on_mount(data),
            Self::Alarm(s) => s.on_mount(data),
            Self::Settings(s) => s.on_mount(data),
            Self::QuickAccess(s) => s.on_mount(data),
            Self::AppDrawer(s) => s.on_mount(data),
            Self::Notifications(s) => s.on_mount(data),
        }
    }

    pub fn unmount(&mut self) {
        match self {
            Self::Clock(s) => s.on_unmount(),
            Self::Stopwatch(s) => s.on_unmount(),
            Self::Timer(s) => s.on_unmount(),
            Self::Alarm(s) => s.on_unmount(),
            Self::Settings(s) => s.on_unmount(),
            Self::QuickAccess(s) => s.on_unmount(),
            Self::AppDrawer(s) => s.on_unmount(),
            Self::Notifications(s) => s.on_unmount(),
        }
    }

    /// Ask the active screen which regions need re-rendering this frame.
    /// Forwarded to the variant's [`Screen::dirty_rects`].
    pub fn dirty_rects(&self, data: &SystemData) -> DirtyRegion {
        match self {
            Self::Clock(s) => s.dirty_rects(data),
            Self::Stopwatch(s) => s.dirty_rects(data),
            Self::Timer(s) => s.dirty_rects(data),
            Self::Alarm(s) => s.dirty_rects(data),
            Self::Settings(s) => s.dirty_rects(data),
            Self::QuickAccess(s) => s.dirty_rects(data),
            Self::AppDrawer(s) => s.dirty_rects(data),
            Self::Notifications(s) => s.dirty_rects(data),
        }
    }

    /// Tell the active screen the frame was rendered with `data`, so it
    /// can update its "last rendered" snapshot. Forwarded to the
    /// variant's [`Screen::clear_dirty`].
    pub fn clear_dirty(&mut self, data: &SystemData) {
        match self {
            Self::Clock(s) => s.clear_dirty(data),
            Self::Stopwatch(s) => s.clear_dirty(data),
            Self::Timer(s) => s.clear_dirty(data),
            Self::Alarm(s) => s.clear_dirty(data),
            Self::Settings(s) => s.clear_dirty(data),
            Self::QuickAccess(s) => s.clear_dirty(data),
            Self::AppDrawer(s) => s.clear_dirty(data),
            Self::Notifications(s) => s.clear_dirty(data),
        }
    }

    /// Which screen is currently active.
    pub fn id(&self) -> ScreenId {
        match self {
            Self::Clock(_) => ScreenId::Clock,
            Self::Stopwatch(_) => ScreenId::Stopwatch,
            Self::Timer(_) => ScreenId::Timer,
            Self::Alarm(_) => ScreenId::Alarm,
            Self::Settings(_) => ScreenId::Settings,
            Self::QuickAccess(_) => ScreenId::QuickAccess,
            Self::AppDrawer(_) => ScreenId::AppDrawer,
            Self::Notifications(_) => ScreenId::Notifications,
        }
    }

    /// Switch to a different screen. Constructs a fresh instance of
    /// the target screen and runs its mount hook. Not valid for the
    /// overlay screens; use `open_quick_access` / `open_app_drawer`.
    pub fn switch_to(&mut self, id: ScreenId, data: &SystemData) {
        self.unmount();
        *self = Self::new(id);
        self.mount(data);
    }

    /// Open the Quick Access overlay.
    pub fn open_quick_access(&mut self, previous: ScreenId, data: &SystemData) {
        self.unmount();
        *self = Self::new_quick_access(previous);
        self.mount(data);
    }

    /// Open the App Drawer overlay.
    pub fn open_app_drawer(&mut self, previous: ScreenId, data: &SystemData) {
        self.unmount();
        *self = Self::new_app_drawer(previous);
        self.mount(data);
    }

    /// Open the Notifications overlay. Notifications doesn't need
    /// a `previous` context the way Quick Access / App Drawer do
    /// (it isn't a launcher and doesn't highlight the source app),
    /// but we keep the call site shape uniform with the other two
    /// global edge-gesture overlays.
    pub fn open_notifications(&mut self, data: &SystemData) {
        self.unmount();
        *self = Self::Notifications(notifications::NotificationsScreen::new());
        self.mount(data);
    }
}
