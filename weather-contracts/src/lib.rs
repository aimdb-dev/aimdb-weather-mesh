//! # Weather Contracts
//!
//! The temperature, humidity and dew point schemas shared by the AimDB weather
//! mesh, and the [`keys`] rule every participant addresses them by.
//!
//! **Every schema type carries its version in its name, from birth.** A type is
//! never renamed and never repointed: [`TemperatureV1`] stays `TemperatureV1`
//! once [`TemperatureV2`] supersedes it, and both remain published shapes that
//! a deployed node may still speak. There is deliberately no unversioned alias
//! for "the latest" — it would be a published name whose meaning changes under
//! callers, which is the one thing a contract must never do. A node names the
//! shape it speaks, and that is readable in its source forever.
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

pub use dew_point::DewPointV1;
pub use humidity::HumidityV1;
pub use temperature::{TemperatureV1, TemperatureV2};

// Re-export traits from aimdb-data-contracts
pub use aimdb_data_contracts::{SchemaType, Settable, Streamable};
