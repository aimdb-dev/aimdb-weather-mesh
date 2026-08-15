//! The profile's `[knx]` table, and the validation that turns it into a
//! resolved configuration.
//!
//! Everything that can be rejected is rejected here, before the database is
//! built: a group address the bus can never carry, a datapoint type that
//! belongs to another quantity, or a profile that names only one of the two
//! quantities the mesh slot needs. The resolved form
//! ([`KnxConfig::resolve`]) carries decoders rather than strings, so the
//! record graph in `main.rs` cannot be handed an unvalidated value.

use crate::dpt::{HumidityDpt, TemperatureDpt};
use serde::Deserialize;

/// Throttle window when the profile does not name one (D7). Fast enough that a
/// slot never looks stale next to the Open-Meteo station's 5-minute cadence,
/// slow enough that an on-change sensor cannot flood the broker.
pub const DEFAULT_MIN_PUBLISH_SECS: u64 = 60;

const DEFAULT_TEMPERATURE_DPT: &str = "9.001";
const DEFAULT_HUMIDITY_DPT: &str = "9.007";

/// The profile's `[knx]` table, as written.
///
/// Both quantity tables are `Option` so that a profile omitting one produces
/// the error this station wants to print rather than serde's, which names a
/// missing field without saying why the station needs it (D6).
#[derive(Debug, Deserialize)]
pub struct KnxProfile {
    /// KNXnet/IP gateway, `knx://host[:port]`. Port defaults to 3671.
    pub gateway: String,
    /// Throttle window in seconds; `0` publishes every telegram.
    pub min_publish_secs: Option<u64>,
    pub temperature: Option<QuantityProfile>,
    pub humidity: Option<QuantityProfile>,
}

/// One quantity's group address and datapoint type.
#[derive(Debug, Deserialize)]
pub struct QuantityProfile {
    pub group_address: String,
    pub dpt: Option<String>,
}

/// A validated `[knx]` table.
#[derive(Debug, Clone)]
pub struct KnxConfig {
    pub gateway: String,
    pub min_publish_ms: u64,
    pub temperature: Quantity<TemperatureDpt>,
    pub humidity: Quantity<HumidityDpt>,
}

/// A validated group address with the decoder to apply to its telegrams.
#[derive(Debug, Clone)]
pub struct Quantity<D> {
    pub group_address: String,
    pub dpt: D,
}

impl KnxProfile {
    /// Validate the table into the form the record graph is built from.
    ///
    /// Errors are returned one at a time, in the order an operator would fix
    /// them: the missing table first, then the addresses, then the datapoint
    /// types.
    pub fn resolve(&self) -> Result<KnxConfig, String> {
        let temperature = self.temperature.as_ref().ok_or_else(|| {
            "[knx.temperature] is missing — a mesh station publishes both temperature \
             and humidity so the hub can derive dew point for its slot"
                .to_string()
        })?;
        let humidity = self.humidity.as_ref().ok_or_else(|| {
            "[knx.humidity] is missing — a mesh station publishes both temperature and \
             humidity so the hub can derive dew point for its slot"
                .to_string()
        })?;

        let temperature_ga = parse_group_address(&temperature.group_address)
            .map_err(|e| format!("[knx.temperature] group_address: {e}"))?;
        let humidity_ga = parse_group_address(&humidity.group_address)
            .map_err(|e| format!("[knx.humidity] group_address: {e}"))?;

        // One address feeding both records would make the two mesh records
        // mirror each other — a temperature reading published as humidity.
        if temperature_ga == humidity_ga {
            return Err(format!(
                "[knx.temperature] and [knx.humidity] both read group address {temperature_ga} — \
                 give each quantity the address of its own sensor"
            ));
        }

        let temperature_dpt = TemperatureDpt::parse(
            temperature
                .dpt
                .as_deref()
                .unwrap_or(DEFAULT_TEMPERATURE_DPT),
        )?;
        let humidity_dpt =
            HumidityDpt::parse(humidity.dpt.as_deref().unwrap_or(DEFAULT_HUMIDITY_DPT))?;

        let min_publish_secs = self.min_publish_secs.unwrap_or(DEFAULT_MIN_PUBLISH_SECS);

        Ok(KnxConfig {
            gateway: self.gateway.clone(),
            min_publish_ms: min_publish_secs.saturating_mul(1_000),
            temperature: Quantity {
                group_address: temperature_ga,
                dpt: temperature_dpt,
            },
            humidity: Quantity {
                group_address: humidity_ga,
                dpt: humidity_dpt,
            },
        })
    }
}

/// Validate a 3-level group address and return it in canonical form.
///
/// The connector matches inbound telegrams by the string a `GroupAddress`
/// renders to, so `"09/1/0"` would subscribe to an address no telegram ever
/// carries. Normalising here makes the profile forgiving without making the
/// route table wrong.
pub fn parse_group_address(raw: &str) -> Result<String, String> {
    let mut parts = raw.split('/');
    let (Some(main), Some(middle), Some(sub), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(format!(
            "'{raw}' is not a 3-level group address — expected main/middle/sub, e.g. 9/1/0"
        ));
    };

    let main = level(main, raw, "main", 31)?;
    let middle = level(middle, raw, "middle", 7)?;
    let sub = level(sub, raw, "sub", 255)?;

    Ok(format!("{main}/{middle}/{sub}"))
}

