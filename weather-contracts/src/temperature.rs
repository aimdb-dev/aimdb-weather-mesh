//! Temperature sensor schema
//!
//! # Schema Evolution
//!
//! Temperature is a **version-aware payload**, so stations and hubs can be
//! updated independently:
//!
//! - **v1**: `{ "schema_version": 1, "temp": f32, "timestamp": u64, "unit": "C"|"F"|"K" }`
//! - **v2** (what new nodes are written against): `{ "schema_version": 2, "celsius": f32, "timestamp": u64 }`
//!
//! The `MigrationChain` impl (via `migration_chain!`) reads `schema_version`
//! from the payload and migrates an older one to the current schema before the
//! record sees it.

extern crate alloc;

use aimdb_data_contracts::{Observable, SchemaType, Settable, Streamable};
use serde::{Deserialize, Serialize};

#[cfg(feature = "linkable")]
use aimdb_data_contracts::Linkable;

#[cfg(feature = "simulatable")]
use aimdb_data_contracts::{RandomWalkParams, Simulatable};
#[cfg(feature = "simulatable")]
use rand::RngExt;

#[cfg(feature = "migratable")]
use aimdb_data_contracts::{MigrationError, MigrationStep};

/// Temperature sensor reading in Celsius (schema v2)
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TemperatureV2 {
    /// Schema version (always 2 for current format)
    #[serde(default = "default_v2_version")]
    pub schema_version: u32,
    /// Temperature in degrees Celsius
    pub celsius: f32,
    /// Unix timestamp (milliseconds) when reading was taken
    pub timestamp: u64,
}

fn default_v2_version() -> u32 {
    2
}

impl TemperatureV2 {
    /// Create a new temperature reading (current schema version)
    pub fn new(celsius: f32, timestamp: u64) -> Self {
        Self {
            schema_version: 2,
            celsius,
            timestamp,
        }
    }
}

impl SchemaType for TemperatureV2 {
    const NAME: &'static str = "temperature";
    const VERSION: u32 = 2;
}

impl Streamable for TemperatureV2 {}

// ═══════════════════════════════════════════════════════════════════
// v1 SCHEMA — still spoken on the wire
// ═══════════════════════════════════════════════════════════════════

/// Temperature reading in a caller-named unit (schema v1).
///
/// v1 format: `{ "schema_version": 1, "temp": f32, "timestamp": u64, "unit": "C"|"F"|"K" }`
///
/// Not deprecated and not test scaffolding: a node built against v1 keeps
/// publishing v1 for as long as it is deployed, and the hub upgrades on ingest
/// through [`TemperatureV1ToV2`]. Superseded by [`TemperatureV2`] as the shape
/// new nodes are written against — never replaced by it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TemperatureV1 {
    /// Schema version marker (always 1 for v1)
    #[serde(default = "default_v1_version")]
    pub schema_version: u32,
    /// Temperature value in the unit specified by `unit` field
    pub temp: f32,
    /// Unix timestamp (milliseconds)
    pub timestamp: u64,
    /// Unit: "C" (Celsius), "F" (Fahrenheit), or "K" (Kelvin)
    pub unit: alloc::string::String,
}

fn default_v1_version() -> u32 {
    1
}

impl TemperatureV1 {
    /// Create a new v1 temperature reading
    pub fn new(temp: f32, timestamp: u64, unit: &str) -> Self {
        Self {
            schema_version: 1,
            temp,
            timestamp,
            unit: alloc::string::String::from(unit),
        }
    }
}

impl SchemaType for TemperatureV1 {
    const NAME: &'static str = "temperature";
    const VERSION: u32 = 1;
}

#[cfg(feature = "linkable")]
impl Linkable for TemperatureV1 {
    fn from_bytes(data: &[u8]) -> Result<Self, alloc::string::String> {
        serde_json::from_slice(data).map_err(|e| alloc::string::ToString::to_string(&e))
    }

    fn to_bytes(&self) -> Result<alloc::vec::Vec<u8>, alloc::string::String> {
        serde_json::to_vec(self).map_err(|e| alloc::string::ToString::to_string(&e))
    }
}

// ═══════════════════════════════════════════════════════════════════
// TYPE-SAFE MIGRATION (v1 → v2)
// ═══════════════════════════════════════════════════════════════════

/// Migration step: Temperature v1 (temp + unit) → v2 (celsius only)
#[cfg(feature = "migratable")]
pub struct TemperatureV1ToV2;

#[cfg(feature = "migratable")]
impl MigrationStep for TemperatureV1ToV2 {
    type Older = TemperatureV1;
    type Newer = TemperatureV2;
    const FROM_VERSION: u32 = 1;
    const TO_VERSION: u32 = 2;

    fn up(v1: TemperatureV1) -> Result<TemperatureV2, MigrationError> {
        let celsius = match v1.unit.as_str() {
            "F" => (v1.temp - 32.0) * 5.0 / 9.0,
            "K" => v1.temp - 273.15,
            _ => v1.temp, // "C" or unknown defaults to Celsius
        };
        Ok(TemperatureV2 {
            schema_version: 2,
            celsius,
            timestamp: v1.timestamp,
        })
    }

