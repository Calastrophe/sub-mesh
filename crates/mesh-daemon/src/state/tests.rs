use std::path::PathBuf;

use chrono::{Duration, Utc};
use mesh_core::{Admission, DenyReason, ModelAllowlist, Provider, ReserveConfig, Window};
use mesh_providers::{Credentials, StoredAccount};

use super::account::merge_windows;
use super::admission::materially_changed;
use super::settings::AccountSettings;
use super::*;

pub fn window(id: &str, used: f64) -> Window {
    Window {
        id: id.to_owned(),
        duration_minutes: Some(300),
        used,
        resets_at: None,
        limited: false,
    }
}

pub fn stored(id: &str, provider: Provider) -> StoredAccount {
    StoredAccount {
        id: id.parse().unwrap(),
        provider,
        label: Some("me@example.com".to_owned()),
        credentials: Credentials {
            access_token: "access".to_owned(),
            refresh_token: Some("refresh".to_owned()),
            expires_at: None,
            chatgpt_account_id: None,
            email: None,
        },
    }
}

pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mesh-daemon-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn test_state(name: &str) -> Arc<AppState> {
    let config: crate::config::Config = r#"
        server_url = "http://server:7070"
        node = "node"
        tailscale_socket = "/nonexistent.sock"
        [allowed_peers]
        allow_loopback = true
    "#
    .parse()
    .unwrap();
    let mut settings = config.resolve(None, "node").unwrap();
    settings.data_dir = temp_dir(name);
    AppState::new(settings, RuntimeSettings::default(), reqwest::Client::new())
}

fn models(patterns: &[&str]) -> ModelAllowlist {
    ModelAllowlist::new(patterns.iter().copied()).unwrap()
}

fn test_state_with_models(name: &str, table: &str) -> Arc<AppState> {
    let config: crate::config::Config = format!(
        r#"
        server_url = "http://server:7070"
        node = "node"
        tailscale_socket = "/nonexistent.sock"
        [allowed_peers]
        allow_loopback = true
        {table}
        "#
    )
    .parse()
    .unwrap();
    let mut settings = config.resolve(None, "node").unwrap();
    settings.data_dir = temp_dir(name);
    AppState::new(settings, RuntimeSettings::default(), reqwest::Client::new())
}

#[test]
fn merge_replaces_same_id_and_keeps_others() {
    let mut current = vec![window("five_hour", 0.2), window("seven_day", 0.5)];
    merge_windows(
        &mut current,
        vec![window("five_hour", 0.9), window("extra", 0.1)],
    );
    assert_eq!(current.len(), 3);
    assert_eq!(current[0].used, 0.9);
    assert_eq!(current[1].used, 0.5);
    assert_eq!(current[2].id, "extra");
}

#[test]
fn decide_orders_paused_auth_then_reserve() {
    let now = Utc::now();
    let reserve = ReserveConfig::default();
    let account = AccountState::new(stored("codex-1", Provider::Codex));
    let mut inner = account.inner();
    inner.windows = vec![window("five_hour", 0.9)];
    inner.auth_error = Some("expired".to_owned());

    let paused = inner.admission(true, &reserve, now);
    assert_eq!(
        paused,
        Admission::Deny {
            reason: DenyReason::Paused
        }
    );

    let auth = inner.admission(false, &reserve, now);
    assert_eq!(
        auth,
        Admission::Deny {
            reason: DenyReason::AuthError
        }
    );

    inner.auth_error = None;
    let over = inner.admission(false, &reserve, now);
    assert_eq!(
        over,
        Admission::Deny {
            reason: DenyReason::Reserve {
                window: "five_hour".to_owned()
            }
        }
    );

    inner.windows[0].used = 0.2;
    assert!(inner.admission(false, &reserve, now).is_admit());
    inner.windows.clear();
    assert!(inner.admission(false, &reserve, now).is_admit());
}

#[test]
fn materially_changed_ignores_headroom_drift() {
    let admit = |headroom| Admission::Admit { headroom };
    let deny = |reason| Admission::Deny { reason };
    assert!(!materially_changed(&admit(0.5), &admit(0.1)));
    assert!(materially_changed(&admit(0.5), &deny(DenyReason::Paused)));
    assert!(materially_changed(&deny(DenyReason::Paused), &admit(0.5)));
    assert!(!materially_changed(
        &deny(DenyReason::Paused),
        &deny(DenyReason::Paused)
    ));
    assert!(materially_changed(
        &deny(DenyReason::Paused),
        &deny(DenyReason::AuthError)
    ));
}

