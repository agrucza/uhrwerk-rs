# uhrwerk-rs

One movement, different cases: Rust-native (`no_std`, esp-hal +
embassy) firmware for ESP32 AMOLED smartwatches. One shared,
board-agnostic system layer (`system-core`: manager, tasks, UI model,
storage, audio) runs on three watches through a `Board` trait plus
per-chip driver seams; each `firmware-*` bin crate only supplies pins,
board bring-up, and its chip choices. Long-term goal: a push-to-talk
Home Assistant voice assistant (Wyoming over TCP).

## Workspace

- `app-core` - hardware-free application model, screens, events
- `system-core` - shared system layer (manager, tasks, storage, audio)
- `firmware-hal` - shared display HAL (CO5300 QSPI AMOLED)
- `drivers` - chip drivers (feature-gated)
- `firmware-s3` / `firmware-c6` / `firmware-twatch-ultra` - board bins

## Hardware status

| Subsystem | Waveshare ESP32-S3-Touch-AMOLED-2.06 | Waveshare ESP32-C6-Touch-AMOLED-2.06 | LilyGo T-Watch Ultra |
|---|---|---|---|
| Display (CO5300 QSPI AMOLED 410x502) | working | working | working |
| Touch | working (FT3168) | working (FT3168) | working (CST9217/9220) |
| PMU (AXP2101) | working | working | working |
| RTC + alarms/timers (PCF85063) | working | working | working (backup cell keeps time through power-off) |
| Light sleep / wake | working | working | working |
| Storage - internal flash | working | working | working |
| Storage - SD card | working | no slot | working (hotplug via card detect) |
| Speaker | working (ES8311) | working (ES8311) | working (MAX98357A) |
| Microphone | working (ES7210) | working (ES7210) | working (T3902 PDM) |
| IMU + motion wake | working (QMI8658) | working (QMI8658) | working (BHI260AP) |
| Wrist-wear detection (raise/lower) | n/a | n/a | working |
| Haptics | no motor populated | no motor | working (DRV2605) |
| GPIO expander (XL9555) | n/a | n/a | working |
| LoRa (SX1262) | n/a | n/a | parked in cold sleep (driver not started) |
| GPS (MIA-M10Q) | n/a | n/a | working (rail-gated time sync, settings UI) |
| NFC (ST25R3916) | n/a | n/a | parked, rail off (driver not started) |
| WiFi / networking | not started | not started | not started |
