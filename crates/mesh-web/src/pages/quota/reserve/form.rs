//! Conversion between a [`ReserveConfig`] and the percent fields of the
//! reserve editor.

use mesh_core::ReserveConfig;

/// Text of the three editor fields for a reserve, in percent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fields {
    pub default: String,
    pub five_hour: String,
    pub weekly: String,
}

fn to_field(fraction: f64) -> String {
    let percent = fraction * 100.0;
    let rounded = (percent * 10.0).round() / 10.0;
    if (percent - rounded).abs() <= percent.abs() * f64::EPSILON {
        rounded.to_string()
    } else {
        percent.to_string()
    }
}

impl Fields {
    /// The fields showing `reserve`. An unset override is an empty field.
    pub fn from_reserve(reserve: &ReserveConfig) -> Self {
        Self {
            default: to_field(reserve.default),
            five_hour: reserve.five_hour.map(to_field).unwrap_or_default(),
            weekly: reserve.weekly.map(to_field).unwrap_or_default(),
        }
    }

    /// Read the fields back into a reserve. A field left as `base` showed it
    /// keeps its exact value. An empty override field clears that override.
    /// With `five_hour` false the form had no 5-hour field, so that value
    /// stays as in `base`.
    pub fn to_reserve(
        &self,
        base: &ReserveConfig,
        five_hour: bool,
    ) -> Result<ReserveConfig, String> {
        let default = parse_percent(&self.default, "Default", Some(base.default))?
            .ok_or_else(|| "Default reserve is required".to_owned())?;
        Ok(ReserveConfig {
            default,
            five_hour: if five_hour {
                parse_percent(&self.five_hour, "5-hour", base.five_hour)?
            } else {
                base.five_hour
            },
            weekly: parse_percent(&self.weekly, "Weekly", base.weekly)?,
        })
    }
}

fn parse_percent(text: &str, name: &str, initial: Option<f64>) -> Result<Option<f64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let text = text.strip_suffix('%').unwrap_or(text).trim_end();
    let value: f64 = text
        .parse()
        .map_err(|_| format!("{name} reserve must be a number"))?;
    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
        return Err(format!("{name} reserve must be between 0 and 100"));
    }
    Ok(Some(
        initial
            .filter(|&fraction| to_field(fraction) == text)
            .unwrap_or(value / 100.0),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(default: &str, five_hour: &str, weekly: &str) -> Fields {
        Fields {
            default: default.to_owned(),
            five_hour: five_hour.to_owned(),
            weekly: weekly.to_owned(),
        }
    }

    #[test]
    fn shows_fractions_as_percent() {
        let reserve = ReserveConfig {
            five_hour: Some(0.1),
            ..ReserveConfig::default()
        };
        assert_eq!(Fields::from_reserve(&reserve), fields("30", "10", ""));
    }

    #[test]
    fn parses_percent_text() {
        let base = ReserveConfig::default();
        let reserve = fields("25", " 10% ", "").to_reserve(&base, true).unwrap();
        assert_eq!(reserve.default, 0.25);
        assert_eq!(reserve.five_hour, Some(0.1));
        assert_eq!(reserve.weekly, None);
        assert!(fields("25", "10%%", "").to_reserve(&base, true).is_err());
        assert!(fields("25", "", "%").to_reserve(&base, true).is_err());
    }

    #[test]
    fn preserves_unchanged_precision() {
        let base = ReserveConfig {
            default: 0.123_456_789,
            five_hour: Some(0.000_001),
            weekly: Some(0.300_000_000_000_000_04),
        };
        let mut fields = Fields::from_reserve(&base);
        assert_eq!(fields.to_reserve(&base, true).unwrap(), base);
        fields.default = "25".to_owned();
        assert_eq!(
            fields.to_reserve(&base, true).unwrap().five_hour,
            base.five_hour
        );
    }

    #[test]
    fn a_hidden_five_hour_field_keeps_its_value() {
        let base = ReserveConfig {
            default: 0.3,
            five_hour: Some(0.12),
            weekly: None,
        };
        let mut fields = Fields::from_reserve(&base);
        fields.five_hour = String::new();
        fields.weekly = "40".to_owned();
        let kept = fields.to_reserve(&base, false).unwrap();
        assert_eq!(kept.five_hour, Some(0.12));
        assert_eq!(kept.weekly, Some(0.4));
        fields.five_hour = "junk".to_owned();
        assert!(fields.to_reserve(&base, false).is_ok());
        assert!(fields.to_reserve(&base, true).is_err());
    }

    #[test]
    fn rejects_bad_input() {
        let base = ReserveConfig::default();
        assert!(fields("", "", "").to_reserve(&base, true).is_err());
        assert!(fields("abc", "", "").to_reserve(&base, true).is_err());
        assert!(fields("101", "", "").to_reserve(&base, true).is_err());
        assert!(fields("30", "-1", "").to_reserve(&base, true).is_err());
        assert!(fields("30", "", "NaN").to_reserve(&base, true).is_err());
    }
}
