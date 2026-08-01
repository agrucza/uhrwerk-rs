//! board.rs - pin assignments and board-level constants for the
//! LilyGo T-Watch Ultra (ESP32-S3, 16MB QSPI NOR flash + 8MB QSPI PSRAM).
//!
//! Sources: `T-Watch Ultra V1.0 SCH 25-07-24.pdf` (LilyGoLib repo) and
//! arduino-esp32 `variants/lilygo_twatch_ultra/pins_arduino.h`, checked
//! against each other block-by-block.
//!
//! These constants are a reference table for the hardware layout. They
//! are intentionally kept feature-complete even when nothing currently
//! imports them - any new subsystem that needs a pin number should be
//! able to pull it from here instead of re-deriving it from the
//! schematic.
//!
//! ## Memory note
//!
//! The PSRAM on this board is the APS6404L: 8 MB **quad**-SPI SDR, not
//! the octal (OPI) part the Waveshare S3 carries. It is left entirely
//! uninitialized - all boards run the internal-SRAM heap + static
//! framebuffer model. If PSRAM is ever revived here (e.g. large audio
//! buffers for the voice-assistant work), the esp-hal config must be
//! quad mode; the old S3 octal init would not work, and quad bandwidth
//! is about half of octal, so keep bulk-throughput users (framebuffer)
//! out of it.

#![allow(dead_code)]

// =============================================================================
// Display - CO5300 AMOLED, QSPI interface
// Resolution: 410 x 502 px (portrait), same "206" panel family as the
// Waveshare boards (column offset 22 - already baked into the shared
// CO5300 driver).
//
// Panel power (VCI) is double-gated: AXP2101 ALDO2 supplies the rail
// and XL9555 P07 (net VCI_EN) enables it - both must be on before the
// panel responds. Reset is a real GPIO on this board.
// =============================================================================
pub const LCD_SDIO0:  u8 = 38;  // QSPI Data 0
pub const LCD_SDIO1:  u8 = 39;  // QSPI Data 1
pub const LCD_SDIO2:  u8 = 42;  // QSPI Data 2
pub const LCD_SDIO3:  u8 = 45;  // QSPI Data 3
pub const LCD_SCLK:   u8 = 40;  // Serial clock
pub const LCD_CS:     u8 = 41;  // Chip select (active low)
pub const LCD_RESET:  u8 = 37;  // Reset (active low)
pub const LCD_TE:     u8 = 6;   // Tear enable

pub const LCD_WIDTH:  u16 = 410;
pub const LCD_HEIGHT: u16 = 502;

// =============================================================================
// Shared I2C bus
// Devices: CST9217 (touch), XL9555 (expander), BHI260AP (IMU),
// AXP2101 (PMU), PCF85063A (RTC), DRV2605 (haptic)
// =============================================================================
pub const I2C_SDA: u8 = 3;
pub const I2C_SCL: u8 = 2;

pub const TOUCH_I2C_ADDR:    u8 = 0x1A;
pub const EXPANDER_I2C_ADDR: u8 = 0x20;
pub const IMU_I2C_ADDR:      u8 = 0x28;
pub const PMU_I2C_ADDR:      u8 = 0x34;
pub const RTC_I2C_ADDR:      u8 = 0x51;
pub const HAPTIC_I2C_ADDR:   u8 = 0x5A;

// =============================================================================
// Touch controller - CST9217 (I2C, shared bus)
// Reset is NOT a GPIO - it runs through XL9555 P10 (see expander block).
// The vendor driver probes 0x1A first and falls back to 0x5A for other
// CST92xx variants; 0x5A would collide with the DRV2605, so expect 0x1A.
// =============================================================================
pub const TOUCH_INT: u8 = 12;   // Interrupt (active low)

// =============================================================================
// IMU - Bosch BHI260AP (I2C, shared bus)
// Smart sensor hub - needs firmware upload at init, unlike the QMI8658.
// =============================================================================
pub const IMU_INT: u8 = 8;

// =============================================================================
// RTC - PCF85063A (I2C, shared bus)
// Same chip as both existing boards; INT is a real GPIO (S3-like wake
// path).
// =============================================================================
pub const RTC_INT: u8 = 1;

