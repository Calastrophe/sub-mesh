//! Text formatting for `meshctl` output.

use std::fmt::Write as _;

use chrono::{DateTime, Duration, Utc};
use mesh_core::admin::{AdminAccount, AdminStatus};
use mesh_core::{Admission, DenyReason, ModelAllowlist, Provider, ReserveConfig, Window};

/// Format a fraction as a percentage, with one decimal only when needed.
fn percent(fraction: f64) -> String {
    let rounded = (fraction * 1000.0).round() / 10.0;
    if rounded.fract() == 0.0 {
        format!("{rounded:.0}%")
    } else {
        format!("{rounded:.1}%")
    }
}

/// Format a positive span in whole minutes, omitting spans under a minute.
fn format_duration(span: Duration) -> Option<String> {
    let minutes = span.num_minutes();
    if minutes <= 0 {
        return None;
    }
    let (days, hours, minutes) = (minutes / (24 * 60), minutes / 60 % 24, minutes % 60);
    Some(if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else {
        format!("{minutes}m")
    })
}

fn resets_text(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    match resets_at {
        None => "-".to_owned(),
        Some(at) => match format_duration(at - now) {
            None => "resets now".to_owned(),
            Some(span) => format!("resets in {span}"),
        },
    }
}

fn tunnel_text(status: &AdminStatus, now: DateTime<Utc>) -> String {
    match (status.tunnel_connected_since, &status.tunnel_error) {
        (Some(since), _) => format!("connected {}", ago_text(since, now)),
        (None, Some(error)) => format!("down: {error}"),
        (None, None) => "connecting".to_owned(),
    }
}

fn ago_text(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    match format_duration(now - at) {
        None => "just now".to_owned(),
        Some(span) => format!("{span} ago"),
    }
}

/// One line describing a reserve setting.
pub fn format_reserve(reserve: &ReserveConfig) -> String {
    let mut text = format!("default {}", percent(reserve.default));
    if let Some(value) = reserve.five_hour {
        let _ = write!(text, ", five-hour {}", percent(value));
    }
    if let Some(value) = reserve.weekly {
        let _ = write!(text, ", weekly {}", percent(value));
    }
    text
}

/// The models an allowlist allows: `all` for no allowlist, otherwise the
/// patterns.
pub fn format_models(models: Option<&ModelAllowlist>) -> String {
    models.map_or_else(|| "all".to_owned(), ToString::to_string)
}

/// The config default allowlist of every provider, one per line. A provider
/// without a default allows every model.
pub fn format_default_models(defaults: &[(Provider, ModelAllowlist)]) -> String {
    let rows: Vec<Vec<String>> = Provider::ALL
        .into_iter()
        .map(|provider| {
            let models = defaults
                .iter()
                .find(|(candidate, _)| *candidate == provider)
                .map(|(_, models)| models);
            vec![format!("{provider}:"), format_models(models)]
        })
        .collect();
    format!("default models:\n{}", table(&rows, &[], "  "))
}

/// The daemon's decision for an account, in words.
fn format_admission(admission: &Admission) -> String {
    match admission {
        Admission::Admit { headroom } => format!("admit ({} headroom)", percent(*headroom)),
        Admission::Deny { reason } => match reason {
            DenyReason::Paused => "deny (paused)".to_owned(),
            DenyReason::AuthError => "deny (login broken, run meshctl login)".to_owned(),
            DenyReason::Reserve { window } => format!("deny (reserve reached on {window})"),
            DenyReason::Limited { window } => format!("deny ({window} is rate limited)"),
        },
    }
}

/// Column alignment for [`table`].
enum Align {
    Left,
    Right,
}