    fn down(v2: TemperatureV2) -> Result<TemperatureV1, MigrationError> {
        Ok(TemperatureV1 {
            schema_version: 1,
            temp: v2.celsius,
            timestamp: v2.timestamp,
            unit: alloc::string::String::from("C"),
        })
    }
}

#[cfg(feature = "migratable")]
aimdb_data_contracts::migration_chain! {
    type Current = TemperatureV2;
    version_field = "schema_version";
    steps {
        TemperatureV1ToV2: TemperatureV1 => TemperatureV2,
    }
}

impl Observable for TemperatureV2 {
    type Signal = f32;
    const UNIT: &'static str = "°C";

    fn signal(&self) -> f32 {
        self.celsius
    }
}

#[cfg(feature = "simulatable")]
impl Simulatable for TemperatureV2 {
    type Params = RandomWalkParams;

    /// Simulate temperature readings with random walk behavior.
    ///
    /// # Params interpretation
    /// - `base`: Center temperature value (default: 22.0°C)
    /// - `variation`: Maximum deviation from base (default: 3.0°C)
    /// - `step`: Random walk step multiplier (default: 0.2)
    /// - `trend`: Linear trend per sample (default: 0.0)
    fn simulate<R: rand::Rng>(
        params: &Self::Params,
        previous: Option<&Self>,
        rng: &mut R,
        timestamp_ms: u64,
    ) -> Self {
        let base = params.base as f32;
        let variation = params.variation as f32;
        let step = params.step as f32;
        let trend = params.trend as f32;

        // Random walk: small delta from previous value, clamped to range
        let current = match previous {
            Some(prev) => {
                let delta = (rng.random::<f32>() - 0.5) * variation * step;
                (prev.celsius + delta + trend).clamp(base - variation, base + variation)
            }
            None => base + (rng.random::<f32>() - 0.5) * variation,
        };

        TemperatureV2 {
            schema_version: 2,
            celsius: current,
            timestamp: timestamp_ms,
        }
    }
}

impl Settable for TemperatureV2 {
    type Value = f32;

    fn set(value: Self::Value, timestamp: u64) -> Self {
        TemperatureV2 {
            schema_version: 2,
            celsius: value,
            timestamp,
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// LINKABLE WITH MIGRATION SUPPORT
// ═══════════════════════════════════════════════════════════════════

#[cfg(all(feature = "linkable", feature = "migratable"))]
impl Linkable for TemperatureV2 {
    fn from_bytes(data: &[u8]) -> Result<Self, alloc::string::String> {
        use aimdb_data_contracts::MigrationChain;
        Self::migrate_from_bytes(data).map_err(|e| alloc::format!("Migration error: {}", e))
    }

    fn to_bytes(&self) -> Result<alloc::vec::Vec<u8>, alloc::string::String> {
        serde_json::to_vec(self).map_err(|e| alloc::string::ToString::to_string(&e))
    }
}

#[cfg(test)]
#[cfg(all(feature = "linkable", feature = "migratable"))]
mod tests {
    use super::*;
    use aimdb_data_contracts::Linkable;

    /// Both live schema versions decode through the same entry point, so a hub
    /// configured for [`TemperatureV2`] admits a v1 node without knowing it is
    /// one. This is the mesh's compatibility guarantee; it was previously
    /// untested, which meant a change to the `Linkable` impl could have removed
    /// it silently.
    #[test]
    fn v1_and_v2_payloads_both_decode_as_v2() {
        let v2 = br#"{"schema_version":2,"celsius":21.5,"timestamp":100}"#;
        assert_eq!(TemperatureV2::from_bytes(v2).unwrap().celsius, 21.5);

        let v1 = br#"{"schema_version":1,"temp":21.5,"timestamp":100,"unit":"C"}"#;
        assert_eq!(TemperatureV2::from_bytes(v1).unwrap().celsius, 21.5);
    }

    /// v1 carried the unit alongside the value; v2 is Celsius by definition, so
    /// the step converts rather than reinterpreting.
    #[test]
    fn the_v1_unit_is_converted_not_dropped() {
        let fahrenheit = br#"{"schema_version":1,"temp":70.7,"timestamp":100,"unit":"F"}"#;
        let t = TemperatureV2::from_bytes(fahrenheit).unwrap();
        assert!((t.celsius - 21.5).abs() < 0.1, "got {}", t.celsius);
        assert_eq!(t.schema_version, 2);

        let kelvin = br#"{"schema_version":1,"temp":294.65,"timestamp":100,"unit":"K"}"#;
        assert!((TemperatureV2::from_bytes(kelvin).unwrap().celsius - 21.5).abs() < 0.1);
    }

    /// The chain dispatches on the version field, so a payload without one is
    /// rejected — note this holds despite `schema_version` carrying a serde
    /// default, which applies only once a version has already been matched.
    #[test]
    fn a_payload_without_a_version_field_is_rejected() {
        assert!(TemperatureV2::from_bytes(br#"{"celsius":21.5,"timestamp":100}"#).is_err());
    }
}
