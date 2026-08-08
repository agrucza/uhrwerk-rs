//! Calendar math shared by the time-sync paths (GPS in the bins,
//! WiFi/NTP in this crate): shifting a UTC date/time into watch-local
//! time before it goes to the RTC task as `RtcCommand::SetTime`.

/// Calendar-correct minute-offset shift of a UTC date/time,
/// including day/month/year rollover in both directions.
pub fn add_minutes(
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    min: u8,
    offset: i32,
) -> (u16, u8, u8, u8, u8) {
    let mut y = year as i32;
    let mut mo = month as i32;
    let mut d = day as i32;
    let mut total = hour as i32 * 60 + min as i32 + offset;
    while total < 0 {
        total += 24 * 60;
        d -= 1;
        if d < 1 {
            mo -= 1;
            if mo < 1 {
                mo = 12;
                y -= 1;
            }
            d = days_in_month(y, mo);
        }
    }
    while total >= 24 * 60 {
        total -= 24 * 60;
        d += 1;
        if d > days_in_month(y, mo) {
            d = 1;
            mo += 1;
            if mo > 12 {
                mo = 1;
                y += 1;
            }
        }
    }
    (y as u16, mo as u8, d as u8, (total / 60) as u8, (total % 60) as u8)
}

fn days_in_month(year: i32, month: i32) -> i32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap { 29 } else { 28 }
        }
    }
}