#[test]
fn runtime_settings_round_trip() {
    let dir = temp_dir("runtime");
    let path = dir.join("nested").join("daemon-state.json");
    assert_eq!(
        RuntimeSettings::load(&path).unwrap(),
        RuntimeSettings::default()
    );

    let mut settings = RuntimeSettings {
        default_reserve: Some(ReserveConfig {
            default: 0.4,
            ..ReserveConfig::default()
        }),
        ..RuntimeSettings::default()
    };
    settings.accounts.insert(
        "claude-1".parse().unwrap(),
        AccountSettings {
            paused: true,
            reserve: Some(ReserveConfig::default()),
            models: Some(models(&["claude-sonnet-*"])),
        },
    );
    settings.save(&path).unwrap();
    assert!(!path.with_extension("json.tmp").exists());
    assert_eq!(RuntimeSettings::load(&path).unwrap(), settings);

    std::fs::write(&path, b"not json").unwrap();
    assert!(RuntimeSettings::load(&path).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn effective_reserve_prefers_account_then_runtime_default() {
    let state = test_state("reserve");
    let id: AccountId = "claude-1".parse().unwrap();
    state.insert_account(stored("claude-1", Provider::Claude));
    let account = state.account(&id).unwrap();
    account.inner().windows = vec![window("five_hour", 0.6)];
    let now = Utc::now();

    assert!(state.admission(&account, now).is_admit());

    let tight = ReserveConfig {
        default: 0.5,
        ..ReserveConfig::default()
    };
    state.set_default_reserve(Some(tight)).unwrap();
    assert!(!state.admission(&account, now).is_admit());
    assert_eq!(state.admin_status(now).default_reserve.default, 0.5);

    let loose = ReserveConfig {
        default: 0.0,
        ..ReserveConfig::default()
    };
    state.set_account_reserve(&id, Some(loose)).unwrap();
    assert!(state.admission(&account, now).is_admit());

    state.set_account_reserve(&id, None).unwrap();
    assert!(!state.admission(&account, now).is_admit());

    state.set_paused(&id, true).unwrap();
    assert_eq!(
        state.admission(&account, now),
        Admission::Deny {
            reason: DenyReason::Paused
        }
    );
    let saved = RuntimeSettings::load(&state.settings.runtime_path()).unwrap();
    assert!(saved.accounts[&id].paused);
    assert_eq!(saved.default_reserve.unwrap().default, 0.5);

    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[test]
fn mutate_tracks_auth_error_and_expired_windows() {
    let state = test_state("mutate");
    let account = state.insert_account(stored("codex-1", Provider::Codex));
    let (_, after) = state.mutate(&account, |inner| inner.auth_error = Some("bad".to_owned()));
    assert_eq!(
        after,
        Admission::Deny {
            reason: DenyReason::AuthError
        }
    );

    let mut old = window("primary", 0.99);
    old.resets_at = Some(Utc::now() - Duration::minutes(1));
    let (_, after) = state.mutate(&account, |inner| {
        inner.auth_error = None;
        inner.merge(vec![old], Utc::now());
    });
    assert!(after.is_admit());
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[test]
fn rejects_bad_reserve_changes() {
    let state = test_state("badreserve");
    let bad = ReserveConfig {
        default: 1.5,
        ..ReserveConfig::default()
    };
    assert!(matches!(
        state.set_default_reserve(Some(bad)),
        Err(StateError::Invalid(_))
    ));
    assert!(matches!(
        state.set_default_reserve(None),
        Err(StateError::Invalid(_))
    ));
    let unknown: AccountId = "nope".parse().unwrap();
    assert!(matches!(
        state.set_paused(&unknown, true),
        Err(StateError::NotFound(_))
    ));
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[test]
fn daemon_report_carries_paused_reserve_and_owner() {
    let state = test_state("report-fields");
    state.insert_account(stored("codex-1", Provider::Codex));
    state.insert_account(stored("claude-1", Provider::Claude));
    let paused_id: AccountId = "codex-1".parse().unwrap();
    let own = ReserveConfig {
        default: 0.8,
        ..ReserveConfig::default()
    };
    state.set_paused(&paused_id, true).unwrap();
    state
        .set_account_reserve(&paused_id, Some(own.clone()))
        .unwrap();

    let report = state.daemon_report();
    assert_eq!(report.owner, None);
    let claude = report
        .accounts
        .iter()
        .find(|a| a.id.as_str() == "claude-1")
        .unwrap();
    assert!(!claude.paused);
    assert_eq!(claude.reserve, state.settings.reserve);
    let codex = report.accounts.iter().find(|a| a.id == paused_id).unwrap();
    assert!(codex.paused);
    assert_eq!(codex.reserve, own);

    state.set_owner(Some("alice@example.com".to_owned()));
    assert_eq!(
        state.daemon_report().owner.as_deref(),
        Some("alice@example.com")
    );
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[tokio::test]
async fn tunnel_status_follows_up_and_down_and_asks_for_a_report() {
    let state = test_state("tunnel-status");
    let status = state.admin_status(chrono::Utc::now());
    assert!(status.tunnel_connected_since.is_none() && status.tunnel_error.is_none());

    state.record_tunnel_up();
    let status = state.admin_status(chrono::Utc::now());
    assert!(status.tunnel_connected_since.is_some() && status.tunnel_error.is_none());
    tokio::time::timeout(std::time::Duration::from_secs(1), state.wait_changed())
        .await
        .expect("coming up asks for a report");

    state.record_tunnel_down("the server closed the tunnel".to_owned());
    let status = state.admin_status(chrono::Utc::now());
    assert!(status.tunnel_connected_since.is_none());
    assert_eq!(
        status.tunnel_error.as_deref(),
        Some("the server closed the tunnel")
    );
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[test]
fn effective_models_prefer_account_then_config_default() {
    let state = test_state_with_models("models", "[models]\nclaude = [\"claude-sonnet-*\"]");
    let claude_id: AccountId = "claude-1".parse().unwrap();
    let codex_id: AccountId = "codex-1".parse().unwrap();
    let claude = state.insert_account(stored("claude-1", Provider::Claude));
    let codex = state.insert_account(stored("codex-1", Provider::Codex));

    assert_eq!(
        state.effective_models(&claude),
        Some(models(&["claude-sonnet-*"]))
    );
    assert_eq!(state.effective_models(&codex), None);
    assert!(state.model_allowed(&claude, "Claude-Sonnet-5-5"));
    assert!(!state.model_allowed(&claude, "claude-opus-5-5"));
    assert!(state.model_allowed(&codex, "anything"));

    let own = models(&["claude-opus-*"]);
    let report = state
        .set_account_models(&claude_id, Some(own.clone()))
        .unwrap();
    assert_eq!(report.models, Some(own.clone()));
    assert!(state.model_allowed(&claude, "claude-opus-5-5"));
    assert!(!state.model_allowed(&claude, "claude-sonnet-5-5"));
    state
        .set_account_models(&codex_id, Some(models(&["gpt-*"])))
        .unwrap();
    assert!(!state.model_allowed(&codex, "o3"));

    let status = state.admin_status(Utc::now());
    assert_eq!(
        status.default_models,
        vec![(Provider::Claude, models(&["claude-sonnet-*"]))]
    );
    let view = |id: &AccountId| {
        status
            .accounts
            .iter()
            .find(|account| &account.report.id == id)
            .unwrap()
    };
    assert_eq!(view(&claude_id).models, Some(own.clone()));
    assert_eq!(view(&claude_id).report.models, Some(own));
    assert_eq!(view(&codex_id).models, Some(models(&["gpt-*"])));

    let saved = RuntimeSettings::load(&state.settings.runtime_path()).unwrap();
    assert_eq!(
        saved.accounts[&claude_id].models,
        Some(models(&["claude-opus-*"]))
    );

    let report = state.set_account_models(&claude_id, None).unwrap();
    assert_eq!(report.models, Some(models(&["claude-sonnet-*"])));
    let saved = RuntimeSettings::load(&state.settings.runtime_path()).unwrap();
    assert_eq!(saved.accounts[&claude_id].models, None);
    assert_eq!(state.admin_status(Utc::now()).accounts[0].models, None);

    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}

#[tokio::test]
async fn changing_models_asks_for_a_report_and_unknown_accounts_fail() {
    let state = test_state("models-notify");
    let id: AccountId = "codex-1".parse().unwrap();
    state.insert_account(stored("codex-1", Provider::Codex));
    state
        .set_account_models(&id, Some(models(&["gpt-*"])))
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), state.wait_changed())
        .await
        .expect("a model change asks for a report");
    assert_eq!(
        state.daemon_report().accounts[0].models,
        Some(models(&["gpt-*"]))
    );

    let unknown: AccountId = "nope".parse().unwrap();
    assert!(matches!(
        state.set_account_models(&unknown, None),
        Err(StateError::NotFound(_))
    ));
    let _ = std::fs::remove_dir_all(&state.settings.data_dir);
}
