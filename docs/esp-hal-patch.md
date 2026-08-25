# Patched esp-hal fork - the C6 sleep freeze

The workspace pins a soft fork of esp-hal via `[patch.crates-io]`:
`https://github.com/agrucza/esp-hal`, branch `uhrwerk-rs` - esp-hal
1.1.0-rc.0 (base commit `347003de`, the exact commit the crates.io
release was built from) plus five changes, four files. The fork
keeps the version number, so every crate in the graph (esp-rtos,
esp-radio, esp-storage, ...) resolves to this one copy. The delta is
always visible as `git diff 347003de..uhrwerk-rs` in the fork.

Upstream syncs are merges of upstream *release commits* (not the dev
head - unreleased version numbers would break cargo's unification
with esp-radio/esp-rtos) into the `uhrwerk-rs` branch, followed by a
`cargo update -p esp-hal` here and a re-flash-verify of all boards.

This file is the investigation record: what the freeze was, how it
was proven, and what the changes do. Changes 1-4 are the C6 sleep
freeze; change 5 is the C6/H2 I2S duplex MCLK fix (our upstream
issue esp-rs/esp-hal#6072, closed as not planned).

## Why

On the ESP32-C6, one WiFi session followed by light sleep freezes the
device: dark, touch does not wake it, plugging USB does not wake it,
only a manual reset recovers it. Without any radio use the same
firmware light-sleeps for 45+ minutes and wakes on touch. A WiFi scan
- no association, no DHCP, no traffic - is enough to trigger it.

The C6 sleep path builds a `SleepTimeConfig` on every `rtc.sleep()`,
re-measuring the RC_FAST_DIV clock there
(`RtcClock::calibrate(RcFastDivClk, 2048)`) to derive the PMU
hardware wait times. Proven mechanism (LP-SRAM breadcrumb, change 3,
captures below): the measurement itself succeeds, but its
clock-node RESTORE re-requests a PLL-derived TIMG node. After a
radio session the deinit has released the PLL's clock-tree
references, so the sleep entry's root-clock switch to XTAL powered
the BBPLL down for real - and the restore's PLL re-enable deadlocks
in unbounded spins (regi2c `while busy`, BBPLL `while !cal_done`)
whose own master clock derives from the PLL being enabled. No panic,
interrupts fine, unwakeable.

The divide-by-zero in `us_to_fastclk` (a `RtcClock::calibrate`
timeout returns 0; upstream #5053 hit it on C6 v0.1 silicon, #5109
made it rarer) was the original suspect. The breadcrumb disproved it
as the freeze mechanism - the fatal cycle never reaches the division
- but changes 1 and 2 stay as hardening for the real timeout case.

## The five changes

1. `src/rtc_cntl/sleep/esp32c6.rs`, `SleepTimeConfig::new`: the
   RC_FAST_DIV calibration is retried up to 3 times; if it keeps
   returning 0, the last value that succeeded is used; if there has
   never been one, a nominal RC_FAST/256 period. Every fallback logs
   (`warn!`/`error!`, visible with esp-hal's `log-04` feature).

2. `src/clock/mod.rs`: `RtcClock::calibrate_rc_fast_div(cycles)`,
   a `pub` (unstable) wrapper over the crate-private `calibrate`, so
   firmware can observe the same value the sleep path divides by.

3. `src/rtc_cntl/sleep/esp32c6.rs`, `pub mod sleep_diag`: a sleep
   breadcrumb in LP SRAM (`.rtc_fast.persistent`). Change 1 did not
   stop the freeze, and USB-Serial-JTAG drops at sleep entry, so a
   panic inside `rtc.sleep()`, a hang during sleep entry, and a wake
   that never fires all look identical from the monitor. LP SRAM
   survives every reset except power-on - NOT the board's PWR
   long-press, which is an AXP power cycle and wipes it; the firmware
   therefore arms the LP watchdog around every sleep (30 s, board
   hook `sleep_watchdog_timeout`) so a stuck sleep self-resets with
   LP SRAM intact and the next boot logs the record. The sleep path
   records the calibration result
   (value, attempts, which fallback) in `SleepTimeConfig::new` and
   sets a marker on each side of the PMU `sleep_req` write in
   `start_sleep`; the firmware arms each cycle before `rtc.sleep()`
   (`C6Board::tune_sleep_config`), closes it right after
   (`C6Board::wake_cause_raw`), and dumps the record at boot
   (`firmware-c6/src/main.rs`). Because the freeze also tears the
   USB-Serial-JTAG tty off the host - no monitor can catch the first
   boot lines after the watchdog reset - a died-mid-cycle record is
   stashed and re-logged every 10 s, and the post-mortem boot holds
   light sleep off (sleeping again would overwrite the record and
   drop the tty). Attach the monitor whenever and wait one interval,
   or Ctrl+R for the full boot log; the device stays awake until
   reflash or power-cycle. Post-mortem mode triggers ONLY when the
   reset reason is the LP watchdog (`SysRtcWdt`) - the one way a real
   freeze ends. Any other reset usually lands mid-sleep (the device
   sleeps most of the time) and would fake the same mid-cycle
   signature; those log one "interrupted sleep cycle - not a freeze"
   line and sleep normally.

   The boot line ("DIED MID-SLEEP-CYCLE") names the last checkpoint
   the fatal cycle passed; the `STAGE_*` constants in `sleep_diag`
   (src/rtc_cntl/sleep/esp32c6.rs) document every checkpoint and what
   being stuck there means. Checkpoints straddle each step of sleep
   entry: firmware pre-sleep, wake-source config, the SOC root clock
   switch to XTAL, the PMU power writes, the RC_FAST_DIV calibration,
   and the PMU `sleep_req` halt itself. Evidence (all 2026-08-24,
   watchdog rescue verified - `SysRtcWdt`, record intact): capture 1
   died between `probe done` and the calibration; capture 2 died
   inside the in-sleep RC_FAST_DIV calibration (CAL_ENTER); capture 3,
   with the CAL_* sub-checkpoints, died at CAL_MEASURED - the
   measurement SUCCEEDED and the death is in the post-measure
   clock-node restore. Root cause, per source: the restore
   re-requests a PLL-derived TIMG node; after a radio session the
   deinit has released the PLL's references, so the XTAL root switch
   at sleep entry powered the BBPLL down for real, and the re-enable
   (`enable_pll_clk_impl(true)`, soc/esp32c6/clocks.rs) deadlocks in
   unbounded spins: `while busy` in regi2c and `while !cal_done` at
   clocks.rs:192 - with the analog-I2C master clock itself derived
   from the PLL being enabled. Pre-radio sleeps never hit this
   because a PLL reference always remains and the restore is a no-op.

4. `src/rtc_cntl/sleep/esp32c6.rs`: `provide_fastclk_period(p)` - the
   FIX for the freeze. The firmware's pre-sleep probe measures
   RC_FAST_DIV while the PLL is still the root clock (safe) and hands
   the value in; `SleepTimeConfig::new` consumes it and skips the
   in-sleep measurement entirely - no clock-node walk, no restore, no
   PLL touch during sleep entry. The breadcrumb marks a provided
   value as `cal_attempts == 0`. If nothing was provided (the probe's
   own measurement timed out and returned 0), the old measuring path
   runs unchanged, with the sleep watchdog as the net. This mirrors
   esp-idf, which calibrates outside the sleep path; the in-sleep
   measure-and-restore is the upstream design flaw. Upstream report
   material: the unbalanced PLL refcount at radio deinit, the two
   unbounded spins, and the PLL-enable path clocking its own regi2c
   from the PLL.

5. `src/i2s/master.rs` (C6/H2 family): bind the I2S MCLK output to
   the TX divider. `set_rx_clock` claimed the MCLK pin
   unconditionally; TX and RX run independent fractional dividers,
   so full duplex left MCLK incoherent with BCK/WS - external codecs
   on a shared MCLK (ES8311 + ES7210) saw per-session corrupted
   capture and stuttering playback. Now `set_tx_clock` binds MCLK to
   the TX divider (ESP-IDF `i2s_ll_mclk_bind_to_tx_clk`) and
   `set_rx_clock` claims it only while the TX clock is disabled -
   order-independent, RX-only unaffected. This is our upstream issue
   esp-rs/esp-hal#6072 (closed as not planned); firmware-c6's
   `tune_i2s` previously poked the same PCR bit and now only sets
   `sig_loopback` (the BCK/WS share, kept bin-side because esp-hal's
   `signal_loopback` config also flips `rx_slave_mod`, which is not
   the verified register state).

Everything else in the fork is byte-identical to the registry crate.
Retire the fork and the `[patch.crates-io]` block as soon as
upstream releases carry equivalents of BOTH the sleep fix and the
I2S MCLK binding (the breadcrumb is diagnostic scaffolding and goes
with the sleep fix).
