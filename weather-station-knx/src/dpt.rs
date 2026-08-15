//! Datapoint types, resolved from the profile and applied to bus telegrams.
//!
//! A KNX installation is free to send humidity as a 2-byte float (DPT 9.007)
//! or a 1-byte percentage (DPT 5.001), so the profile names the type per group
//! address. The allow-list here is per *quantity*: `9.004` is a perfectly
//! valid datapoint type, but it is illuminance, and a station that accepted it
//! for `[knx.temperature]` would publish lux into the mesh as degrees Celsius.
//!
//! Both decoders yield `f32` in the unit the contract expects — °C and
//! percent — so the record wiring in `main.rs` does not care which was chosen.

use aimdb_knx_connector::dpt::{Dpt5, Dpt9, DptDecode};

/// The datapoint type of `[knx.temperature]`.
///
/// Only DPT 9.001 is accepted: every temperature datapoint a KNX weather
/// sensor sends is 9.001, and the 1-byte types have no temperature meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemperatureDpt;

/// The datapoint type of `[knx.humidity]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HumidityDpt {
    /// DPT 9.007 — 2-byte float, percent.
    Float,
    /// DPT 5.001 — 1-byte scaled percentage, 0–255 raw mapped to 0–100.
    Percent,
}

/// Decoding failed: the telegram does not carry a value of this datapoint
/// type. Carried as a string because `KnxError` is not `Display` here and the
/// only consumer is a log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

impl TemperatureDpt {
    /// Accepted spellings of the temperature datapoint type.
    pub const ACCEPTED: &'static str = r#""9.001" (2-byte float, °C)"#;

    /// Parse the profile's `dpt` string.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "9.001" => Ok(Self),
            other => Err(format!(
                "[knx.temperature] dpt = \"{other}\" is not a temperature type — \
                 use {}",
                Self::ACCEPTED
            )),
        }
    }

    /// Decode one telegram into degrees Celsius.
    pub fn decode(&self, data: &[u8]) -> Result<f32, DecodeError> {
        Dpt9::Temperature
            .decode(data)
            .map_err(|_| DecodeError("not a valid DPT 9.001 value"))
    }

    /// The profile spelling, for the startup route table.
    pub fn identifier(&self) -> &'static str {
        "9.001"
    }
}

impl HumidityDpt {
    /// Accepted spellings of the humidity datapoint type.
    pub const ACCEPTED: &'static str = r#""9.007" (2-byte float) or "5.001" (1-byte percent)"#;

    /// Parse the profile's `dpt` string.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "9.007" => Ok(Self::Float),
            "5.001" => Ok(Self::Percent),
            other => Err(format!(
                "[knx.humidity] dpt = \"{other}\" is not a humidity type — use {}",
                Self::ACCEPTED
            )),
        }
    }

    /// Decode one telegram into percent.
    ///
    /// DPT 5.001 arrives as 0–100 already scaled from the raw 0–255 octet, so
    /// widening to `f32` is all that is left; the contract carries percent
    /// either way.
    pub fn decode(&self, data: &[u8]) -> Result<f32, DecodeError> {
        match self {
            Self::Float => Dpt9::Humidity
                .decode(data)
                .map_err(|_| DecodeError("not a valid DPT 9.007 value")),
            Self::Percent => Dpt5::Percentage
                .decode(data)
                .map(|percent| percent as f32)
                .map_err(|_| DecodeError("not a valid DPT 5.001 value")),
        }
    }

    /// The profile spelling, for the startup route table.
    pub fn identifier(&self) -> &'static str {
        match self {
            Self::Float => "9.007",
            Self::Percent => "5.001",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temperature_accepts_only_9_001() {
        assert_eq!(TemperatureDpt::parse("9.001"), Ok(TemperatureDpt));
        // 9.007 is a humidity type, 5.001 a percentage — both are valid DPTs,
        // and both would be nonsense as degrees Celsius.
        assert!(TemperatureDpt::parse("9.007").is_err());
        assert!(TemperatureDpt::parse("5.001").is_err());
        assert!(TemperatureDpt::parse("9.004").is_err());
        assert!(TemperatureDpt::parse("").is_err());
    }

    #[test]
    fn humidity_accepts_the_float_and_percent_types() {
        assert_eq!(HumidityDpt::parse("9.007"), Ok(HumidityDpt::Float));
        assert_eq!(HumidityDpt::parse("5.001"), Ok(HumidityDpt::Percent));
        assert!(HumidityDpt::parse("9.001").is_err());
        assert!(HumidityDpt::parse("9.004").is_err());
        assert!(HumidityDpt::parse("").is_err());
    }

    /// The rejection has to say what *is* accepted — the operator is holding a
    /// profile and needs to know what to write instead.
    #[test]
    fn rejection_lists_the_accepted_values() {
        let err = HumidityDpt::parse("9.004").unwrap_err();
        assert!(err.contains("9.007"), "{err}");
        assert!(err.contains("5.001"), "{err}");
        assert!(err.contains("[knx.humidity]"), "{err}");

        let err = TemperatureDpt::parse("9.004").unwrap_err();
        assert!(err.contains("9.001"), "{err}");
        assert!(err.contains("[knx.temperature]"), "{err}");
    }

    /// DPT 9 is a 2-byte float: 0x0C1A is 21.0 with exponent 0.
    #[test]
    fn temperature_decodes_a_two_byte_float() {
        let celsius = TemperatureDpt.decode(&[0x0C, 0x1A]).unwrap();
        assert!((celsius - 21.0).abs() < 0.1, "got {celsius}");
    }

    #[test]
    fn humidity_decodes_both_encodings() {
        let float = HumidityDpt::Float.decode(&[0x0C, 0x1A]).unwrap();
        assert!((float - 21.0).abs() < 0.1, "got {float}");

        // DPT 5.001 scales the raw octet 0–255 onto 0–100 %.
        assert_eq!(HumidityDpt::Percent.decode(&[0xFF]).unwrap(), 100.0);
        assert_eq!(HumidityDpt::Percent.decode(&[0x00]).unwrap(), 0.0);
        let half = HumidityDpt::Percent.decode(&[0x80]).unwrap();
        assert!((half - 50.0).abs() <= 1.0, "got {half}");
    }

    /// A short or empty telegram is an error, never a coerced 0 — publishing
    /// 0 °C into the mesh would drag the hub's derived dew point with it.
    #[test]
    fn a_malformed_telegram_is_rejected() {
        assert!(TemperatureDpt.decode(&[]).is_err());
        assert!(TemperatureDpt.decode(&[0x0C]).is_err());
        assert!(HumidityDpt::Float.decode(&[0x0C]).is_err());
        assert!(HumidityDpt::Percent.decode(&[]).is_err());
    }
}