/// One level of a group address, bounded by what the 16-bit KNX encoding holds.
fn level(value: &str, raw: &str, name: &str, max: u16) -> Result<u16, String> {
    let parsed: u16 = value
        .parse()
        .map_err(|_| format!("'{raw}': {name} group '{value}' is not a number"))?;
    if parsed > max {
        return Err(format!(
            "'{raw}': {name} group {parsed} is out of range (0–{max})"
        ));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(toml: &str) -> KnxProfile {
        toml::from_str(toml).unwrap()
    }

    const FULL: &str = r#"
        gateway = "knx://192.168.1.4:3671"
        min_publish_secs = 30

        [temperature]
        group_address = "9/1/0"
        dpt = "9.001"

        [humidity]
        group_address = "9/1/1"
        dpt = "5.001"
    "#;

    #[test]
    fn resolves_a_complete_table() {
        let cfg = profile(FULL).resolve().unwrap();
        assert_eq!(cfg.gateway, "knx://192.168.1.4:3671");
        assert_eq!(cfg.min_publish_ms, 30_000);
        assert_eq!(cfg.temperature.group_address, "9/1/0");
        assert_eq!(cfg.humidity.group_address, "9/1/1");
        assert_eq!(cfg.humidity.dpt, HumidityDpt::Percent);
    }

    #[test]
    fn optional_fields_take_their_defaults() {
        let cfg = profile(
            r#"
            gateway = "knx://192.168.1.4"

            [temperature]
            group_address = "9/1/0"

            [humidity]
            group_address = "9/1/1"
            "#,
        )
        .resolve()
        .unwrap();
        assert_eq!(cfg.min_publish_ms, DEFAULT_MIN_PUBLISH_SECS * 1_000);
        assert_eq!(cfg.temperature.dpt.identifier(), "9.001");
        assert_eq!(cfg.humidity.dpt.identifier(), "9.007");
    }

    /// D6: a temperature-only station leaves the hub permanently unable to
    /// derive dew point for the slot, so it is refused at startup.
    #[test]
    fn a_missing_quantity_table_is_rejected_by_name() {
        let err = profile(
            r#"
            gateway = "knx://192.168.1.4:3671"

            [temperature]
            group_address = "9/1/0"
            "#,
        )
        .resolve()
        .unwrap_err();
        assert!(err.contains("[knx.humidity]"), "{err}");

        let err = profile(
            r#"
            gateway = "knx://192.168.1.4:3671"

            [humidity]
            group_address = "9/1/1"
            "#,
        )
        .resolve()
        .unwrap_err();
        assert!(err.contains("[knx.temperature]"), "{err}");
    }

    #[test]
    fn one_address_cannot_feed_both_quantities() {
        let err = profile(
            r#"
            gateway = "knx://192.168.1.4:3671"

            [temperature]
            group_address = "9/1/0"

            [humidity]
            group_address = "9/1/0"
            "#,
        )
        .resolve()
        .unwrap_err();
        assert!(err.contains("9/1/0"), "{err}");
    }

    /// Equality is on the canonical form, so a padded spelling of the same
    /// address does not slip past the duplicate check.
    #[test]
    fn duplicate_detection_sees_through_spelling() {
        let err = profile(
            r#"
            gateway = "knx://192.168.1.4:3671"

            [temperature]
            group_address = "9/1/0"

            [humidity]
            group_address = "09/01/00"
            "#,
        )
        .resolve()
        .unwrap_err();
        assert!(err.contains("9/1/0"), "{err}");
    }

    #[test]
    fn a_wrong_dpt_for_the_quantity_is_rejected() {
        let err = profile(
            r#"
            gateway = "knx://192.168.1.4:3671"

            [temperature]
            group_address = "9/1/0"
            dpt = "9.004"

            [humidity]
            group_address = "9/1/1"
            "#,
        )
        .resolve()
        .unwrap_err();
        assert!(err.contains("9.001"), "{err}");
    }

    #[test]
    fn group_addresses_parse_and_normalise() {
        assert_eq!(parse_group_address("9/1/0").unwrap(), "9/1/0");
        assert_eq!(parse_group_address("0/0/0").unwrap(), "0/0/0");
        assert_eq!(parse_group_address("31/7/255").unwrap(), "31/7/255");
        assert_eq!(parse_group_address("09/01/007").unwrap(), "9/1/7");
    }

    #[test]
    fn out_of_range_and_malformed_addresses_are_rejected() {
        for raw in [
            "32/0/0",  // main above 31
            "0/8/0",   // middle above 7
            "0/0/256", // sub above 255
            "1/0",     // 2-level notation
            "1/0/0/0", // 4 levels
            "a/b/c",   // not numbers
            "",        // empty
            "9//0",    // empty level
            "-1/0/0",  // negative
            " 9/1/0",  // stray whitespace
        ] {
            assert!(
                parse_group_address(raw).is_err(),
                "expected '{raw}' to be rejected"
            );
        }
    }

    /// The message has to name the level that is wrong; "invalid group
    /// address" sends the operator back to the KNX spec.
    #[test]
    fn a_range_error_names_the_level() {
        let err = parse_group_address("32/0/0").unwrap_err();
        assert!(err.contains("main"), "{err}");
        assert!(err.contains("0–31"), "{err}");
    }
}
