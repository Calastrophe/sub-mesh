//! Shared dashboard state: the latest overview, UI settings, toasts and the
//! actions that change a subscription.

use std::collections::HashSet;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use mesh_core::api::{
    DEFAULT_USAGE_HOURS, Direction, LogEntry, Overview, Session, SettingsView, Subscription,
    UsageReport,
};
use mesh_core::{AccountId, ModelAllowlist, Provider, ReserveConfig};

use crate::api;
use crate::format::label_text;
use crate::logs;
use crate::summary::{Filter, reports_five_hour};

const TOAST_LIFETIME: Duration = Duration::from_secs(6);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub id: u32,
    pub kind: ToastKind,
    pub text: String,
}

/// A change the owner can make to a subscription.
pub enum Action {
    SetPaused(bool),
    /// Set the reserve; `None` goes back to the daemon default.
    SetReserve(Option<ReserveConfig>),
    /// Set the model allowlist; `None` goes back to the daemon default.
    SetModels(Option<ModelAllowlist>),
    Remove,
}

/// All shared signals. It is `Copy`, so closures capture it freely.
#[derive(Clone, Copy)]
pub struct Store {
    pub overview: RwSignal<Option<Overview>>,
    /// True while an overview request is in flight.
    pub loading: RwSignal<bool>,
    /// True when the last overview request failed.
    overview_error: RwSignal<bool>,
    /// The caller's log entries loaded so far, newest first.
    pub logs: RwSignal<Option<Vec<LogEntry>>>,
    /// True while the newest log page is in flight.
    pub logs_loading: RwSignal<bool>,
    /// True while an older log page is in flight.
    pub logs_paging: RwSignal<bool>,
    pub logs_error: RwSignal<bool>,
    /// True when the server may hold entries older than those loaded.
    pub logs_more: RwSignal<bool>,
    pub log_filter: RwSignal<Filter>,
    /// Which direction the log shows; `None` shows both. Read it with
    /// [`Store::log_direction`]; change it with [`Store::set_log_direction`].
    log_direction: RwSignal<Option<Direction>>,
    log_generation: RwSignal<u64>,
    pub errors_only: RwSignal<bool>,
    /// What the mesh used of the caller's subscriptions in the period.
    pub usage: RwSignal<Option<UsageReport>>,
    /// True while a usage request is in flight.
    pub usage_loading: RwSignal<bool>,
    pub usage_error: RwSignal<bool>,
    pub usage_filter: RwSignal<Filter>,
    /// The period the usage covers, in hours. Read it with
    /// [`Store::usage_hours`]; change it with [`Store::set_usage_hours`].
    usage_hours: RwSignal<u32>,
    usage_generation: RwSignal<u64>,
    overview_invalidated: RwSignal<bool>,
    applying: RwSignal<HashSet<AccountId>>,
    pub show_emails: RwSignal<bool>,
    pub filter: RwSignal<Filter>,
    pub toasts: RwSignal<Vec<Toast>>,
    next_toast: RwSignal<u32>,
    pub session: RwSignal<Option<Session>>,
    pub session_error: RwSignal<bool>,
    pub session_denied: RwSignal<bool>,
    session_loading: RwSignal<bool>,
    session_generation: RwSignal<u64>,
    pub settings: RwSignal<Option<SettingsView>>,
    settings_loading: RwSignal<bool>,
    settings_error: RwSignal<bool>,
    settings_generation: RwSignal<u64>,
    pub settings_busy: RwSignal<bool>,
}

/// The store provided by [`crate::app::App`].
pub fn use_store() -> Store {
    expect_context()
}

impl Store {
    pub fn new() -> Self {
        Self {
            overview: RwSignal::new(None),
            loading: RwSignal::new(false),
            overview_error: RwSignal::new(false),
            logs: RwSignal::new(None),
            logs_loading: RwSignal::new(false),
            logs_paging: RwSignal::new(false),
            logs_error: RwSignal::new(false),
            logs_more: RwSignal::new(false),
            log_filter: RwSignal::new(Filter::All),
            log_direction: RwSignal::new(None),
            log_generation: RwSignal::new(0),
            errors_only: RwSignal::new(false),
            usage: RwSignal::new(None),
            usage_loading: RwSignal::new(false),
            usage_error: RwSignal::new(false),
            usage_filter: RwSignal::new(Filter::All),
            usage_hours: RwSignal::new(DEFAULT_USAGE_HOURS),
            usage_generation: RwSignal::new(0),
            overview_invalidated: RwSignal::new(false),
            applying: RwSignal::new(HashSet::new()),
            show_emails: RwSignal::new(false),
            filter: RwSignal::new(Filter::All),
            toasts: RwSignal::new(Vec::new()),
            next_toast: RwSignal::new(0),
            session: RwSignal::new(None),
            session_error: RwSignal::new(false),
            session_denied: RwSignal::new(false),
            session_loading: RwSignal::new(false),
            session_generation: RwSignal::new(0),
            settings: RwSignal::new(None),
            settings_loading: RwSignal::new(false),
            settings_error: RwSignal::new(false),
            settings_generation: RwSignal::new(0),
            settings_busy: RwSignal::new(false),
        }
    }

