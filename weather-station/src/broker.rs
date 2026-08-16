//! Reaching the mesh broker with the credential the profile carries.
//!
//! The pre-flight probe is the part that matters to the mesh: a slot can be
//! revoked, and without it a revoked station retries forever in silence.

use std::time::Duration;

use tracing::info;

use crate::{BrokerProfile, StationError};

/// How long the pre-flight probe waits for a CONNACK before giving up.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(15);

/// One CONNECT → CONNACK round-trip before building the database.
///
/// The connector's event loop retries connection errors forever, which suits a
/// broker that comes and goes but not a credential the mesh can revoke: a
/// revoked slot would retry silently. Probing once turns an auth rejection into
/// an error message at startup.
pub(crate) async fn preflight_broker_check(
    broker: &BrokerProfile,
    client_id: &str,
) -> Result<(), StationError> {
    let (tls, host, port) = split_broker_url(&broker.url)?;

    let mut opts = rumqttc::MqttOptions::new(format!("{client_id}-preflight"), host, port);
    opts.set_keep_alive(Duration::from_secs(10));
    opts.set_credentials(&broker.username, &broker.password);
    if tls {
        opts.set_transport(rumqttc::Transport::Tls(rumqttc::TlsConfiguration::Native));
    }

    let (client, mut event_loop) = rumqttc::AsyncClient::new(opts, 4);
    let result = tokio::time::timeout(PREFLIGHT_TIMEOUT, async {
        loop {
            match event_loop.poll().await {
                Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) => return Ok(()),
                Ok(_) => continue,
                Err(e) => return Err(e),
            }
        }
    })
    .await;
    let _ = client.disconnect().await;

    match result {
        Ok(Ok(())) => {
            info!("✅ Broker accepted the station credential");
            Ok(())
        }
        Ok(Err(rumqttc::ConnectionError::ConnectionRefused(code))) => {
            Err(StationError::CredentialRejected(format!("{code:?}")))
        }
        Ok(Err(e)) => Err(StationError::BrokerUnreachable {
            url: redact_url(&broker.url),
            reason: e.to_string(),
        }),
        Err(_) => Err(StationError::BrokerTimeout(redact_url(&broker.url))),
    }
}

/// Split a `mqtt[s]://host[:port]` broker URL into (tls, host, port).
pub(crate) fn split_broker_url(url: &str) -> Result<(bool, String, u16), StationError> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| StationError::BrokerUrl(format!("broker URL '{url}' has no scheme")))?;
    let tls = match scheme {
        "mqtt" => false,
        "mqtts" => true,
        other => {
            return Err(StationError::BrokerUrl(format!(
                "unsupported broker scheme '{other}'"
            )))
        }
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse()
                .map_err(|_| StationError::BrokerUrl(format!("invalid broker port in '{url}'")))?,
        ),
        None => (authority.to_string(), if tls { 8883 } else { 1883 }),
    };
    Ok((tls, host, port))
}

/// Embed the profile's credential into the connector URL
/// (`mqtts://user:pass@host:port`), the form the MQTT connector parses.
pub(crate) fn url_with_credentials(broker: &BrokerProfile) -> Result<String, StationError> {
    let (scheme, rest) = broker.url.split_once("://").ok_or_else(|| {
        StationError::BrokerUrl(format!("broker URL '{}' has no scheme", broker.url))
    })?;
    Ok(format!(
        "{scheme}://{}:{}@{rest}",
        broker.username, broker.password
    ))
}

/// Strip URL-embedded credentials before logging.
pub fn redact_url(url: &str) -> String {
    if let Some((scheme, rest)) = url.split_once("://") {
        if let Some((_creds, host)) = rest.rsplit_once('@') {
            return format!("{scheme}://…@{host}");
        }
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broker(url: &str) -> BrokerProfile {
        BrokerProfile {
            url: url.to_string(),
            username: "station-17".to_string(),
            password: "s3cret".to_string(),
        }
    }

    #[test]
    fn broker_url_splits_with_scheme_defaults() {
        assert_eq!(
            split_broker_url("mqtts://broker.example.com:8883").unwrap(),
            (true, "broker.example.com".to_string(), 8883)
        );
        assert_eq!(
            split_broker_url("mqtts://broker.example.com").unwrap(),
            (true, "broker.example.com".to_string(), 8883)
        );
        assert_eq!(
            split_broker_url("mqtt://localhost").unwrap(),
            (false, "localhost".to_string(), 1883)
        );
        assert!(split_broker_url("http://x").is_err());
        assert!(split_broker_url("no-scheme").is_err());
    }

    #[test]
    fn connector_url_carries_credentials() {
        assert_eq!(
            url_with_credentials(&broker("mqtts://broker.example.com:8883")).unwrap(),
            "mqtts://station-17:s3cret@broker.example.com:8883"
        );
    }

    #[test]
    fn redaction_hides_credentials() {
        assert_eq!(
            redact_url("mqtts://station-17:s3cret@broker.example.com:8883"),
            "mqtts://…@broker.example.com:8883"
        );
        assert_eq!(redact_url("mqtt://localhost"), "mqtt://localhost");
    }

    /// The revocation policy is mesh text, not station text: every station
    /// reports it the same way.
    #[test]
    fn a_rejected_credential_names_the_recovery_path() {
        let message = StationError::CredentialRejected("NotAuthorized".to_string()).to_string();
        assert!(message.contains("silent for 30 days, or by the operator"));
        assert!(message.contains("Re-join the mesh"));
    }
}
