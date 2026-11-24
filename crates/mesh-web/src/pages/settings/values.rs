use mesh_core::api::{SettingKind, SettingName, SettingValue};

use crate::format::{check_setting, setting_bounds};

const MIB: f64 = 1_048_576.0;

pub fn label(name: SettingName) -> &'static str {
    match name {
        SettingName::CacheTtlSecs => "Cache TTL",
        SettingName::AffinityMinHeadroom => "Affinity headroom",
        SettingName::MaxAttempts => "Max attempts",
        SettingName::DaemonStaleSecs => "Daemon stale after",
        SettingName::MaxBodyBytes => "Max body",
        SettingName::ConsumersAllowLoopback => "Consumer loopback",
        SettingName::DaemonsAllowLoopback => "Daemon loopback",
    }
}

pub fn unit(kind: SettingKind) -> &'static str {
    match kind {
        SettingKind::Seconds => "s",
        SettingKind::Fraction => "%",
        SettingKind::Bytes => "MiB",
        SettingKind::Count | SettingKind::Flag => "",
    }
}

fn trimmed(number: f64) -> String {
    let rounded = (number * 100.0).round() / 100.0;
    format!("{}", rounded + 0.0)
}

pub fn to_text(kind: SettingKind, value: SettingValue) -> String {
    match kind {
        SettingKind::Flag => if value.as_flag() { "on" } else { "off" }.to_owned(),
        SettingKind::Fraction => trimmed(value.as_f64() * 100.0),
        SettingKind::Bytes => trimmed(value.as_f64() / MIB),
        SettingKind::Seconds | SettingKind::Count => trimmed(value.as_f64()),
    }
}

pub fn shown(kind: SettingKind, value: SettingValue) -> String {
    let text = to_text(kind, value);
    match unit(kind) {
        "" => text,
        "%" => format!("{text}%"),
        unit => format!("{text} {unit}"),
    }
}

pub fn parse(
    name: SettingName,
    text: &str,
    current: SettingValue,
    config: SettingValue,
) -> Result<SettingValue, String> {
    let kind = name.kind();
    let text = text.trim();
    if text == to_text(kind, current) {
        return Ok(current);
    }
    let text = text.strip_suffix(unit(kind)).unwrap_or(text).trim_end();
    let number: f64 = text
        .parse()
        .map_err(|_| format!("{} must be a number", label(name)))?;
    let value = match kind {
        SettingKind::Fraction => SettingValue::Fraction(number / 100.0),
        SettingKind::Bytes => SettingValue::Fraction((number * MIB).round()),
        SettingKind::Seconds | SettingKind::Count => SettingValue::Fraction(number),
        SettingKind::Flag => return Err(format!("{} is on or off", label(name))),
    };
    check_setting(name, value, config).map_err(|reason| {
        let scale = match kind {
            SettingKind::Fraction => 100.0,
            SettingKind::Bytes => 1.0 / MIB,
            _ => return reason,
        };
        let (low, high) = setting_bounds(name, config);
        format!(
            "{} must be between {} and {} {}",
            label(name),
            trimmed(low * scale),
            trimmed(high * scale),
            unit(kind)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_show_in_their_units() {
        assert_eq!(
            to_text(SettingKind::Fraction, SettingValue::Fraction(0.02)),
            "2"
        );
        assert_eq!(
            shown(SettingKind::Fraction, SettingValue::Fraction(0.055)),
            "5.5%"
        );
        assert_eq!(
            shown(SettingKind::Bytes, SettingValue::Whole(64 * 1024 * 1024)),
            "64 MiB"
        );
        assert_eq!(
            shown(SettingKind::Seconds, SettingValue::Whole(300)),
            "300 s"
        );
        assert_eq!(shown(SettingKind::Flag, SettingValue::Flag(true)), "on");
    }

    #[test]
    fn text_parses_back() {
        let headroom = SettingName::AffinityMinHeadroom;
        let current = SettingValue::Fraction(0.123_456);
        assert_eq!(parse(headroom, "12.35", current, current), Ok(current));
        assert_eq!(
            parse(headroom, "25%", current, current),
            Ok(SettingValue::Fraction(0.25))
        );
        assert!(parse(headroom, "150", current, current).is_err());
        assert_eq!(
            parse(
                SettingName::MaxBodyBytes,
                "16",
                SettingValue::Whole(1),
                SettingValue::Whole(1)
            ),
            Ok(SettingValue::Whole(16 * 1024 * 1024))
        );
        assert_eq!(
            parse(
                SettingName::MaxAttempts,
                "4",
                SettingValue::Whole(3),
                SettingValue::Whole(3)
            ),
            Ok(SettingValue::Whole(4))
        );
        assert!(
            parse(
                SettingName::MaxAttempts,
                "2.5",
                SettingValue::Whole(3),
                SettingValue::Whole(3)
            )
            .is_err()
        );
        assert!(
            parse(
                SettingName::CacheTtlSecs,
                "x",
                SettingValue::Whole(3),
                SettingValue::Whole(3)
            )
            .is_err()
        );
        let error = parse(
            SettingName::MaxBodyBytes,
            "0",
            SettingValue::Whole(1 << 26),
            SettingValue::Whole(1 << 26),
        )
        .unwrap_err();
        assert!(error.contains("128 MiB"), "{error}");
        assert_eq!(
            parse(
                SettingName::MaxBodyBytes,
                "200",
                SettingValue::Whole(1 << 26),
                SettingValue::Whole(256 << 20),
            ),
            Ok(SettingValue::Whole(200 << 20))
        );
    }
}
