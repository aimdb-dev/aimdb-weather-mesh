//! # Weather Contracts
//!
//! Concrete data contracts for the AimDB weather mesh: the `Temperature`,
//! `Humidity`, `DewPoint`, and `GpsLocation` schemas plus their record keys.
//!
//! This crate is `no_std` compatible for use on MCU nodes.

#![cfg_attr(not(feature = "std"), no_std)]

// Concrete data contracts — example implementations of the `aimdb-data-contracts`
// traits (`SchemaType`, `Streamable`, `Observable`, `Settable`, `Linkable`,
// `Simulatable`, `Migratable`) for weather monitoring.
pub mod dew_point;
pub mod humidity;
pub mod location;
pub mod temperature;

pub use dew_point::DewPoint;
pub use humidity::Humidity;
pub use location::GpsLocation;
pub use temperature::{Temperature, TemperatureV1, TemperatureV2};

// Re-export traits from aimdb-data-contracts
pub use aimdb_data_contracts::{SchemaType, Settable, Streamable};

// Re-export RecordKey for convenience
pub use aimdb_core::RecordKey;

/// Temperature record keys for each weather station node.
///
/// Each variant represents a temperature sensor with its MQTT topic.
#[derive(RecordKey, Clone, Copy, PartialEq, Eq, Debug)]
#[key_prefix = "temp."]
pub enum TempKey {
    #[key = "alpha"]
    #[link_address = "mqtt://sensors/alpha/temperature"]
    Alpha,

    #[key = "beta"]
    #[link_address = "mqtt://sensors/beta/temperature"]
    Beta,

    #[key = "gamma"]
    #[link_address = "mqtt://sensors/gamma/temperature"]
    Gamma,
}

/// Humidity record keys for each weather station node.
///
/// Each variant represents a humidity sensor with its MQTT topic.
#[derive(RecordKey, Clone, Copy, PartialEq, Eq, Debug)]
#[key_prefix = "humidity."]
pub enum HumidityKey {
    #[key = "alpha"]
    #[link_address = "mqtt://sensors/alpha/humidity"]
    Alpha,

    #[key = "beta"]
    #[link_address = "mqtt://sensors/beta/humidity"]
    Beta,

    #[key = "gamma"]
    #[link_address = "mqtt://sensors/gamma/humidity"]
    Gamma,
}

/// Dew point record keys for each weather station node.
///
/// Dew point is derived from the corresponding [`TempKey`] and [`HumidityKey`]
/// via `transform_join` — not sensed directly.
#[derive(RecordKey, Clone, Copy, PartialEq, Eq, Debug)]
#[key_prefix = "dew_point."]
pub enum DewPointKey {
    #[key = "alpha"]
    #[link_address = "mqtt://sensors/alpha/dew_point"]
    Alpha,

    #[key = "beta"]
    #[link_address = "mqtt://sensors/beta/dew_point"]
    Beta,

    #[key = "gamma"]
    #[link_address = "mqtt://sensors/gamma/dew_point"]
    Gamma,
}
