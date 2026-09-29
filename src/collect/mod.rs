//! Collectors turn two readings of the kernel's counters into gauges.

pub mod process;
pub mod system;
pub mod units;
pub mod users;

/// `(now - before) / seconds`, or nothing when the counter went backwards.
///
/// A counter goes backwards when it is reset or when its subject was
/// replaced (a device re-created, a pid reused). Either way the difference
/// describes nothing that happened, so there is no sample for the interval.
pub fn rate(now: u64, before: u64, seconds: f64) -> f64 {
    if now < before || seconds <= 0.0 {
        return f64::NAN;
    }
    (now - before) as f64 / seconds
}

/// `part / whole` as a percentage; nothing when there is no whole.
pub fn percent(part: f64, whole: f64) -> f64 {
    if whole <= 0.0 {
        return f64::NAN;
    }
    100.0 * part / whole
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_that_went_backwards_has_no_rate() {
        assert_eq!(rate(150, 100, 10.0), 5.0);
        assert!(rate(100, 150, 10.0).is_nan());
        assert!(rate(150, 100, 0.0).is_nan());
    }

    #[test]
    fn a_percentage_of_nothing_is_nothing() {
        assert_eq!(percent(25.0, 200.0), 12.5);
        assert!(percent(1.0, 0.0).is_nan());
    }
}
