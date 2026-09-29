//! Wall-clock times, as people write them and as the store keeps them.

use anyhow::{bail, Result};

/// A moment, in epoch seconds, from what was typed:
///
/// - `now`
/// - a distance back from now: `-90s`, `-15m`, `-2h`, `-1d`
/// - a local time today: `14:30`, `14:30:05`
/// - a local date and time: `2026-09-29 14:30`, `2026-09-29T14:30:05`
/// - a local date, meaning its first moment: `2026-09-29`
/// - epoch seconds: `1790000000`
pub fn parse(text: &str, now: f64) -> Result<f64> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("now") {
        return Ok(now);
    }
    if let Some(back) = text.strip_prefix('-') {
        return Ok(now - parse_span(back)?);
    }
    if !text.is_empty() && text.chars().all(|c| c.is_ascii_digit()) {
        return Ok(text.parse()?);
    }

    let (date, time) = match text.split_once([' ', 'T']) {
        Some((date, time)) => (Some(date), Some(time)),
        None if text.contains(':') => (None, Some(text)),
        None => (Some(text), None),
    };

    let mut tm = local(now);
    if let Some(date) = date {
        let parts: Vec<&str> = date.split('-').collect();
        let [year, month, day] = parts[..] else {
            bail!("{text:?} is not a time: expected a date as YYYY-MM-DD");
        };
        tm.tm_year = year.parse::<i32>()? - 1900;
        tm.tm_mon = month.parse::<i32>()? - 1;
        tm.tm_mday = day.parse()?;
    }
    let (mut hour, mut minute, mut second) = (0, 0, 0);
    if let Some(time) = time {
        let parts: Vec<&str> = time.split(':').collect();
        match parts[..] {
            [h, m] => (hour, minute) = (h.parse()?, m.parse()?),
            [h, m, s] => (hour, minute, second) = (h.parse()?, m.parse()?, s.parse()?),
            _ => bail!("{text:?} is not a time: expected HH:MM or HH:MM:SS"),
        }
    }
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..61).contains(&second) {
        bail!("{text:?} is not a time of day");
    }
    tm.tm_hour = hour;
    tm.tm_min = minute;
    tm.tm_sec = second;
    // Let the C library work out whether daylight saving applies then.
    tm.tm_isdst = -1;

    // SAFETY: mktime reads and normalizes the struct it is given.
    let epoch = unsafe { libc::mktime(&mut tm) };
    if epoch == -1 {
        bail!("{text:?} is not a time this system can represent");
    }
    Ok(epoch as f64)
}

/// A length of time: `90s`, `15m`, `2h`, `1d`, or bare seconds.
pub fn parse_span(text: &str) -> Result<f64> {
    let text = text.trim();
    let (number, unit) = match text.char_indices().last() {
        Some((index, unit)) if unit.is_ascii_alphabetic() => (&text[..index], unit),
        _ => (text, 's'),
    };
    let number: f64 = match number.parse() {
        Ok(number) => number,
        Err(_) => bail!("{text:?} is not a length of time: expected a number and s, m, h, or d"),
    };
    let seconds = match unit {
        's' => 1.0,
        'm' => 60.0,
        'h' => 3600.0,
        'd' => 86_400.0,
        _ => bail!("{text:?} is not a length of time: the unit is s, m, h, or d"),
    };
    if number < 0.0 || !number.is_finite() {
        bail!("{text:?} is not a length of time");
    }
    Ok(number * seconds)
}

fn local(epoch: f64) -> libc::tm {
    let seconds = epoch as libc::time_t;
    // SAFETY: tm is plain data that localtime_r fills in.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&seconds, &mut tm) };
    tm
}

/// `2026-09-29 14:30:05`, local time.
pub fn format(epoch: f64) -> String {
    let tm = local(epoch);
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

    const NOW: f64 = 1_790_000_000.0;

    #[test]
    fn distances_back_from_now() {
        assert_eq!(parse("now", NOW).unwrap(), NOW);
        assert_eq!(parse("-90s", NOW).unwrap(), NOW - 90.0);
        assert_eq!(parse("-15m", NOW).unwrap(), NOW - 900.0);
        assert_eq!(parse("-2h", NOW).unwrap(), NOW - 7200.0);
        assert_eq!(parse("-1d", NOW).unwrap(), NOW - 86_400.0);
        assert_eq!(parse("-45", NOW).unwrap(), NOW - 45.0);
    }

    #[test]
    fn epoch_seconds_are_taken_as_they_are() {
        assert_eq!(parse("1753000000", NOW).unwrap(), 1_753_000_000.0);
    }

    #[test]
    fn a_written_time_formats_back_to_itself() {
        // Whatever the zone, writing a local time and reading it back as a
        // local time is the identity.
        for text in [
            "2026-09-29 14:30:05",
            "2026-01-15 00:00:00",
            "2026-06-30 23:59:59",
        ] {
            assert_eq!(format(parse(text, NOW).unwrap()), text);
        }
        assert_eq!(
            format(parse("2026-09-29T14:30", NOW).unwrap()),
            "2026-09-29 14:30:00"
        );
        assert_eq!(
            format(parse("2026-09-29", NOW).unwrap()),
            "2026-09-29 00:00:00"
        );
    }

    #[test]
    fn a_time_of_day_is_today() {
        let today = &format(NOW)[..10];
        assert_eq!(
            format(parse("14:30", NOW).unwrap()),
            format!("{today} 14:30:00")
        );
    }

    #[test]
    fn what_is_not_a_time_is_refused() {
        for text in [
            "",
            "yesterday",
            "-5x",
            "25:00",
            "2026-13",
            "12:00:00:00",
            "-",
        ] {
            assert!(parse(text, NOW).is_err(), "{text:?} was accepted");
        }
    }

    #[test]
    fn spans() {
        assert_eq!(parse_span("30").unwrap(), 30.0);
        assert_eq!(parse_span("1.5h").unwrap(), 5400.0);
        assert!(parse_span("h").is_err());
        assert!(parse_span("").is_err());
    }
}
