//! Pure helpers for the request log: merging pages, filtering, and the short
//! texts its table shows.

use mesh_core::api::{Direction, LogEntry, TokenUsage, Transport};

use crate::format::display_label;
use crate::level::Level;
use crate::summary::Filter;

/// How many entries one page asks for.
pub const PAGE: u32 = 100;

/// Combine a fresh newest page with the entries already loaded. A full page
/// keeps older entries when the page reaches the loaded range. Otherwise,
/// the next Load older request starts from the new page to fill the gap.
pub fn merge_newest(page: Vec<LogEntry>, loaded: &[LogEntry]) -> Vec<LogEntry> {
    let Some(oldest) = page.last().map(|entry| entry.id) else {
        return page;
    };
    if page.len() < PAGE as usize || loaded.first().is_some_and(|entry| entry.id < oldest) {
        return page;
    }
    let mut merged = page;
    merged.extend(loaded.iter().filter(|entry| entry.id < oldest).cloned());
    merged
}

/// Add an older page below the loaded entries, skipping any already there.
pub fn append_older(loaded: &mut Vec<LogEntry>, page: Vec<LogEntry>) {
    let oldest = loaded.last().map_or(i64::MAX, |entry| entry.id);
    loaded.extend(page.into_iter().filter(|entry| entry.id < oldest));
}

/// Whether the request failed: an error status, or an error text.
pub fn is_error(entry: &LogEntry) -> bool {
    entry.status >= 400 || entry.error.is_some()
}

/// The level that colors the status. An error text on a success status, as
/// on a WebSocket that opened and then failed, still warns.
pub fn status_level(entry: &LogEntry) -> Level {
    match Level::from_status(entry.status) {
        Level::Ok if entry.error.is_some() => Level::Warn,
        level => level,
    }
}

/// Whether the entry passes the provider filter and, if set, the errors-only
/// switch.
pub fn shown(entry: &LogEntry, filter: Filter, errors_only: bool) -> bool {
    filter.matches(entry.provider) && (!errors_only || is_error(entry))
}

/// Method and path, like `POST /v1/messages`.
pub fn request_text(entry: &LogEntry) -> String {
    format!("{} /{}", entry.method, entry.path.trim_start_matches('/'))
}

/// Name of how the request arrived.
pub fn transport_name(transport: Transport) -> &'static str {
    match transport {
        Transport::Http => "HTTP",
        Transport::Websocket => "WebSocket",
    }
}

/// Name of which way the request went.
pub fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Outbound => "Outbound",
        Direction::Inbound => "Inbound",
    }
}

/// A tailnet login, masked unless `show_full`, or a dash when unknown.
pub fn login_text(login: Option<&str>, show_full: bool) -> String {
    login.map_or_else(|| "—".to_owned(), |login| display_label(login, show_full))
}

/// A count in few characters: `840`, `1.2k`, `48k`, `3.4M`.
pub fn compact(count: u64) -> String {
    if count < 1000 {
        return count.to_string();
    }
    const UNITS: [&str; 6] = ["k", "M", "G", "T", "P", "E"];
    let mut value = count as f64 / 1000.0;
    let mut unit = 0;
    loop {
        let rounded = if value < 10.0 {
            (value * 10.0).round() / 10.0
        } else {
            value.round()
        };
        if rounded >= 1000.0 && unit + 1 < UNITS.len() {
            value /= 1000.0;
            unit += 1;
        } else {
            return format!("{rounded}{}", UNITS[unit]);
        }
    }
}

/// Input and output tokens, like `12k / 840`.
pub fn tokens_text(usage: &TokenUsage) -> String {
    format!("{} / {}", compact(usage.input), compact(usage.output))
}

#[cfg(test)]
mod tests {
    use mesh_core::Provider;

    use super::*;
    use crate::mock;

    fn entries() -> Vec<LogEntry> {
        mock::logs(500, None, None)
    }

    fn ids(entries: &[LogEntry]) -> Vec<i64> {
        entries.iter().map(|entry| entry.id).collect()
    }

