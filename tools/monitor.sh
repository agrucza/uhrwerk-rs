#!/usr/bin/env bash
# Passive serial monitor for all boards (USB-Serial-JTAG). Attaches
# WITHOUT touching the DTR/RTS strapping lines, so it never resets
# the chip into download mode (espflash's attach does - use espflash
# only for flashing and deliberate Ctrl+R resets). The tty vanishes
# whenever a board hardware-light-sleeps and reappears on wake; this
# rides out or reattaches across those gaps.
#
# Usage: tools/monitor.sh [port]
# Default port: first Espressif USB-JTAG device by stable id. With
# more than one board plugged in, pass the exact by-id path.
# Exit: Ctrl-t q under tio (release Ctrl before the q); Ctrl+C in
# the cat fallback.

PORT="${1:-$(ls /dev/serial/by-id/usb-Espressif_USB_JTAG_serial_debug_unit* 2>/dev/null | head -1)}"
if [ -z "$PORT" ]; then
    echo "no Espressif USB-JTAG device found and no port given" >&2
    exit 1
fi

# Prefer tio when installed: it asserts DTR on open (the S3's older
# USB-Serial-JTAG block stays silent for a bare cat, the C6's newer
# one does not care), never performs the esptool reset dance, and
# reconnects on its own when the tty vanishes and returns. Exit with
# Ctrl-t q.
if command -v tio >/dev/null; then
    # INLCRNL: the firmware emits bare \n; map to \r\n so lines don't
    # stair-step in the raw terminal.
    exec tio --map INLCRNL "$PORT"
fi

echo "watching $PORT (Ctrl+C to exit)"

trap 'echo; exit 0' INT
while :; do
    until [ -e "$PORT" ]; do sleep 0.2; done
    echo "--- attached $(date +%T) ---"
    # raw: no line mangling; -hupcl: don't drop control lines on
    # close. Baud is ignored by USB CDC but stty wants one.
    stty -F "$PORT" raw -echo -hupcl 115200 2>/dev/null || { sleep 0.2; continue; }
    cat "$PORT"
    echo "--- disconnected $(date +%T) ---"
    sleep 0.2
done
