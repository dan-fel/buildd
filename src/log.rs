//! The daemon's log: a line for each thing worth a person's attention, each
//! starting with the local time, so a log read hours later says when.

use std::fmt;
use std::time::SystemTime;

/// Writes a line to the daemon's log, its standard error.
macro_rules! log {
    ($($argument:tt)*) => {
        $crate::log::write(format_args!($($argument)*))
    };
}
pub(crate) use log;

pub(crate) fn write(message: fmt::Arguments<'_>) {
    eprintln!("{} buildd: {message}", local_time(SystemTime::now()));
}

/// `at` in the local time zone, as `2026-10-06 12:34:56`.
pub fn local_time(at: SystemTime) -> String {
    let seconds = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("the clock is past 1970")
        .as_secs();
    let time = libc::time_t::try_from(seconds).expect("seconds since 1970 fit time_t");
    // SAFETY: `tm` holds integers and a pointer, for which zeroes are valid.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call.
    let converted = unsafe { libc::localtime_r(&raw const time, &raw mut tm) };
    assert!(
        !converted.is_null(),
        "a time after 1970 has a local calendar date"
    );
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_times_are_calendar_dates_and_clock_times() {
        let text = local_time(SystemTime::now());
        let bytes = text.as_bytes();
        assert_eq!(bytes.len(), 19, "{text}");
        for (index, byte) in bytes.iter().enumerate() {
            match index {
                4 | 7 => assert_eq!(*byte, b'-', "{text}"),
                10 => assert_eq!(*byte, b' ', "{text}"),
                13 | 16 => assert_eq!(*byte, b':', "{text}"),
                _ => assert!(byte.is_ascii_digit(), "{text}"),
            }
        }
    }
}
