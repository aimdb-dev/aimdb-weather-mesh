//! The mesh half of a station: its slot, the handshake that claims it, and the
//! records the hub expects that slot to publish.

use alloc::format;
use alloc::string::String;

use aimdb_core::StringKey;
use weather_contracts::keys;

use crate::broker::redact_url;
use crate::{slot_from_station_id, AppProfile, BrokerProfile, StationError};

/// Outbound buffer depth for the slot's mesh records: enough slack for the
/// outbound pump to ride out a reconnecting broker.
pub const MESH_BUFFER_CAPACITY: usize = 10;

/// A station's place in the mesh.
///
/// [`MeshSlot::join`] performs the handshake — the slot-scoped identity, and on
/// a host build the pre-flight CONNECT that turns a revoked credential into a
/// startup error — and [`MeshSlot::attach`] puts the mesh half of the record
/// graph on a builder the station still owns.
///
/// The advanced door on a host and the *only* door on an MCU, since
/// [`Station`](crate::Station) exists only under `tokio-runtime`. Reach for it
/// when the station ingests *through* the graph — a `link_from`, a transform —
/// rather than from a `.source()`.
pub struct MeshSlot {
    slot: u16,
    name: String,
    client_id: String,
    broker: BrokerCredential,
    temperature_key: StringKey,
    humidity_key: StringKey,
    temperature_topic: String,
    humidity_topic: String,
}

/// What the connector needs to reach the broker as this slot.
///
/// Kept in parts rather than as one credentialled URL because the two
/// connectors disagree about where the credential goes: Tokio parses it out of
/// the URL, Embassy takes it as a `with_credentials` argument.
struct BrokerCredential {
    url: String,
    username: String,
    password: String,
}

impl MeshSlot {
    /// Claim the slot the profile was issued for.
    ///
    /// Reports the station's identity, then — with the `preflight` feature —
    /// probes the broker before anything is built, so a revoked slot fails at
    /// startup rather than retrying in silence. Without it the station goes
    /// straight to building the graph and finds out from the connector.
    pub async fn join(
        station_id: &str,
        app: &AppProfile,
        broker: &BrokerProfile,
    ) -> Result<Self, StationError> {
        let slot = slot_from_station_id(station_id)?;

        aimdb_core::log_info!("🚀 Starting Weather Station \"{}\"", app.name);
        aimdb_core::log_info!("📡 Broker: {} (slot {})", redact_url(&broker.url), slot);

        let client_id = format!("weather-station-{slot}");

        #[cfg(feature = "preflight")]
        crate::broker::preflight_broker_check(broker, &client_id).await?;

        Ok(Self {
            slot,
            name: app.name.clone(),
            client_id,
            broker: BrokerCredential {
                url: broker.url.clone(),
                username: broker.username.clone(),
                password: broker.password.clone(),
            },
            temperature_key: StringKey::intern(keys::temperature_key(slot)),
            humidity_key: StringKey::intern(keys::humidity_key(slot)),
            temperature_topic: keys::temperature_topic(slot),
            humidity_topic: keys::humidity_topic(slot),
        })
    }

    /// The banner that tells an operator the station is publishing where the
    /// mesh expects it.
    ///
    /// [`Station::run`](crate::Station::run) prints this for you; a station on
    /// this door calls it once its own graph is registered, so its startup
    /// reporting lands first.
    pub fn log_ready(&self) {
        aimdb_core::log_info!("");
        aimdb_core::log_info!("🎯 Weather Station \"{}\" ready!", self.name);
        aimdb_core::log_info!("📡 Publishing to MQTT topics:");
        aimdb_core::log_info!("   - {}", self.temperature_topic);
        aimdb_core::log_info!("   - {}", self.humidity_topic);
        aimdb_core::log_info!("   (dew point is derived at the hub from these two)");
        aimdb_core::log_info!("");
        aimdb_core::log_info!("Press Ctrl+C to stop");
    }

    /// The slot number this station publishes into.
    pub fn slot(&self) -> u16 {
        self.slot
    }

    /// The station name from the profile's `[app]` table.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The MQTT client id this station connects as, `weather-station-<n>`.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// The broker URL from the profile, without the credential.
    ///
    /// Part of what an MCU station hands its own connector; a host station gets
    /// this wired for it by [`attach`](Self::attach).
    pub fn broker_url(&self) -> &str {
        &self.broker.url
    }

    /// The slot's broker username.
    pub fn username(&self) -> &str {
        &self.broker.username
    }

    /// The slot's broker password.
    pub fn password(&self) -> &str {
        &self.broker.password
    }

    /// Record key `station.<n>.temperature`.
    pub fn temperature_key(&self) -> StringKey {
        self.temperature_key
    }

    /// Record key `station.<n>.humidity`.
    pub fn humidity_key(&self) -> StringKey {
        self.humidity_key
    }

    /// Outbound topic for [`temperature_key`](Self::temperature_key).
    pub fn temperature_topic(&self) -> &str {
        &self.temperature_topic
    }

    /// Outbound topic for [`humidity_key`](Self::humidity_key).
    pub fn humidity_topic(&self) -> &str {
        &self.humidity_topic
    }
}