/// Lay out `rows` in aligned columns, each line starting with `indent`.
/// Columns are separated by two spaces and lines have no trailing spaces.
fn table(rows: &[Vec<String>], aligns: &[Align], indent: &str) -> String {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let mut line = String::from(indent);
        for (column, cell) in row.iter().enumerate() {
            if column > 0 {
                line.push_str("  ");
            }
            let width = widths[column];
            if matches!(aligns.get(column), Some(Align::Right)) {
                let _ = write!(line, "{cell:>width$}");
            } else {
                let _ = write!(line, "{cell:<width$}");
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// A table of windows: `id  used%  reserve%  resets in 2h13m`. The reserve
/// column is left out when `reserve` is `None`.
pub fn window_table(
    windows: &[Window],
    reserve: Option<&ReserveConfig>,
    now: DateTime<Utc>,
    indent: &str,
) -> String {
    let rows: Vec<Vec<String>> = windows
        .iter()
        .map(|window| {
            let mut row = vec![window.id.clone(), percent(window.used_at(now))];
            if let Some(reserve) = reserve {
                row.push(percent(reserve.reserve_for(window)));
            }
            let mut resets = resets_text(window.resets_at, now);
            if window.limited_at(now) {
                resets.push_str(" (limited)");
            }
            row.push(resets);
            row
        })
        .collect();
    let aligns = if reserve.is_some() {
        &[Align::Left, Align::Right, Align::Right][..]
    } else {
        &[Align::Left, Align::Right]
    };
    table(&rows, aligns, indent)
}

/// The account's own reserve if set, otherwise the default.
pub fn effective_reserve<'a>(
    account: &'a AdminAccount,
    default: &'a ReserveConfig,
) -> &'a ReserveConfig {
    account.reserve.as_ref().unwrap_or(default)
}

fn errors_text(account: &AdminAccount) -> String {
    let mut parts = Vec::new();
    if let Some(error) = &account.auth_error {
        parts.push(format!("login: {error}"));
    }
    if let Some(error) = &account.last_poll_error {
        parts.push(format!("usage poll: {error}"));
    }
    if parts.is_empty() {
        "-".to_owned()
    } else {
        parts.join("; ")
    }
}

pub fn render_status(status: &AdminStatus, now: DateTime<Utc>) -> String {
    let report = match (&status.last_report_ok, &status.last_report_error) {
        (Some(at), None) => format!("ok {}", ago_text(*at, now)),
        (Some(at), Some(error)) => format!("error: {error} (last ok {})", ago_text(*at, now)),
        (None, Some(error)) => format!("error: {error}"),
        (None, None) => "none yet".to_owned(),
    };
    let header = [
        ("node", status.node.clone()),
        ("server", status.server_url.clone()),
        ("tunnel", tunnel_text(status, now)),
        ("last report", report),
        ("default reserve", format_reserve(&status.default_reserve)),
    ];
    let rows: Vec<Vec<String>> = header
        .into_iter()
        .map(|(key, value)| vec![format!("{key}:"), value])
        .collect();
    let mut out = table(&rows, &[], "");

    if status.accounts.is_empty() {
        out.push_str("\nNo accounts. Run `meshctl login codex` or `meshctl login claude`.\n");
        return out;
    }
    for account in &status.accounts {
        let report = &account.report;
        let label = report.label.as_deref().unwrap_or("(no label)");
        let paused = if account.paused { "paused" } else { "active" };
        let reserve = effective_reserve(account, &status.default_reserve);
        let _ = write!(
            out,
            "\n{}  {}  {label}  {paused}\n",
            report.id, report.provider
        );
        let own = if account.models.is_some() {
            " (own)"
        } else {
            ""
        };
        let details = vec![
            vec!["decision:".to_owned(), format_admission(&report.admission)],
            vec![
                "models:".to_owned(),
                format!("{}{own}", format_models(report.models.as_ref())),
            ],
            vec!["errors:".to_owned(), errors_text(account)],
            vec!["updated:".to_owned(), ago_text(report.updated_at, now)],
        ];
        out.push_str(&table(&details, &[], "  "));
        if report.windows.is_empty() {
            out.push_str("  windows: none known yet\n");
        } else {
            out.push_str("  windows (used, reserve):\n");
            out.push_str(&window_table(&report.windows, Some(reserve), now, "    "));
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use chrono::TimeZone;
    use mesh_core::{AccountId, AccountReport, Provider};

    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 20, 12, 0, 0).unwrap()
    }

    #[test]
    fn formats_durations() {
        for (span, expected) in [
            (Duration::seconds(-5), None),
            (Duration::seconds(59), None),
            (Duration::minutes(45), Some("45m")),
            (Duration::minutes(133), Some("2h13m")),
            (Duration::minutes(125), Some("2h05m")),
            (Duration::hours(76), Some("3d4h")),
            (Duration::hours(24), Some("1d0h")),
        ] {
            assert_eq!(format_duration(span).as_deref(), expected, "{span}");
        }
    }

    #[test]
    fn formats_percentages() {
        assert_eq!(percent(0.3), "30%");
        assert_eq!(percent(0.125), "12.5%");
        assert_eq!(percent(0.0), "0%");
        assert_eq!(percent(1.0), "100%");
    }

    #[test]
    fn aligns_table_columns() {
        let rows = vec![
            vec!["a".to_owned(), "5%".to_owned(), "x".to_owned()],
            vec!["long".to_owned(), "100%".to_owned(), "y".to_owned()],
        ];
        let out = table(&rows, &[Align::Left, Align::Right], "> ");
        assert_eq!(out, "> a       5%  x\n> long  100%  y\n");
    }

    pub(crate) fn sample_status() -> AdminStatus {
        let window = |id: &str, minutes: u32, used: f64, reset_min: i64| Window {
            id: id.to_owned(),
            duration_minutes: Some(minutes),
            used,
            resets_at: Some(now() + Duration::minutes(reset_min)),
            limited: false,
        };
        let id: AccountId = "claude-ab12".parse().unwrap();
        let account = AdminAccount {
            report: AccountReport {
                id,
                provider: Provider::Claude,
                label: Some("me@example.com".to_owned()),
                windows: vec![
                    window("five_hour", 300, 0.2, 133),
                    window("seven_day", 10080, 0.55, 3 * 24 * 60 + 4 * 60),
                ],
                admission: Admission::Admit { headroom: 0.15 },
                paused: false,
                reserve: ReserveConfig::default(),
                models: None,
                updated_at: now() - Duration::minutes(2),
            },
            paused: false,
            reserve: Some(ReserveConfig {
                default: 0.3,
                five_hour: Some(0.1),
                weekly: None,
            }),
            models: None,
            auth_error: None,
            last_poll_error: Some("HTTP 500".to_owned()),
        };
        AdminStatus {
            node: "laptop".to_owned(),
            server_url: "http://100.64.0.1:7070".to_owned(),
            tunnel_connected_since: Some(now() - Duration::minutes(5)),
            tunnel_error: None,
            default_reserve: ReserveConfig::default(),
            default_models: Vec::new(),
            last_report_ok: Some(now() - Duration::seconds(30)),
            last_report_error: None,
            accounts: vec![account],
        }
    }

    #[test]
    fn renders_status() {
        let expected = "\
node:             laptop
server:           http://100.64.0.1:7070
tunnel:           connected 5m ago
last report:      ok just now
default reserve:  default 30%

claude-ab12  claude  me@example.com  active
  decision:  admit (15% headroom)
  models:    all
  errors:    usage poll: HTTP 500
  updated:   2m ago
  windows (used, reserve):
    five_hour  20%  10%  resets in 2h13m
    seven_day  55%  30%  resets in 3d4h
";
        assert_eq!(render_status(&sample_status(), now()), expected);
    }

    #[test]
    fn renders_effective_models_and_marks_own() {
        let list = |patterns: &[&str]| ModelAllowlist::new(patterns.iter().copied()).unwrap();
        let mut status = sample_status();
        status.accounts[0].report.models = Some(list(&["claude-sonnet-*", "claude-haiku-*"]));
        let text = render_status(&status, now());
        assert!(
            text.contains("  models:    claude-sonnet-*, claude-haiku-*\n"),
            "{text}"
        );

        status.accounts[0].models = status.accounts[0].report.models.clone();
        let text = render_status(&status, now());
        assert!(
            text.contains("  models:    claude-sonnet-*, claude-haiku-* (own)\n"),
            "{text}"
        );
    }

    #[test]
    fn renders_default_models_per_provider() {
        let claude = ModelAllowlist::new(["claude-sonnet-*", "claude-haiku-*"]).unwrap();
        assert_eq!(
            format_default_models(&[(Provider::Claude, claude)]),
            "default models:\n  codex:   all\n  claude:  claude-sonnet-*, claude-haiku-*\n"
        );
        assert_eq!(
            format_default_models(&[]),
            "default models:\n  codex:   all\n  claude:  all\n"
        );
        assert_eq!(format_models(None), "all");
    }

    #[test]
    fn renders_denied_and_empty() {
        let mut status = sample_status();
        status.accounts[0].report.admission = Admission::Deny {
            reason: DenyReason::Reserve {
                window: "seven_day".to_owned(),
            },
        };
        status.accounts[0].paused = true;
        status.accounts[0].report.windows.clear();
        let text = render_status(&status, now());
        assert!(text.contains("paused"));
        assert!(text.contains("deny (reserve reached on seven_day)"));
        assert!(text.contains("windows: none known yet"));

        status.accounts.clear();
        assert!(render_status(&status, now()).contains("No accounts."));
    }

    #[test]
    fn window_table_without_reserve_column() {
        let status = sample_status();
        let text = window_table(&status.accounts[0].report.windows, None, now(), "");
        assert_eq!(
            text,
            "five_hour  20%  resets in 2h13m\nseven_day  55%  resets in 3d4h\n"
        );
    }
}