// =============================================================================
// Power management - AXP2101 (I2C, shared bus)
// Unlike both Waveshare boards, the IRQ line IS routed to a readable
// GPIO - PMU events (power key short/long press, VBUS, charge state)
// can be interrupt-driven instead of polled.
//
// Rail map (schematic + vendor doc, all confirmed):
//   DC1        ESP32-S3            3.3V   (never touch)
//   LDO1/VRTC  GPS backup          3.3V   (can't be turned off)
//   ALDO1      SD card             3.3V
//   ALDO2      Display (VCI)       3.3V   (ANDed with XL9555 VCI_EN)
//   ALDO3      LoRa                3.3V   (chip parked in cold sleep at boot)
//   ALDO4      Sensor (BHI260AP)   1.8V
//   BLDO1      GPS                 3.3V   (off at boot - u-blox hw backup mode)
//   BLDO2      Speaker (MAX98357A) 3.3V
//   DLDO1      NFC                 3.3V   (never enabled - chip undriven)
//   VBACKUP    RTC button battery  3.3V
//   DC2-DC5, CPUSLDO: unused
// =============================================================================
pub const PMU_IRQ: u8 = 7;

// =============================================================================
// Buttons
// =============================================================================

/// Boot button - GPIO0, active-low. Also enters download mode.
/// Usable as wake source (low level).
pub const BTN_BOOT: u8 = 0;

/// PWR button - connected to the AXP2101 PWRON pin only, no direct
/// GPIO. Press events arrive via the PMU IRQ line (GPIO7) - actually
/// readable on this board, so short/long press becomes a real input.

// =============================================================================
// Shared SPI bus (SD card + LoRa + NFC)
// =============================================================================
pub const SPI_MOSI: u8 = 34;
pub const SPI_MISO: u8 = 33;
pub const SPI_SCK:  u8 = 35;

pub const SD_CS:    u8 = 21;    // SD detect is XL9555 P12, not a GPIO

pub const LORA_CS:   u8 = 36;   // SX1262 (this unit's radio variant)
pub const LORA_RST:  u8 = 47;   // pulsed once at boot (cold-sleep park)
pub const LORA_BUSY: u8 = 48;   // high while the chip sleeps (20k pull)
pub const LORA_IRQ:  u8 = 14;

pub const NFC_CS:  u8 = 4;      // ST25R3916
pub const NFC_IRQ: u8 = 5;

// =============================================================================
// GPS - UBlox MIA-M10Q (UART)
// =============================================================================
pub const GPS_TX:  u8 = 43;     // ESP TX -> GPS RX
pub const GPS_RX:  u8 = 44;     // GPS TX -> ESP RX
pub const GPS_PPS: u8 = 13;

// =============================================================================
// Audio
// Speaker: MAX98357A Class-D amp on plain I2S - no codec chip, no I2C
// init, no MCLK. Mic: TDK T3902 PDM (not an I2S-slave codec).
// =============================================================================
pub const I2S_BCLK: u8 = 9;
pub const I2S_WS:   u8 = 10;    // LRCLK / WCLK
pub const I2S_DOUT: u8 = 11;

pub const PDM_MIC_CLK:  u8 = 17;
pub const PDM_MIC_DATA: u8 = 18;

// =============================================================================
// GPIO expander - XL9555 @ 0x20, load-bearing for bring-up.
//
// Pin constants are the driver's linear index 0-15 (0-7 = datasheet
// P00-P07, 8-15 = P10-P17) - the same convention as the vendor
// libraries. Verified three ways: SensorLib register math, vendor
// pins_arduino.h, schematic nets.
// =============================================================================

/// Haptic driver enable - P06, schematic net M_EN, gates DRV2605 EN.
pub const EXP_DRV_EN: u8 = 6;
/// Display power enable - P07, net VCI_EN. Must be high (with ALDO2 on)
/// before the panel answers.
pub const EXP_DISP_EN: u8 = 7;
/// Touch reset - P10, net TP_RST. Active low; hold high for normal
/// operation.
pub const EXP_TOUCH_RST: u8 = 8;
/// SD card detect - P12, net SD_DET. Input; LOW = card inserted.
pub const EXP_SD_DET: u8 = 10;
/// LoRa antenna RF switch select - P13, net LORA_SEL. Routing, not
/// an enable: HIGH = built-in LoRa antenna (vendor default), LOW =
/// RF to the USB-connector path. Deliberately left unconfigured:
/// the XL9555's internal ~100 k pull-up parks it HIGH, i.e. the
/// default route. The LoRa effort drives it explicitly.
pub const EXP_LORA_RF_SW: u8 = 11;

// =============================================================================
// Flash filesystem region (LittleFS)
//
// This bin owns its partition geometry. Keep in sync with the
// `storage` row in `partitions-twatch-ultra.csv` (0x810000, 0x7E0000) - the
// shared `system_core::flash_fs` takes these as a `FlashRegion` and
// has no board identity of its own.
// =============================================================================
pub const FLASH_FS_START: u32 = 0x0081_0000; // byte offset
pub const FLASH_FS_SIZE:  u32 = 0x007E_0000; // 7.875 MB
