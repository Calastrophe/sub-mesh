//! Pure display helpers: percentages, times, masked labels and window names.

use std::fmt::Display;

use chrono::{DateTime, Duration, TimeZone, Utc};
use mesh_core::api::Subscription;
use mesh_core::{Window, WindowClass};

const MASK: &str = "•••";

/// A fraction as a whole percent, like `58%`. Not clamped, so sums over
/// several subscriptions can exceed 100%. A non-finite percentage shows as `--`.
pub fn percent(fraction: f64) -> String {
    let percent = fraction * 100.0;
    if !percent.is_finite() {
        return "--".to_owned();
    }
    format!("{:.0}%", percent.round() + 0.0)
}

/// A length of time, like `3h 5m` or `2 days`.
pub fn span(duration: Duration) -> String {
    let secs = duration.num_seconds();
    match secs {
        ..60 => "<1m".to_owned(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => {
            let (hours, minutes) = (secs / 3600, secs % 3600 / 60);
            if minutes == 0 {
                format!("{hours}h")
            } else {
                format!("{hours}h {minutes}m")
            }
        }
        _ => match secs / 86_400 {
            1 => "1 day".to_owned(),
            days => format!("{days} days"),
        },
    }
}

/// `5m ago` style text for a past instant.
pub fn ago(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let secs = (now - then).num_seconds();
    match secs {
        ..10 => "just now".to_owned(),
        10..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86_400 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// Month, day and minute in the given zone, like `09/14, 18:23`.
pub fn clock<Tz>(at: DateTime<Utc>, zone: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    at.with_timezone(zone).format("%m/%d, %H:%M").to_string()
}

/// Like [`clock`] with seconds, for log tables: `09/14 18:23:05`.
pub fn clock_seconds<Tz>(at: DateTime<Utc>, zone: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    at.with_timezone(zone).format("%m/%d %H:%M:%S").to_string()
}

/// The full time with milliseconds and the zone's offset, for detail views:
/// `2026-09-14 18:23:05.120 +02:00`.
pub fn full_time<Tz>(at: DateTime<Utc>, zone: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    at.with_timezone(zone)
        .format("%Y-%m-%d %H:%M:%S%.3f %:z")
        .to_string()
}

/// A whole number with comma separators: `1,234,567`.
pub fn thousands(count: u64) -> String {
    let digits = count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// When a window resets: `in 4 days · 09/16, 14:00`, or `No reset pending`.
pub fn reset_text<Tz>(now: DateTime<Utc>, at: Option<DateTime<Utc>>, zone: &Tz) -> String
where
    Tz: TimeZone,
    Tz::Offset: Display,
{
    match at {
        Some(at) if at > now => format!("in {} · {}", span(at - now), clock(at, zone)),
        _ => "No reset pending".to_owned(),
    }
}

/// Response time, like `340 ms` or `1.2 s`.
pub fn latency(millis: u64) -> String {
    if millis < 1000 {
        format!("{millis} ms")
    } else {
        format!("{:.1} s", millis as f64 / 1000.0)
    }
}

/// How long something took, from `340 ms` up to `1h 5m`.
pub fn elapsed(millis: u64) -> String {
    let secs = millis / 1000;
    match secs {
        0..60 => latency(millis),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
    }
}

/// Number of dots in the priority display.
pub const PRIORITY_DOTS: u8 = 5;

/// How many of the [`PRIORITY_DOTS`] to fill for a balancer score, relative
/// to `best`, the highest score among the same provider's subscriptions.
/// The best gets all of them, any other candidate at least one, and a
/// subscription that is not a candidate (`score` is `None`) none.
pub fn priority_dots(score: Option<f64>, best: Option<f64>) -> u8 {
    let Some(score) = score.filter(|score| score.is_finite() && *score >= 0.0) else {
        return 0;
    };
    let ratio = best
        .filter(|best| best.is_finite() && *best > 0.0)
        .map_or(0.0, |best| (score / best).clamp(0.0, 1.0));
    ((ratio * f64::from(PRIORITY_DOTS)).ceil() as u8).clamp(1, PRIORITY_DOTS)
}

fn head(text: &str, keep: usize) -> String {
    let keep = keep.min(text.chars().count() / 2);
    text.chars().take(keep).collect()
}

/// Hide most of a label: `claude-tester@example.com` becomes
/// `clau•••@ex•••.com`.
pub fn mask_label(label: &str) -> String {
    let Some((local, domain)) = label.split_once('@') else {
        return format!("{}{MASK}", head(label, 4));
    };
    let (host, tld) = match domain.rsplit_once('.') {
        Some((host, tld)) => (host, Some(tld)),
        None => (domain, None),
    };
    let mut masked = format!("{}{MASK}@{}{MASK}", head(local, 4), head(host, 2));
    if let Some(tld) = tld {
        masked.push('.');
        masked.push_str(tld);
    }
    masked
}

/// The text shown for a subscription's name: its label, or its id when it
/// has none. Masked unless `show_full`.
pub fn label_text(sub: &Subscription, show_full: bool) -> String {
    let raw = sub.label.as_deref().unwrap_or_else(|| sub.id.as_str());
    display_label(raw, show_full)
}

/// Render a label according to the dashboard's visibility setting.
pub fn display_label(label: &str, show_full: bool) -> String {
    if show_full {
        label.to_owned()
    } else {
        mask_label(label)
    }
}

/// Human name of a window class: `5-hour` or `Weekly`.
pub fn class_name(class: WindowClass) -> &'static str {
    match class {
        WindowClass::FiveHour => "5-hour",
        WindowClass::Weekly => "Weekly",
    }
}

/// Human name of a window.
pub fn window_label(window: &Window) -> &'static str {
    class_name(window.class())
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;

    use super::*;

    fn at(hour: u32, minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 12, hour, minute, 0).unwrap()
    }

    #[test]
    fn percent_rounds_and_handles_edges() {
        assert_eq!(percent(0.584), "58%");
        assert_eq!(percent(0.585), "59%");
        assert_eq!(percent(4.09), "409%");
        assert_eq!(percent(-0.0), "0%");
        assert_eq!(percent(f64::NAN), "--");
        assert_eq!(percent(f64::INFINITY), "--");
        assert_eq!(percent(f64::MAX), "--");
    }

    #[test]
    fn span_picks_a_unit() {
        assert_eq!(span(Duration::seconds(20)), "<1m");
        assert_eq!(span(Duration::minutes(12)), "12m");
        assert_eq!(span(Duration::minutes(180)), "3h");
        assert_eq!(span(Duration::minutes(185)), "3h 5m");
        assert_eq!(span(Duration::hours(25)), "1 day");
        assert_eq!(span(Duration::hours(100)), "4 days");
    }

    #[test]
    fn ago_reads_naturally() {
        let now = at(10, 0);
        assert_eq!(ago(now, now - Duration::seconds(3)), "just now");
        assert_eq!(ago(now, now - Duration::seconds(30)), "30s ago");
        assert_eq!(ago(now, now - Duration::minutes(5)), "5m ago");
        assert_eq!(ago(now, now - Duration::hours(3)), "3h ago");
        assert_eq!(ago(now, now - Duration::days(2)), "2d ago");
    }

    #[test]
    fn reset_text_combines_relative_and_clock() {
        let now = Utc.with_ymd_and_hms(2026, 9, 12, 18, 23, 0).unwrap();
        let reset = now + Duration::days(2);
        assert_eq!(
            reset_text(now, Some(reset), &Utc),
            "in 2 days · 09/14, 18:23"
        );
        let plus_two = FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(
            reset_text(now, Some(reset), &plus_two),
            "in 2 days · 09/14, 20:23"
        );
        assert_eq!(reset_text(now, None, &Utc), "No reset pending");
        assert_eq!(reset_text(now, Some(now), &Utc), "No reset pending");
    }

    #[test]
    fn full_time_shows_milliseconds_and_offset() {
        let ts = at(18, 23) + Duration::milliseconds(120);
        assert_eq!(full_time(ts, &Utc), "2026-09-12 18:23:00.120 +00:00");
        let plus_two = FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(full_time(ts, &plus_two), "2026-09-12 20:23:00.120 +02:00");
    }

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn formats_latency() {
        assert_eq!(latency(340), "340 ms");
        assert_eq!(latency(1234), "1.2 s");
    }

    #[test]
    fn formats_elapsed_time() {
        assert_eq!(elapsed(4), "4 ms");
        assert_eq!(elapsed(12_340), "12.3 s");
        assert_eq!(elapsed(72_000), "1m 12s");
        assert_eq!(elapsed(3_900_000), "1h 5m");
    }

    #[test]
    fn priority_dots_are_relative_to_the_best() {
        assert_eq!(priority_dots(Some(2.31e-3), Some(2.31e-3)), 5);
        assert_eq!(priority_dots(Some(1.0e-3), Some(4.0e-3)), 2);
        assert_eq!(priority_dots(Some(1.0e-9), Some(4.0e-3)), 1);
        assert_eq!(priority_dots(Some(0.0), Some(0.0)), 1);
        assert_eq!(priority_dots(None, Some(1.0)), 0);
    }

    #[test]
    fn mask_label_hides_most_of_an_email() {
        assert_eq!(mask_label("claude-tester@example.com"), "clau•••@ex•••.com");
        assert_eq!(mask_label("a@b.io"), "•••@•••.io");
        assert_eq!(mask_label("noatsign-label"), "noat•••");
        assert_eq!(mask_label("x@localhost"), "•••@lo•••");
        assert_eq!(mask_label(""), "•••");
    }

    #[test]
    fn mask_label_is_unicode_safe() {
        assert_eq!(mask_label("éééééééé@ééé.com"), "éééé•••@é•••.com");
    }

    #[test]
    fn window_names_follow_the_class() {
        assert_eq!(class_name(WindowClass::FiveHour), "5-hour");
        assert_eq!(class_name(WindowClass::Weekly), "Weekly");
    }
}
