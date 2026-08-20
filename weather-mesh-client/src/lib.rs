//! # Weather Mesh Client
//!
//! The browser half of the mesh: `aimdb-wasm-adapter`'s runtime fused with this
//! mesh's contracts and its naming rule, compiled to one wasm module and
//! published to npm as `@aimdb/weather-mesh-client`.
//!
//! ## Why a crate exists for this
//!
//! `SchemaRegistry::register<T>()` builds a table of monomorphized closures —
//! `get_typed::<T>`, `set_typed::<T>`, `subscribe_typed::<T>` — keyed by
//! `T::NAME`. The dispatch table is built at Rust monomorphization time, not at
//! runtime, so **the contracts have to compile into the same wasm module as the
//! adapter**. There is no way to ship a generic adapter that a separate
//! contracts package plugs into from JavaScript. The publishable unit is always
//! "the browser client for *this* mesh", and this crate is that fusion point.
//!
//! `weather-contracts` cannot host it: that crate is `no_std` for MCU nodes,
//! and `wasm-bindgen` would break it exactly as `rumqttc` would.
//!
//! ## What this crate is *not*
//!
//! It is not the public API. The exports here are the raw plane — string keys
//! and `unknown` payloads — and the package presents the mesh instead:
//! `connectWeatherMesh()`, `mesh.station(17).temperature`. That facade is
//! TypeScript, in `js/`, and it is what a consumer imports. These exports stay
//! public beneath it so a power user can compose against the same rule the
//! facade applies rather than hand-writing key strings.

use wasm_bindgen::prelude::*;
use weather_contracts::keys;

#[cfg(target_arch = "wasm32")]
use aimdb_wasm_adapter::bindings::WasmDb;
#[cfg(target_arch = "wasm32")]
use weather_contracts::{DewPointV1, HumidityV1, TemperatureV2};

/// Create a `WasmDb` whose schema registry already knows this mesh's types.
///
/// The returned database is unconfigured — `knownSchemas()` is populated, but
/// no record exists until `configureRecord()` and `build()` have run. The
/// facade does that; a direct caller does it by hand.
///
/// # Registration is one type per schema name
///
/// The registry keys by `T::NAME`, and `TemperatureV1` and `TemperatureV2` both
/// declare `NAME = "temperature"` — the version lives in `T::VERSION`, which the
/// registry does not look at. Registering both would silently overwrite one
/// with the other (`register` documents itself as overwriting), and the symptom
/// would be a browser rendering nothing while every log stayed quiet.
///
/// So: **register the newest type of each name, exactly once.** What the client
/// then speaks on the wire is v2 only — see the note on migration below.
///
/// # This client is v2-only inbound
///
/// The hub normalizes on ingest: a station may publish a v1 payload, and
/// `TemperatureV2::from_bytes` runs the migration chain before the value ever
/// reaches a hub record. Everything the hub then serves over AimX is v2.
///
/// That matters because the browser's inbound path does *not* migrate — the
/// bridge deserializes with plain serde, not through `Linkable`, so a v1 payload
/// arriving here would fail to decode and log a console warning rather than
/// upgrade. The client's version tolerance is the hub's, not this crate's. If
/// anything ever bridges raw station payloads to a browser, that assumption
/// breaks and this is where it breaks.
///
/// Only exists on `wasm32`: the adapter it returns a handle to does not compile
/// for any other target, which is what the per-target dependency in
/// `Cargo.toml` records. `make wasm-check` is what proves this half builds.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(js_name = "createWeatherDb")]
pub fn create_weather_db() -> WasmDb {
    let mut db = WasmDb::new();
    db.register::<TemperatureV2>();
    db.register::<HumidityV1>();
    db.register::<DewPointV1>();
    db
}

// ─── The naming rule ──────────────────────────────────────────────────────
//
// Compiled from `weather-contracts::keys` rather than restated, so the
// TypeScript side derives the mesh's spelling instead of transcribing it a
// fourth time. What crosses is the *rule*; which slots exist is a property of a
// running hub and is discovered, never compiled in.

/// Record key for a slot's temperature reading — `station.<n>.temperature`.
#[wasm_bindgen(js_name = "temperatureKey")]
pub fn temperature_key(slot: u16) -> String {
    keys::temperature_key(slot)
}

/// Record key for a slot's humidity reading — `station.<n>.humidity`.
#[wasm_bindgen(js_name = "humidityKey")]
pub fn humidity_key(slot: u16) -> String {
    keys::humidity_key(slot)
}

/// Record key for a slot's dew point — `station.<n>.dew_point`.
///
/// Derived at the hub from the two readings above; no station publishes it.
#[wasm_bindgen(js_name = "dewPointKey")]
pub fn dew_point_key(slot: u16) -> String {
    keys::dew_point_key(slot)
}

/// The slot a record key belongs to, or `undefined` if the key is not one this
/// mesh spells.
///
/// This is the direction the browser actually needs. A client learns which
/// records a hub serves from an AimX `record.list`, and the reply carries no
/// slot field — `RecordMetadata.entity` is the key's *last* segment
/// (`temperature`), not its middle one. The slot only comes back out by
/// applying the rule in reverse.
#[wasm_bindgen(js_name = "slotFromKey")]
pub fn slot_from_key(key: &str) -> Option<u16> {
    keys::slot_from_key(key)
}

// ─── Protocol ─────────────────────────────────────────────────────────────

/// The AimX protocol major.minor this client speaks.
///
/// Exported so no consumer hardcodes it. The hub accepts exactly one major and
/// refuses the rest at the WebSocket upgrade, so this is the number a
/// `ProtocolMismatchError` reports as `clientSpeaks`.
///
/// A getter rather than a constant because `wasm_bindgen` cannot export a
/// `&'static str` as a JS constant; the facade re-exports it as one.
#[wasm_bindgen(js_name = "aimxProtocolVersion")]
pub fn aimx_protocol_version() -> String {
    aimdb_core::remote::PROTOCOL_VERSION.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exports are thin on purpose — the value is that they exist at all,
    /// so TypeScript never spells a key. This pins that they forward to the
    /// contracts crate rather than reimplementing it.
    #[test]
    fn the_exported_rule_is_the_contracts_rule() {
        assert_eq!(temperature_key(17), keys::temperature_key(17));
        assert_eq!(humidity_key(17), keys::humidity_key(17));
        assert_eq!(dew_point_key(17), keys::dew_point_key(17));
        assert_eq!(slot_from_key("station.17.humidity"), Some(17));
        assert_eq!(slot_from_key("station.17.pressure"), None);
    }

    /// The facade compares the hub's advertised version against this, so a
    /// silent change of shape here becomes a silently broken version check.
    #[test]
    fn the_protocol_version_is_a_major_minor_string() {
        let v = aimx_protocol_version();
        let mut parts = v.split('.');
        assert!(parts.next().is_some_and(|p| p.parse::<u32>().is_ok()));
        assert!(parts.next().is_some_and(|p| p.parse::<u32>().is_ok()));
        assert_eq!(parts.next(), None, "expected MAJOR.MINOR, got {v:?}");
    }
}
