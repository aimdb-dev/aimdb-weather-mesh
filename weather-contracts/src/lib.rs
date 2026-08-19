//! # Weather Contracts
//!
//! The `Temperature`, `Humidity` and `DewPoint` schemas shared by the AimDB
//! weather mesh, and the [`keys`] rule every participant addresses them by.
//! Records are addressed by `StringKey` at the hub and at the stations, so what
//! this crate carries is how a key is *spelled* — never an enumeration of which
//! keys exist, which is a property of a running deployment.
//!
//! This crate is `no_std` compatible for use on MCU nodes.

#![cfg_attr(not(feature = "std"), no_std)]

// Concrete data contracts: implementations of the `aimdb-data-contracts` traits
// (`SchemaType`, `Streamable`, `Observable`, `Settable`, `Linkable`,
// `Simulatable`, `Migratable`) for weather monitoring.
pub mod dew_point;
pub mod humidity;
pub mod temperature;

// The record-key and topic naming rule, shared by stations, the hub and the
// browser client so none of them can spell it differently.
pub mod keys;

pub use dew_point::DewPoint;
pub use humidity::Humidity;
pub use temperature::{Temperature, TemperatureV1, TemperatureV2};

// Re-export traits from aimdb-data-contracts
pub use aimdb_data_contracts::{SchemaType, Settable, Streamable};
