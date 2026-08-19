use app_core::config::DisplayConfig;
use embassy_time::{Duration, Timer};
use esp_hal::gpio::Output;
use firmware_hal::display::{CO5300, EspQspi};

/// Concrete type of the display handle the rest of the firmware uses.
/// Parameterized over the esp-hal peripheral lifetime `'d`; the
/// framebuffer is `'static` because it's leaked out of a Vec at init
/// time in the manager.
pub type Display<'d> = CO5300<'static, EspQspi<'d>, Output<'d>>;

// `DisplayState` lives in `app_core::ui::types` so Model / UI can
// reason about display power state without touching hardware.
// Re-exported here so existing firmware imports keep working.
pub use app_core::ui::types::DisplayState;

// The display init sequence (QSPI bus build, reset pulse, CO5300 init,
// wake, display-on) lives in `firmware-hal` so every board shares one
// implementation. Re-export it here so the existing
// `display::init_display(...)` call site in the manager keeps
// compiling unchanged.
pub use firmware_hal::display::init_display;

/// Wake-from-Off, part 1: `SLPOUT` plus the post-SLPOUT command
/// blackout (datasheet 7.5.12: the chip self-diagnoses and reloads
/// register defaults for 5 ms - commands sent into that window are
/// lost; 10 ms leaves margin), then re-arm the brightness-control
/// block (7.5.40: with BCTRL cleared, DBV brightness values are
/// silently ignored; one command, harmless if already set).
///
/// The panel is now powered but dark (`DISPON` not yet issued) and
/// GRAM accepts writes (7.5.23: RAMWR is available in every mode,
/// Sleep In included) - the caller renders the wake frame here,
/// overlapping the booster's [`SLPOUT_BOOST`] ramp, then calls
/// [`panel_on`]. The panel's first lit frame is current content.
pub async fn panel_wake(display: &mut Display<'_>) {
    display.wake().await;
    Timer::after(Duration::from_millis(10)).await;
    display.enable_brightness_ctrl().await;
}

/// Booster-on time after `SLPOUT` (datasheet 7.5.12 flow chart:
/// "takes 120 ms to become Sleep Out mode (booster on)"). `DISPON`
/// earlier risks an unstabilized first frame; the wake render
/// overlaps this ramp and the caller tops up the remainder before
/// [`panel_on`].
pub const SLPOUT_BOOST: Duration = Duration::from_millis(120);

/// Wake-from-Off, part 2: target brightness, then `DISPON` + its
/// settle. The wake frame is already in GRAM and brightness is set
/// before the panel lights, so the first lit frame is current
/// content at the correct level.
pub async fn panel_on(
    display: &mut Display<'_>,
    to: DisplayState,
    config: &DisplayConfig,
) {
    let brightness = match to {
        DisplayState::Dim => config.brightness_dim,
        _ => config.brightness_active,
    };
    display.set_brightness(brightness).await;
    display.display_on().await;
    Timer::after(Duration::from_millis(70)).await;
    log::info!("display: Off -> {:?}", to);
}

/// Apply a non-wake display-state transition: brightness moves
/// between Active and Dim, and the full `DISPOFF` + `SLPIN`
/// sequence for Off.
///
/// Going to `Off` sends both DISPOFF (stops panel output) and
/// SLPIN (shuts down the panel oscillator + booster) because
/// DISPOFF alone still leaves the panel internal logic running at
/// ~mA. SLPIN drops to panel standby (~uA).
///
/// Waking from Off is deliberately NOT handled here - the manager
/// orchestrates it as [`panel_wake`] -> fresh-time wait + wake-frame
/// render (into the dark GRAM) -> [`panel_on`], so the first lit
/// frame is current content at the correct brightness.
pub async fn transition(
    display: &mut Display<'_>,
    from: DisplayState,
    to: DisplayState,
    config: &DisplayConfig,
) {
    match to {
        DisplayState::Off => {
            display.display_off().await;
            // Small settle between DISPOFF and SLPIN so the panel has
            // finished stopping its output scan before we drop the
            // oscillator. Datasheet requires >= 5 ms after SLPIN before
            // the next command; 10 ms is comfortable either way.
            Timer::after(Duration::from_millis(10)).await;
            display.sleep().await;
            Timer::after(Duration::from_millis(10)).await;
        }
        DisplayState::Active => {
            display.set_brightness(config.brightness_active).await;
        }
        DisplayState::Dim => {
            display.set_brightness(config.brightness_dim).await;
        }
    }
    log::info!("display: {:?} -> {:?}", from, to);
}
