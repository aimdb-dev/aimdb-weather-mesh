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
//!
//! [`slot_from_key`] is the same rule read backwards, and it exists for the one
//! participant that receives keys rather than deriving them: a browser client
//! learns which records a hub serves from an AimX `record.list`, and has to get
//! a slot number back out. The wire carries no slot field — `RecordMetadata`'s
//! `entity` is the key's last segment (`temperature`), not its middle one — so
//! the slot is only recoverable by applying this rule in reverse. Doing that
//! here rather than in the client is what stops the formula being spelled a
//! fourth time, in TypeScript.

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

/// The slot a record key belongs to, or `None` if the key is not one this mesh
/// spells.
///
/// The inverse of [`temperature_key`] and its siblings: `station.17.humidity`
/// is slot 17. Strict on purpose — a key that merely resembles the rule is not
/// a key this mesh produced, and answering `Some` for it would hand a caller a
/// slot that nothing publishes to.
///
/// ```
/// # use weather_contracts::keys::{humidity_key, slot_from_key};
/// assert_eq!(slot_from_key(&humidity_key(17)), Some(17));
/// assert_eq!(slot_from_key("station.17"), None);
/// ```
pub fn slot_from_key(key: &str) -> Option<u16> {
    let mut parts = key.split('.');

    if parts.next()? != "station" {
        return None;
    }
    let slot = parts.next()?;
    let quantity = parts.next()?;

    // Exactly three segments: `station.17.temperature.extra` is not ours.
    if parts.next().is_some() {
        return None;
    }
    if !matches!(quantity, "temperature" | "humidity" | "dew_point") {
        return None;
    }

    // `str::parse` accepts a leading `+`, which would make `station.+17.…` a
    // second spelling of slot 17. The mesh has one spelling per slot.
    if !slot.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    slot.parse().ok()
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

    /// Round-trips across the whole slot range the hub can be configured for,
    /// so the two directions cannot drift apart one at a time.
    #[test]
    fn every_key_the_rule_produces_reads_back_as_its_own_slot() {
        for slot in [0u16, 1, 17, 63, 64, 1000, u16::MAX] {
            assert_eq!(slot_from_key(&temperature_key(slot)), Some(slot));
            assert_eq!(slot_from_key(&humidity_key(slot)), Some(slot));
            assert_eq!(slot_from_key(&dew_point_key(slot)), Some(slot));
        }
    }

    /// A key that merely resembles the rule is not one this mesh produced.
    /// Answering `Some` for any of these would hand a browser client a slot
    /// number for a record nothing publishes to.
    #[test]
    fn keys_this_mesh_does_not_spell_are_rejected() {
        for key in [
            "station.17",                   // no quantity
            "station.17.temperature.extra", // too many segments
            "station.17.pressure",          // not a mesh quantity
            "station.x.temperature",        // slot is not a number
            "station.+17.temperature",      // a second spelling of slot 17
            "station.-1.temperature",       // ditto, and out of range
            "station.65536.temperature",    // beyond u16
            "station..temperature",         // empty slot segment
            "knx.17.temperature",           // another namespace entirely
            "",
        ] {
            assert_eq!(slot_from_key(key), None, "should have rejected {key:?}");
        }
    }

    /// Topics are deliberately not accepted: they use `/` and carry a scheme,
    /// and nothing in the mesh ever needs to read a slot back out of one.
    #[test]
    fn topics_are_not_keys() {
        assert_eq!(slot_from_key(&temperature_topic(17)), None);
    }
}
