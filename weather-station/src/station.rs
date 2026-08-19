//! The default door: a station that only has to supply its readings.

use std::future::Future;
use std::sync::Arc;

use aimdb_core::{AimDbBuilder, Producer, RuntimeContext};
use aimdb_tokio_adapter::TokioAdapter;
use weather_contracts::{HumidityV1, TemperatureV2};

use crate::{AppProfile, BrokerProfile, MeshSlot, StationError};

/// A joined mesh station with its record graph already built.
///
/// The handshake, the slot's records, the outbound links and the serializers
/// are sealed in here; what is left is the feed. Supply one async task per
/// quantity and call [`run`](Self::run):
///
/// ```no_run
/// # use aimdb_core::{Producer, RuntimeContext};
/// # use weather_contracts::{HumidityV1, TemperatureV2};
/// # use weather_station::{AppProfile, BrokerProfile, Station, StationError};
/// # async fn temperature_source(ctx: RuntimeContext, producer: Producer<TemperatureV2>) {}
/// # async fn humidity_source(ctx: RuntimeContext, producer: Producer<HumidityV1>) {}
/// # async fn example(app: &AppProfile, broker: &BrokerProfile) -> Result<(), StationError> {
/// Station::join("slot-17", app, broker)
///     .await?
///     .temperature(temperature_source)
///     .humidity(humidity_source)
///     .run()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// A station whose readings arrive *through* a connector rather than from a
/// poll loop wants [`MeshSlot`] instead — it hands back the builder unbuilt.
pub struct Station {
    slot: MeshSlot,
    builder: AimDbBuilder,
}

impl Station {
    /// Join the mesh on the Tokio runtime and register the slot's records.
    ///
    /// See [`MeshSlot::join`] for what the handshake does.
    pub async fn join(
        station_id: &str,
        app: &AppProfile,
        broker: &BrokerProfile,
    ) -> Result<Self, StationError> {
        let slot = MeshSlot::join(station_id, app, broker).await?;
        let builder = slot.attach(AimDbBuilder::new().runtime(Arc::new(TokioAdapter::new()?)));
        Ok(Self { slot, builder })
    }

    /// Feed `station.<n>.temperature`.
    ///
    /// The task owns its own cadence and runs as part of the record graph; every
    /// value it produces is serialized onto the slot's MQTT topic.
    pub fn temperature<F, Fut>(mut self, f: F) -> Self
    where
        F: FnOnce(RuntimeContext, Producer<TemperatureV2>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let key = self.slot.temperature_key();
        self.builder.configure::<TemperatureV2>(key, |reg| {
            reg.source(f);
        });
        self
    }

    /// Feed `station.<n>.humidity`. The mesh slot carries both quantities: the
    /// hub derives dew point from them, so a station that publishes only one
    /// leaves its slot without a dew point.
    pub fn humidity<F, Fut>(mut self, f: F) -> Self
    where
        F: FnOnce(RuntimeContext, Producer<HumidityV1>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let key = self.slot.humidity_key();
        self.builder.configure::<HumidityV1>(key, |reg| {
            reg.source(f);
        });
        self
    }

    /// This station's place in the mesh, for startup reporting of its own.
    pub fn mesh_slot(&self) -> &MeshSlot {
        &self.slot
    }

    /// Drive the graph until it stops, which is normally never.
    pub async fn run(self) -> Result<(), StationError> {
        self.slot.log_ready();
        self.builder.run().await?;
        Ok(())
    }
}
