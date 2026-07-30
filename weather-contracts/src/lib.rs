//! # Weather Contracts
//!
//! The `Temperature`, `Humidity` and `DewPoint` schemas shared by the AimDB
//! weather mesh. Records are addressed by `StringKey` at the hub and stations,
//! so this crate carries no key enums — only the schemas themselves.
//!
//! This crate is `no_std` compatible for use on MCU nodes.

#![cfg_attr(not(feature = "std"), no_std)]

// Concrete data contracts — example implementations of the `aimdb-data-contracts`
// traits (`SchemaType`, `Streamable`, `Observable`, `Settable`, `Linkable`,
// `Simulatable`, `Migratable`) for weather monitoring.
pub mod dew_point;
pub mod humidity;
pub mod temperature;

pub use dew_point::DewPoint;
pub use humidity::Humidity;
pub use temperature::{Temperature, TemperatureV1, TemperatureV2};

// Re-export traits from aimdb-data-contracts
pub use aimdb_data_contracts::{SchemaType, Settable, Streamable};
