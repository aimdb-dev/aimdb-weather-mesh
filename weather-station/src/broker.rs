//! Reaching the mesh broker with the credential the profile carries.
//!
//! Splitting the URL and hiding the credential is pure string work and shared
//! by every station. The pre-flight probe is not: it is a second MQTT client
//! bought for one CONNECT, which is worth it on a host and not on an MCU, so it
//! sits behind the `preflight` feature.

use alloc::format;
use alloc::string::{String, ToString};

#[cfg(feature = "preflight")]
use crate::{BrokerProfile, StationError};

/// How long the pre-flight probe waits for a CONNACK before giving up.
#[cfg(feature = "preflight")]
const PREFLIGHT_TIMEOUT: core::time::Duration = core::time::Duration::from_secs(15);

/// One CONNECT → CONNACK round-trip before building the database.
///
/// The connector's event loop retries connection errors forever, which suits a
/// broker that comes and goes but not a credential the mesh can revoke: a
/// revoked slot would retry silently. Probing once turns an auth rejection into
/// an error message at startup.
///
/// A station built without the `preflight` feature skips this and learns the
/// same thing from the connector's reconnect loop instead — later, and only in
/// the log, which is the trade an MCU makes.
#[cfg(feature = "preflight")]
pub(crate) async fn preflight_broker_check(
    broker: &BrokerProfile,
    client_id: &str,
) -> Result<(), StationError> {
    let (tls, host, port) = split_broker_url(&broker.url)?;

    let mut opts = rumqttc::MqttOptions::new(format!("{client_id}-preflight"), host, port);
    opts.set_keep_alive(core::time::Duration::from_secs(10));
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
            aimdb_core::log_info!("✅ Broker accepted the station credential");
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
///
/// Only the pre-flight probe needs the URL taken apart: both connectors parse
/// the URL themselves.
#[cfg(feature = "preflight")]
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

/// Embed the credential into the connector URL
/// (`mqtts://user:pass@host:port`), the form the Tokio MQTT connector parses.
///
/// Tokio-only: the Embassy connector takes the credential as its own
/// `with_credentials` argument, so it never has to travel inside a URL.
///
/// A URL with no scheme is returned unchanged rather than rejected — the slot
/// was already claimed by the time this runs, and the connector reports a bad
/// URL better than a second parse of it would.
#[cfg(feature = "tokio-runtime")]
pub(crate) fn url_with_credentials_from(url: &str, username: &str, password: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{username}:{password}@{rest}"),
        None => url.to_string(),
    }
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

    #[cfg(feature = "preflight")]
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

    #[cfg(feature = "tokio-runtime")]
    #[test]
    fn connector_url_carries_credentials() {
        assert_eq!(
            url_with_credentials_from("mqtts://broker.example.com:8883", "station-17", "s3cret"),
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
        let message =
            crate::StationError::CredentialRejected("NotAuthorized".to_string()).to_string();
        assert!(message.contains("silent for 30 days, or by the operator"));
        assert!(message.contains("Re-join the mesh"));
    }
}