    #[test]
    fn directions_have_names() {
        assert_eq!(direction_name(Direction::Outbound), "Outbound");
        assert_eq!(direction_name(Direction::Inbound), "Inbound");
    }

    #[test]
    fn logins_are_masked_unless_shown() {
        assert_eq!(login_text(Some("bob@example.com"), true), "bob@example.com");
        assert_eq!(login_text(Some("bob@example.com"), false), "b•••@ex•••.com");
        assert_eq!(login_text(None, true), "—");
        assert_eq!(login_text(None, false), "—");
    }

    #[test]
    fn compact_counts() {
        assert_eq!(compact(0), "0");
        assert_eq!(compact(840), "840");
        assert_eq!(compact(1000), "1k");
        assert_eq!(compact(1234), "1.2k");
        assert_eq!(compact(9_960), "10k");
        assert_eq!(compact(48_200), "48k");
        assert_eq!(compact(999_499), "999k");
        assert_eq!(compact(999_999), "1M");
        assert_eq!(compact(3_400_000), "3.4M");
        assert_eq!(compact(u64::MAX), "18E");
    }

    #[test]
    fn token_texts() {
        let usage = TokenUsage {
            input: 12_400,
            cache_read: 9_100,
            cache_write: 0,
            output: 840,
            reasoning: 0,
        };
        assert_eq!(tokens_text(&usage), "12k / 840");
    }

    #[test]
    fn errors_are_found_by_status_or_text() {
        let all = entries();
        assert!(all.iter().any(is_error));
        assert!(all.iter().any(|entry| !is_error(entry)));
        let mut entry = all.iter().find(|entry| !is_error(entry)).unwrap().clone();
        assert_eq!(status_level(&entry), Level::Ok);
        entry.error = Some("stream cut".to_owned());
        assert!(is_error(&entry));
        assert_eq!(status_level(&entry), Level::Warn);
        entry.status = 502;
        assert_eq!(status_level(&entry), Level::Hot);
    }

    #[test]
    fn filters_combine() {
        let all = entries();
        let codex = Filter::Only(Provider::Codex);
        assert!(
            all.iter()
                .filter(|entry| shown(entry, codex, false))
                .all(|entry| entry.provider == Provider::Codex)
        );
        let failed: Vec<_> = all
            .iter()
            .filter(|entry| shown(entry, Filter::All, true))
            .collect();
        assert!(!failed.is_empty() && failed.iter().all(|entry| is_error(entry)));
        assert!(all.iter().all(|entry| shown(entry, Filter::All, false)));
    }

    #[test]
    fn request_text_has_one_slash() {
        let mut entry = entries().remove(0);
        entry.method = "POST".to_owned();
        entry.path = "v1/messages".to_owned();
        assert_eq!(request_text(&entry), "POST /v1/messages");
        entry.path = "/responses".to_owned();
        assert_eq!(request_text(&entry), "POST /responses");
    }

    #[test]
    fn a_short_page_replaces_everything() {
        let all = entries();
        let page = all[..3].to_vec();
        assert_eq!(merge_newest(page.clone(), &all), page);
        assert!(merge_newest(Vec::new(), &all).is_empty());
    }

    #[test]
    fn a_full_page_keeps_older_loaded_entries() {
        let all = entries();
        assert!(all.len() > PAGE as usize + 20);
        let loaded = &all[10..PAGE as usize + 20];
        let page = all[..PAGE as usize].to_vec();
        let merged = merge_newest(page, loaded);
        assert_eq!(ids(&merged), ids(&all[..PAGE as usize + 20]));
    }

    #[test]
    fn a_full_page_without_overlap_restarts_paging_at_the_gap() {
        let all = entries();
        let (page, older) = all.split_at(PAGE as usize);
        assert_eq!(merge_newest(page.to_vec(), older), page);
    }

    #[test]
    fn older_pages_append_without_repeats() {
        let all = entries();
        let mut loaded = all[..30].to_vec();
        append_older(&mut loaded, all[20..60].to_vec());
        assert_eq!(ids(&loaded), ids(&all[..60]));
        append_older(&mut loaded, Vec::new());
        assert_eq!(loaded.len(), 60);
    }
}