    pub fn is_admin(self) -> bool {
        self.session
            .with(|session| session.as_ref().is_some_and(|session| session.admin))
    }

    pub fn pending_requests(self) -> u32 {
        self.session.with(|session| {
            session
                .as_ref()
                .and_then(|session| session.pending_requests)
                .unwrap_or(0)
        })
    }

    pub fn refresh_session(self) {
        if self.session_loading.get_untracked() {
            return;
        }
        self.session_loading.set(true);
        let generation = self.session_generation.get_untracked();
        spawn_local(async move {
            let result = api::session().await;
            if self.session_generation.try_get_untracked() != Some(generation) {
                return;
            }
            self.session_loading.set(false);
            match result {
                Ok(session) => {
                    if self
                        .session
                        .with_untracked(|known| known.as_ref() != Some(&session))
                    {
                        self.session.set(Some(session));
                    }
                    self.session_error.set(false);
                    self.session_denied.set(false);
                }
                Err(error) => {
                    self.session_error.set(true);
                    self.session_denied.set(matches!(
                        error,
                        api::Error::Api {
                            status: 401 | 403,
                            ..
                        }
                    ));
                    if self.session.with_untracked(Option::is_some) {
                        self.toast(ToastKind::Error, error.to_string());
                    }
                }
            }
        });
    }

    fn restart_session(self) {
        self.session_generation
            .update(|generation| *generation += 1);
        self.session_loading.set(false);
        self.refresh_session();
    }

    pub fn set_session(self, session: Session) {
        self.session_generation
            .update(|generation| *generation += 1);
        self.session_loading.set(false);
        self.session_error.set(false);
        self.session_denied.set(false);
        self.session.set(Some(session));
    }

    pub fn settings_failed(self) -> bool {
        self.settings_error.get() && !self.settings_loading.get()
    }

    pub fn refresh_settings(self) {
        if self.settings_loading.get_untracked()
            || self.settings_busy.get_untracked()
            || !self.is_admin_untracked()
        {
            return;
        }
        self.settings_loading.set(true);
        let generation = self.settings_generation.get_untracked();
        spawn_local(async move {
            let result = api::settings().await;
            if self.settings_generation.try_get_untracked() != Some(generation) {
                return;
            }
            self.settings_loading.set(false);
            match result {
                Ok(view) => {
                    if self
                        .settings
                        .with_untracked(|known| known.as_ref() != Some(&view))
                    {
                        self.settings.set(Some(view));
                    }
                    self.settings_error.set(false);
                }
                Err(error) => {
                    self.settings_error.set(true);
                    self.toast(ToastKind::Error, error.to_string());
                }
            }
        });
    }

    fn is_admin_untracked(self) -> bool {
        self.session
            .with_untracked(|session| session.as_ref().is_some_and(|session| session.admin))
    }

    pub fn change_settings(
        self,
        message: String,
        call: impl std::future::Future<Output = api::Result<SettingsView>> + 'static,
        done: impl FnOnce(bool) + 'static,
    ) {
        if self.settings_busy.get_untracked() {
            return;
        }
        self.settings_busy.set(true);
        self.settings_generation
            .update(|generation| *generation += 1);
        self.settings_loading.set(false);
        spawn_local(async move {
            let result = call.await;
            if self.settings_busy.try_set(false).is_some() {
                return;
            }
            match result {
                Ok(view) => {
                    self.settings.set(Some(view));
                    self.settings_error.set(false);
                    self.toast(ToastKind::Info, message);
                    self.restart_session();
                    done(true);
                }
                Err(error) => {
                    self.toast(ToastKind::Error, error.to_string());
                    done(false);
                }
            }
        });
    }

    /// Server time of the latest overview.
    pub fn now(self) -> Option<chrono::DateTime<chrono::Utc>> {
        self.overview.with(|o| o.as_ref().map(|o| o.now))
    }

    pub fn with_subscription<T>(
        self,
        id: &AccountId,
        read: impl FnOnce(&Subscription) -> T,
    ) -> Option<T> {
        self.overview.with(|overview| {
            overview
                .as_ref()?
                .subscriptions
                .iter()
                .find(|sub| &sub.id == id)
                .map(read)
        })
    }

