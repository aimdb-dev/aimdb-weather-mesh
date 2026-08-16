//! The mesh half of a station: its slot, the handshake that claims it, and the
//! records the hub expects that slot to publish.

use aimdb_core::{buffer::BufferCfg, connector::SerializeError, AimDbBuilder, StringKey};
use aimdb_data_contracts::Linkable;
use aimdb_mqtt_connector::MqttConnector;
use aimdb_tokio_adapter::TokioRecordRegistrarExt;
use tracing::info;
use weather_contracts::{Humidity, Temperature};

use crate::broker::{preflight_broker_check, redact_url, url_with_credentials};
use crate::{slot_from_station_id, AppProfile, BrokerProfile, StationError};

/// Outbound buffer depth for the slot's mesh records: enough slack for the
/// outbound pump to ride out a reconnecting broker.
const MESH_BUFFER_CAPACITY: usize = 10;

/// A station's place in the mesh.
///
/// [`MeshSlot::join`] performs the handshake — the slot-scoped identity, the
/// pre-flight CONNECT that turns a revoked credential into a startup error —
/// and [`MeshSlot::attach`] puts the mesh half of the record graph on a builder
/// the station still owns.
///
/// This is the advanced door. [`Station`](crate::Station) is the same graph with
/// the builder owned for you; reach for `MeshSlot` only when the station ingests
/// *through* the graph — a `link_from` off another connector, a transform —
/// rather than from a `.source()`.
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
pub struct MeshSlot {
    slot: u16,
    name: String,
    client_id: String,
    mqtt_url: String,
    temperature_key: StringKey,
    humidity_key: StringKey,
    temperature_topic: String,
    humidity_topic: String,
}

impl MeshSlot {
    /// Claim the slot the profile was issued for.
    ///
    /// Reports the station's identity, then probes the broker before anything
    /// is built, so a revoked slot fails at startup rather than retrying in
    /// silence.
    pub async fn join(
        station_id: &str,
        app: &AppProfile,
        broker: &BrokerProfile,
    ) -> Result<Self, StationError> {
        let slot = slot_from_station_id(station_id)?;

        info!("🚀 Starting Weather Station \"{}\"", app.name);
        info!("📡 Broker: {} (slot {slot})", redact_url(&broker.url));

        let client_id = format!("weather-station-{slot}");
        preflight_broker_check(broker, &client_id).await?;

        Ok(Self {
            slot,
            name: app.name.clone(),
            mqtt_url: url_with_credentials(broker)?,
            client_id,
            temperature_key: StringKey::intern(format!("station.{slot}.temperature")),
            humidity_key: StringKey::intern(format!("station.{slot}.humidity")),
            temperature_topic: format!("mqtt://station/{slot}/temperature"),
            humidity_topic: format!("mqtt://station/{slot}/humidity"),
        })
    }

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
    pub fn attach(&self, builder: AimDbBuilder) -> AimDbBuilder {
        let mut builder = builder
            .with_connector(MqttConnector::new(&self.mqtt_url).with_client_id(&self.client_id));

        builder.configure::<Temperature>(self.temperature_key, |reg| {
            reg.buffer(BufferCfg::SpmcRing {
                capacity: MESH_BUFFER_CAPACITY,
            });
            reg.link_to(&self.temperature_topic)
                .with_serializer(|_ctx, t: &Temperature| {
                    t.to_bytes().map_err(|_| SerializeError::InvalidData)
                })
                .finish();
        });

        builder.configure::<Humidity>(self.humidity_key, |reg| {
            reg.buffer(BufferCfg::SpmcRing {
                capacity: MESH_BUFFER_CAPACITY,
            });
            reg.link_to(&self.humidity_topic)
                .with_serializer(|_ctx, h: &Humidity| {
                    h.to_bytes().map_err(|_| SerializeError::InvalidData)
                })
                .finish();
        });

        builder
    }

    /// The banner that tells an operator the station is publishing where the
    /// mesh expects it.
    ///
    /// [`Station::run`](crate::Station::run) prints this for you; a station on
    /// this door calls it once its own graph is registered, so its startup
    /// reporting lands first.
    pub fn log_ready(&self) {
        info!("");
        info!("🎯 Weather Station \"{}\" ready!", self.name);
        info!("📡 Publishing to MQTT topics:");
        info!("   - {}", self.temperature_topic);
        info!("   - {}", self.humidity_topic);
        info!("   (dew point is derived at the hub from these two)");
        info!("");
        info!("Press Ctrl+C to stop");
    }

    /// The slot number this station publishes into.
    pub fn slot(&self) -> u16 {
        self.slot
    }

    /// The station name from the profile's `[app]` table.
    pub fn name(&self) -> &str {
        &self.name
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
