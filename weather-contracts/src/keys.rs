//! The mesh's record-key and topic naming rule.
//!
//! Every participant derives `station.<n>.…` and `mqtt://station/<n>/…` from a
//! slot number — a station from the slot its join profile assigns it, the hub
//! from each slot in its pool, a browser client from a slot it discovered. They
//! have to agree exactly: a participant spelling these differently publishes
//! into a topic nobody subscribes to, and nothing fails loudly.
//!
//! What lives here is the *rule*, not the set. Which slots exist is a property
//! of a running hub, and of whoever has joined it — discovered at runtime,
//! never compiled in.
//!
//! Keys are returned as `String` rather than interned: `StringKey` lives in
//! `aimdb-core`, and this crate stays serde-only so it can ship as the small
//! contract wheel. Callers intern.

extern crate alloc;

use alloc::{format, string::String};

/// Record key for a slot's temperature reading.
pub fn temperature_key(slot: u16) -> String {
    format!("station.{slot}.temperature")
}

/// Record key for a slot's humidity reading.
pub fn humidity_key(slot: u16) -> String {
    format!("station.{slot}.humidity")
}

/// Record key for a slot's dew point.
///
/// No topic counterpart: dew point is derived at the hub from the two readings
/// above and served over AimX, so nothing publishes it to the broker.
pub fn dew_point_key(slot: u16) -> String {
    format!("station.{slot}.dew_point")
}

/// Outbound connector URL a station publishes temperature on, and the hub
/// links from.
pub fn temperature_topic(slot: u16) -> String {
    format!("mqtt://station/{slot}/temperature")
}

/// Outbound connector URL a station publishes humidity on, and the hub links
/// from.
pub fn humidity_topic(slot: u16) -> String {
    format!("mqtt://station/{slot}/humidity")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact strings are the contract. Pinned literally, because deriving
    /// the expectation the same way the code does would assert nothing.
    #[test]
    fn the_naming_rule_is_what_the_hub_and_stations_agreed_on() {
        assert_eq!(temperature_key(3), "station.3.temperature");
        assert_eq!(humidity_key(3), "station.3.humidity");
        assert_eq!(dew_point_key(3), "station.3.dew_point");
        assert_eq!(temperature_topic(3), "mqtt://station/3/temperature");
        assert_eq!(humidity_topic(3), "mqtt://station/3/humidity");
    }
}
