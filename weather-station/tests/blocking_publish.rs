//! The blocking door, end to end against a real broker.
//!
//! Ignored by default because it needs one. To run it:
//!
//! ```bash
//! mosquitto -p 1883 &
//! cargo test -p weather-station --features sync -- --ignored
//! ```
//!
//! The assertion that earns its keep is that the *first* reading arrives.
//! Producing before the slot's outbound links have subscribed loses the value
//! and still returns `Ok`, because the buffer is a broadcast — measured at
//! seven losses in eight attempts without the readiness gate in
//! `StationHandle::open`. A test that published on a timer would pass either
//! way; this one publishes immediately.

#![cfg(feature = "sync")]

use std::sync::mpsc;
use std::time::Duration;

use weather_station::{AppProfile, BrokerProfile, StationHandle};

const BROKER: &str = "mqtt://localhost:1883";
const SLOT: &str = "slot-11";

/// Subscribe on a thread of its own and forward `(topic, payload)` pairs.
fn subscribe(topic: &str) -> mpsc::Receiver<(String, String)> {
    let (tx, rx) = mpsc::channel();
    let topic = topic.to_string();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let mut opts = rumqttc::MqttOptions::new("weather-station-test-sub", "localhost", 1883);
            opts.set_keep_alive(Duration::from_secs(5));
            let (client, mut event_loop) = rumqttc::AsyncClient::new(opts, 16);
            client
                .subscribe(&topic, rumqttc::QoS::AtMostOnce)
                .await
                .unwrap();

            while let Ok(event) = event_loop.poll().await {
                if let rumqttc::Event::Incoming(rumqttc::Packet::Publish(p)) = event {
                    let payload = String::from_utf8_lossy(&p.payload).to_string();
                    if tx.send((p.topic.clone(), payload)).is_err() {
                        return;
                    }
                }
            }
        });
    });

    // Let the SUBSCRIBE land before the station publishes.
    std::thread::sleep(Duration::from_millis(500));
    rx
}

#[test]
#[ignore = "needs an MQTT broker on localhost:1883"]
fn the_first_reading_reaches_the_mesh() {
    let readings = subscribe("station/11/#");

    let app = AppProfile {
        name: "blocking-door-test".to_string(),
        lat: None,
        lon: None,
    };
    let broker = BrokerProfile {
        url: BROKER.to_string(),
        username: "station-11".to_string(),
        password: "local".to_string(),
    };

    // No async runtime here: this is the shape an FFI caller has.
    let station = StationHandle::open(SLOT, &app, &broker).expect("join the mesh");

    // Immediately — no settling delay. This is the regression guard.
    station.publish_temperature(21.5).expect("publish");
    station.publish_humidity(55.0).expect("publish");

    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while seen.len() < 2 && std::time::Instant::now() < deadline {
        match readings.recv_timeout(Duration::from_secs(1)) {
            Ok(reading) => seen.push(reading),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(e) => panic!("subscriber died: {e}"),
        }
    }

    station.close().expect("close");

    let temperature = seen
        .iter()
        .find(|(t, _)| t == "station/11/temperature")
        .unwrap_or_else(|| panic!("no temperature published; saw {seen:?}"));
    assert!(
        temperature.1.contains("\"celsius\":21.5"),
        "unexpected payload: {}",
        temperature.1
    );

    let humidity = seen
        .iter()
        .find(|(t, _)| t == "station/11/humidity")
        .unwrap_or_else(|| panic!("no humidity published; saw {seen:?}"));
    assert!(
        humidity.1.contains("\"percent\":55.0"),
        "unexpected payload: {}",
        humidity.1
    );
}

/// The slot's identity and topics come from the profile, not from the caller.
#[test]
#[ignore = "needs an MQTT broker on localhost:1883"]
fn the_handle_exposes_the_slot_it_joined() {
    let app = AppProfile {
        name: "blocking-door-test".to_string(),
        lat: None,
        lon: None,
    };
    let broker = BrokerProfile {
        url: BROKER.to_string(),
        username: "station-11".to_string(),
        password: "local".to_string(),
    };

    let station = StationHandle::open(SLOT, &app, &broker).expect("join the mesh");
    let slot = station.mesh_slot();

    assert_eq!(slot.slot(), 11);
    assert_eq!(slot.temperature_topic(), "mqtt://station/11/temperature");
    assert_eq!(slot.humidity_topic(), "mqtt://station/11/humidity");

    station.close().expect("close");
}