    /// Whether any subscription of `provider` reports a 5-hour window.
    pub fn shows_five_hour(self, provider: Provider) -> bool {
        self.overview.with(|overview| {
            overview
                .as_ref()
                .is_some_and(|o| reports_five_hour(&o.subscriptions, provider))
        })
    }

    /// The highest balancer score among subscriptions of `provider`.
    pub fn best_score(self, provider: Provider) -> Option<f64> {
        self.overview.with(|overview| {
            overview
                .as_ref()?
                .subscriptions
                .iter()
                .filter(|sub| sub.provider == provider)
                .filter_map(|sub| sub.score)
                .reduce(f64::max)
        })
    }

    pub fn overview_failed(self) -> bool {
        self.overview_error.get() && !self.loading.get()
    }

    pub fn overview_status(self) -> &'static str {
        if self.overview_failed() {
            "Could not load the overview."
        } else {
            "Loading…"
        }
    }

    /// Keep the last good snapshot and allow only one overview request at a time.
    pub fn refresh(self) {
        if self.loading.get_untracked() {
            return;
        }
        self.loading.set(true);
        spawn_local(async move {
            loop {
                let result = api::overview().await;
                let Some(invalidated) = self.overview_invalidated.try_update(std::mem::take) else {
                    return;
                };
                if invalidated {
                    continue;
                }
                match result {
                    Ok(overview) => {
                        self.overview.set(Some(overview));
                        self.overview_error.set(false);
                    }
                    Err(error) => {
                        self.overview_error.set(true);
                        self.toast(ToastKind::Error, error.to_string());
                    }
                }
                break;
            }
            self.loading.set(false);
        });
    }

    /// The direction the log is filtered to; `None` shows both.
    pub fn log_direction(self) -> Option<Direction> {
        self.log_direction.get()
    }

    /// Show only `direction`, or both for `None`. The entries loaded are
    /// dropped and the newest page is fetched again.
    pub fn set_log_direction(self, direction: Option<Direction>) {
        if self.log_direction.get_untracked() == direction {
            return;
        }
        self.log_generation.update(|generation| *generation += 1);
        self.log_direction.set(direction);
        self.logs_loading.set(false);
        self.logs_paging.set(false);
        self.logs.set(None);
        self.logs_more.set(false);
        self.logs_error.set(false);
        self.refresh_logs();
    }

    /// Fetch the newest log page and fold it into the entries loaded, so
    /// pages already loaded below it stay.
    pub fn refresh_logs(self) {
        if self.logs_loading.get_untracked() {
            return;
        }
        self.logs_loading.set(true);
        let direction = self.log_direction.get_untracked();
        let generation = self.log_generation.get_untracked();
        spawn_local(async move {
            let result = api::logs(logs::PAGE, None, direction).await;
            if self.log_generation.try_get_untracked() != Some(generation) {
                return;
            }
            match result {
                Ok(page) => {
                    let page_len = page.len();
                    let merged = self.logs.with_untracked(|loaded| match loaded {
                        Some(loaded) => logs::merge_newest(page, loaded),
                        None => page,
                    });
                    if merged.len() == page_len {
                        self.logs_more.set(page_len >= logs::PAGE as usize);
                    }
                    self.logs.set(Some(merged));
                    self.logs_error.set(false);
                }
                Err(error) => {
                    self.logs_error.set(true);
                    self.toast(ToastKind::Error, error.to_string());
                }
            }
            self.logs_loading.set(false);
        });
    }

    /// Fetch the page of log entries below the oldest one loaded.
    pub fn load_older_logs(self) {
        if self.logs_paging.get_untracked() {
            return;
        }
        let Some(before) = self.oldest_log_id() else {
            return;
        };
        self.logs_paging.set(true);
        let direction = self.log_direction.get_untracked();
        let generation = self.log_generation.get_untracked();
        spawn_local(async move {
            let result = api::logs(logs::PAGE, Some(before), direction).await;
            if self.log_generation.try_get_untracked() != Some(generation) {
                return;
            }
            self.logs_paging.set(false);
            if !self.logs_more.get_untracked() || self.oldest_log_id() != Some(before) {
                return;
            }
            match result {
                Ok(page) => {
                    self.logs_more.set(page.len() >= logs::PAGE as usize);
                    self.logs.update(|loaded| {
                        if let Some(loaded) = loaded {
                            logs::append_older(loaded, page);
                        }
                    });
                }
                Err(error) => self.toast(ToastKind::Error, error.to_string()),
            }
        });
    }

    fn oldest_log_id(self) -> Option<i64> {
        self.logs
            .with_untracked(|loaded| loaded.as_ref()?.last().map(|entry| entry.id))
    }

    /// The period the usage covers, in hours.
    pub fn usage_hours(self) -> u32 {
        self.usage_hours.get()
    }

    /// Cover `hours` instead. The usage loaded is dropped and fetched again.
    pub fn set_usage_hours(self, hours: u32) {
        if self.usage_hours.get_untracked() == hours {
            return;
        }
        self.usage_generation.update(|generation| *generation += 1);
        self.usage_hours.set(hours);
        self.usage_loading.set(false);
        self.usage.set(None);
        self.usage_error.set(false);
        self.refresh_usage();
    }

    /// Fetch the usage of the current period, keeping the last good report
    /// on failure.
    pub fn refresh_usage(self) {
        if self.usage_loading.get_untracked() {
            return;
        }
        self.usage_loading.set(true);
        let hours = self.usage_hours.get_untracked();
        let generation = self.usage_generation.get_untracked();
        spawn_local(async move {
            let result = api::usage(hours).await;
            if self.usage_generation.try_get_untracked() != Some(generation) {
                return;
            }
            match result {
                Ok(report) => {
                    self.usage.set(Some(report));
                    self.usage_error.set(false);
                }
                Err(error) => {
                    self.usage_error.set(true);
                    self.toast(ToastKind::Error, error.to_string());
                }
            }
            self.usage_loading.set(false);
        });
    }

    /// Show a toast. An identical toast already on screen is not repeated.
    pub fn toast(self, kind: ToastKind, text: String) {
        if self
            .toasts
            .with_untracked(|toasts| toasts.iter().any(|t| t.kind == kind && t.text == text))
        {
            return;
        }
        let id = self.next_toast.get_untracked();
        self.next_toast.set(id + 1);
        self.toasts
            .update(|toasts| toasts.push(Toast { id, kind, text }));
        set_timeout(move || self.dismiss(id), TOAST_LIFETIME);
    }

    pub fn dismiss(self, id: u32) {
        let _ = self
            .toasts
            .try_update(|toasts| toasts.retain(|t| t.id != id));
    }

    pub fn is_applying(self, id: &AccountId) -> bool {
        self.applying.with(|ids| ids.contains(id))
    }

    /// Run `action` on a subscription. `done` gets whether it succeeded.
    /// Failures become error toasts.
    pub fn apply(self, id: AccountId, action: Action, done: impl FnOnce(bool) + 'static) {
        if self.applying.try_update(|ids| ids.insert(id.clone())) != Some(true) {
            return;
        }
        let name = self
            .with_subscription(&id, |sub| label_text(sub, false))
            .unwrap_or_else(|| id.to_string());
        spawn_local(async move {
            let (message, result) = match action {
                Action::SetPaused(paused) => (
                    if paused {
                        format!("Paused {name}")
                    } else {
                        format!("Resumed {name}")
                    },
                    api::set_paused(&id, paused).await.map(Some),
                ),
                Action::SetReserve(reserve) => (
                    match reserve {
                        Some(_) => format!("Updated reserve of {name}"),
                        None => format!("Reserve of {name} reset to default"),
                    },
                    api::set_reserve(&id, reserve).await.map(Some),
                ),
                Action::SetModels(models) => (
                    match models {
                        Some(_) => format!("Updated model allowlist of {name}"),
                        None => format!("Model allowlist of {name} reset to default"),
                    },
                    api::set_models(&id, models).await.map(Some),
                ),
                Action::Remove => (
                    format!("Removed {name}"),
                    api::remove(&id).await.map(|()| None),
                ),
            };
            let Some(loading) = self.loading.try_get_untracked() else {
                return;
            };
            match result {
                Ok(outcome) => {
                    self.merge(&id, outcome);
                    self.toast(ToastKind::Info, message);
                    if loading {
                        self.overview_invalidated.set(true);
                    } else {
                        self.refresh();
                    }
                    done(true);
                }
                Err(error) => {
                    self.toast(ToastKind::Error, error.to_string());
                    done(false);
                }
            }
            let _ = self.applying.try_update(|ids| ids.remove(&id));
        });
    }

    /// Fold a server answer into the overview without waiting for a refetch.
    fn merge(self, id: &AccountId, updated: Option<Subscription>) {
        self.overview.update(|overview| {
            let Some(overview) = overview else { return };
            match updated {
                Some(updated) => {
                    if let Some(sub) = overview.subscriptions.iter_mut().find(|s| &s.id == id) {
                        *sub = updated;
                    }
                }
                None => overview.subscriptions.retain(|s| &s.id != id),
            }
        });
    }
}
