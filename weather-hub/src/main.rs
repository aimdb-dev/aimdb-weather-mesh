use aimdb_core::{buffer::BufferCfg, AimDbBuilder, RecordKey, StringKey};
use aimdb_data_contracts::{Linkable, ObservableRegistrarExt};
use aimdb_tokio_adapter::{TokioAdapter, TokioRecordRegistrarExt};
use weather_contracts::{DewPoint, Humidity, Temperature};

#[tokio::main]
async fn main() -> aimdb_core::DbResult<()> {
    // Initialize logging. `aimdb` is the runtime-context log target (ctx.log()
    // via the adapter); without it in the fallback filter, contract-violation
    // reports from the mesh deserializers are invisible.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "weather_hub=info,aimdb_core=info,aimdb=info".into()),
        )
        .init();

    tracing::info!("🚀 Starting Weather Hub");

    // Broker: MQTT_URL takes a full connector URL (e.g. the public mesh broker,
    // mqtts://hub-sub:…@xxxx.eu-central-1.emqx.cloud:8883); otherwise fall back
    // to a host-only MQTT_BROKER variable for a local mosquitto.
    let mqtt_url = std::env::var("MQTT_URL").unwrap_or_else(|_| {
        let mqtt_broker = std::env::var("MQTT_BROKER").unwrap_or_else(|_| "localhost".to_string());
        format!("mqtt://{}", mqtt_broker)
    });

    // Log the host only: MQTT_URL carries the broker credential.
    tracing::info!(
        "📡 Connecting to MQTT broker: {}",
        mqtt_url.rsplit('@').next().unwrap_or(&mqtt_url)
    );

    let runtime = std::sync::Arc::new(TokioAdapter::new()?);

    let mut builder = AimDbBuilder::new().runtime(runtime).with_connector(
        aimdb_mqtt_connector::MqttConnector::new(&mqtt_url).with_client_id("weather-hub"),
    );

    // MESH_SLOTS caps how many stations the mesh admits.
    let slots: u16 = std::env::var("MESH_SLOTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64);
    tracing::info!("🕸️  Mesh mode: {} slots", slots);

    // AimX introspection endpoint: what `aimdb --connect tcp://…:7433 record
    // list` talks to. Read-only (the default policy) and loopback-only unless
    // AIMX_BIND says otherwise; the hosted deployment fronts it as
    // aimdb.dev:7433.
    let aimx_bind = std::env::var("AIMX_BIND").unwrap_or_else(|_| "127.0.0.1:7433".to_string());
    tracing::info!("🔌 AimX endpoint: tcp://{}", aimx_bind);
    builder = builder.with_connector(aimdb_tcp_connector::TcpServer::new(&aimx_bind));

    configure_mesh(&mut builder, slots);

    tracing::info!("✅ Weather Hub configured, starting...");

    builder.run().await
}

/// Registers the mesh slot pool: temperature, humidity and derived dew point
/// for every slot, bound to `station/{slot}/…` on the broker.
///
/// Every slot's records exist from startup, so admitting a station is only a
/// matter of handing out an unused slot number; no hub restart is involved. The
/// schema check happens per payload instead: `from_bytes` rejects anything that
/// doesn't match the contract, and the rejection is logged at the hub.
///
/// Values are not logged per message: across dozens of slots that output
/// drowns out everything else. `.observe()` keeps the signal gauges available
/// on `record list` / `record get`.
fn configure_mesh(builder: &mut AimDbBuilder, slots: u16) {
    for slot in 0..slots {
        let temp_key = StringKey::intern(format!("station.{slot}.temperature"));
        let temp_topic = format!("mqtt://station/{slot}/temperature");
        builder.configure::<Temperature>(temp_key, |reg| {
            // Single Producer, Multiple Consumers (SPMC) ring buffer: one
            // station publishes into it, the dashboard and `aimdb record get`
            // read from it.
            reg.buffer(BufferCfg::SpmcRing { capacity: 100 });
            // Folds °C into the record's signal gauge (last/min/max/mean), the
            // per-slot stats behind `record list` / `record get`.
            reg.observe();
            // JSON codec for AimX record.get/subscribe — the dashboard and
            // `aimdb record get` read the mesh through this.
            reg.with_remote_access();
            let key_name = temp_key.as_str();
            reg.link_from(&temp_topic)
                .with_deserializer(move |ctx, data: &[u8]| {
                    Temperature::from_bytes(data).map_err(|e| {
                        // Report the rejection against the record key, so a
                        // station publishing a malformed payload is visible.
                        ctx.log()
                            .error(&format!("{key_name}: rejected payload: {e}"));
                        e
                    })
                })
                .finish();
        });

        let humidity_key = StringKey::intern(format!("station.{slot}.humidity"));
        let humidity_topic = format!("mqtt://station/{slot}/humidity");
        builder.configure::<Humidity>(humidity_key, |reg| {
            reg.buffer(BufferCfg::SpmcRing { capacity: 100 });
            reg.observe();
            reg.with_remote_access();
            let key_name = humidity_key.as_str();
            reg.link_from(&humidity_topic)
                .with_deserializer(move |ctx, data: &[u8]| {
                    Humidity::from_bytes(data).map_err(|e| {
                        ctx.log()
                            .error(&format!("{key_name}: rejected payload: {e}"));
                        e
                    })
                })
                .finish();
        });

        // Derived at the hub so the dashboard stays supplied even when a
        // station publishes only temperature/humidity.
        let dew_point_key = StringKey::intern(format!("station.{slot}.dew_point"));
        builder.configure::<DewPoint>(dew_point_key, |reg| {
            reg.buffer(BufferCfg::SpmcRing { capacity: 100 });
            reg.observe();
            reg.with_remote_access();
            reg.transform_join(|b| {
                b.input::<Temperature>(temp_key)
                    .input::<Humidity>(humidity_key)
                    .on_triggers(|mut rx, producer| async move {
                        let mut last_temp: Option<Temperature> = None;
                        let mut last_hum: Option<Humidity> = None;
                        while let Ok(trigger) = rx.recv().await {
                            match trigger.index() {
                                0 => last_temp = trigger.as_input::<Temperature>().cloned(),
                                1 => last_hum = trigger.as_input::<Humidity>().cloned(),
                                _ => {}
                            }
                            if let (Some(t), Some(h)) = (&last_temp, &last_hum) {
                                // Magnus approximation: T_dp ≈ T - (100 - RH) / 5
                                let dew_point = t.celsius - (100.0 - h.percent) / 5.0;
                                let timestamp = t.timestamp.max(h.timestamp);
                                producer.produce(DewPoint {
                                    celsius: dew_point,
                                    timestamp,
                                });
                            }
                        }
                    })
            });
        });
    }
}