/// Register the slot's two mesh records on a builder: buffers, outbound links
/// and serializers — everything except the feed.
///
/// A macro, not a method: buffer construction is adapter-specific, supplied by
/// each adapter's extension trait with the same `.buffer(cfg)` shape. Expanding
/// at the call site lets whichever trait is in scope resolve it, which keeps
/// this crate free of a runtime dependency.
///
/// Host stations do not call this: [`MeshSlot::attach`] does it for them.
///
/// ```text
/// use aimdb_embassy_adapter::EmbassyRecordRegistrarExt;   // brings `.buffer(cfg)`
///
/// let mut builder = AimDbBuilder::new().runtime(runtime).with_connector(
///     MqttConnectorBuilder::new(mesh.broker_url(), stack)
///         .with_client_id(mesh.client_id())
///         .with_credentials(mesh.username(), mesh.password()),
/// );
/// weather_station::configure_slot_records!(&mut builder, &mesh);
/// ```
#[macro_export]
macro_rules! configure_slot_records {
    ($builder:expr, $slot:expr) => {{
        use $crate::__macro_deps::aimdb_core::buffer::BufferCfg;
        use $crate::__macro_deps::aimdb_core::connector::SerializeError;
        use $crate::__macro_deps::aimdb_data_contracts::Linkable;
        use $crate::__macro_deps::weather_contracts::{HumidityV1, TemperatureV2};

        let builder = $builder;
        let slot = $slot;

        builder.configure::<TemperatureV2>(slot.temperature_key(), |reg| {
            reg.buffer(BufferCfg::SpmcRing {
                capacity: $crate::MESH_BUFFER_CAPACITY,
            });
            reg.link_to(slot.temperature_topic())
                .with_serializer(|_ctx, t: &TemperatureV2| {
                    t.to_bytes().map_err(|_| SerializeError::InvalidData)
                })
                .finish();
        });

        builder.configure::<HumidityV1>(slot.humidity_key(), |reg| {
            reg.buffer(BufferCfg::SpmcRing {
                capacity: $crate::MESH_BUFFER_CAPACITY,
            });
            reg.link_to(slot.humidity_topic())
                .with_serializer(|_ctx, h: &HumidityV1| {
                    h.to_bytes().map_err(|_| SerializeError::InvalidData)
                })
                .finish();
        });
    }};
}

#[cfg(feature = "tokio-runtime")]
impl MeshSlot {
    /// Put the mesh half of the record graph on `builder`: the MQTT connector,
    /// the slot's two records, their buffers, and the outbound links that carry
    /// them.
    ///
    /// The feed is deliberately absent. A station adds it by configuring the
    /// same keys again — [`temperature_key`](Self::temperature_key) and
    /// [`humidity_key`](Self::humidity_key) — with a `.source()`, or with a
    /// transform off a record of its own.
    ///
    /// Register any connector the station's own intake needs *before* calling
    /// this: a `link_from` whose scheme has no connector is a build error.
    ///
    /// ```no_run
    /// # use aimdb_core::AimDbBuilder;
    /// # use aimdb_tokio_adapter::TokioAdapter;
    /// # use std::sync::Arc;
    /// # use weather_station::{AppProfile, BrokerProfile, MeshSlot, StationError};
    /// # async fn example(app: &AppProfile, broker: &BrokerProfile) -> Result<(), StationError> {
    /// let mesh = MeshSlot::join("slot-17", app, broker).await?;
    ///
    /// let mut builder = mesh.attach(AimDbBuilder::new().runtime(Arc::new(TokioAdapter::new()?)));
    /// // ... configure `mesh.temperature_key()` again to add the feed ...
    ///
    /// mesh.log_ready();
    /// builder.run().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn attach(&self, builder: aimdb_core::AimDbBuilder) -> aimdb_core::AimDbBuilder {
        use aimdb_mqtt_connector::MqttConnector;
        use aimdb_tokio_adapter::TokioRecordRegistrarExt;

        let url = crate::broker::url_with_credentials_from(
            &self.broker.url,
            &self.broker.username,
            &self.broker.password,
        );

        let mut builder =
            builder.with_connector(MqttConnector::new(&url).with_client_id(&self.client_id));

        crate::configure_slot_records!(&mut builder, self);

        builder
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use super::*;

    fn slot_named(station_id: &str) -> MeshSlot {
        let slot = slot_from_station_id(station_id).unwrap();
        MeshSlot {
            slot,
            name: "test".to_string(),
            client_id: format!("weather-station-{slot}"),
            broker: BrokerCredential {
                url: "mqtt://localhost:1883".to_string(),
                username: "u".to_string(),
                password: "p".to_string(),
            },
            temperature_key: StringKey::intern(keys::temperature_key(slot)),
            humidity_key: StringKey::intern(keys::humidity_key(slot)),
            temperature_topic: keys::temperature_topic(slot),
            humidity_topic: keys::humidity_topic(slot),
        }
    }

    /// Record keys and topics are the mesh contract: the hub registers
    /// `station.<n>.*` and subscribes `station/+/…`, so a station deriving
    /// these differently joins nothing. Pinned here rather than left to the
    /// two `attach` bodies, which is what lets the MCU build share them.
    #[test]
    fn the_slot_fixes_the_keys_and_topics_the_hub_expects() {
        let mesh = slot_named("slot-3");

        assert_eq!(mesh.slot(), 3);
        assert_eq!(mesh.client_id(), "weather-station-3");
        assert_eq!(
            aimdb_core::RecordKey::as_str(&mesh.temperature_key()),
            "station.3.temperature"
        );
        assert_eq!(
            aimdb_core::RecordKey::as_str(&mesh.humidity_key()),
            "station.3.humidity"
        );
        assert_eq!(mesh.temperature_topic(), "mqtt://station/3/temperature");
        assert_eq!(mesh.humidity_topic(), "mqtt://station/3/humidity");
    }
}
