//! The wall clock a station's timestamps come from, and the startup check that
//! refuses to run without one.

use crate::StationError;

/// Refuse to start when the system clock is unusable.
///
/// A reading with no usable timestamp is worse than no reading: the hub keys
/// its dew-point join off them. A station that starts anyway publishes readings
/// the hub cannot pair, and looks healthy doing it.
///
/// For a station that stamps its own readings. One whose timestamps come from
/// the runtime still wants it: `unix_time()` reports no error, it reports zero.
pub fn check_wall_clock() -> Result<(), StationError> {
    unix_millis().map(|_| ())
}

/// Wall-clock milliseconds.
pub(crate) fn unix_millis() -> Result<u64, StationError> {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .map_err(|_| StationError::NoWallClock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_carry_a_wall_clock_timestamp() {
        // Sanity: milliseconds since the epoch, not seconds and not zero.
        assert!(unix_millis().unwrap() > 1_700_000_000_000);
        assert!(check_wall_clock().is_ok());
    }
}
