use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::{Extension, Json};
use chrono::{DateTime, TimeDelta, Utc};
use mesh_core::api::{Me, Overview, Subscription, UserActivity};

use super::subscriptions::can_manage;
use crate::AppState;
use crate::db::Activity;
use crate::peer::Peer;
use crate::registry::{AccountView, Snapshot};

/// How far back request counts reach.
const ACTIVITY_WINDOW: TimeDelta = TimeDelta::hours(1);

/// `GET OVERVIEW_PATH`
pub async fn overview(
    State(state): State<Arc<AppState>>,
    Extension(peer): Extension<Peer>,
) -> Json<Overview> {
    let now = Utc::now();
    let snapshot = state.with_registry(|registry| registry.snapshot(now));
    let activity = activity(&state, now).await;
    Json(build(
        snapshot,
        &activity,
        peer,
        state.config.insecure_skip_whois,
        now,
    ))
}

/// Request counts for the last hour. A database error is logged and leaves
/// the counts empty, so the rest of the overview still shows.
pub async fn activity(state: &AppState, now: DateTime<Utc>) -> Activity {
    let since = now - ACTIVITY_WINDOW;
    match state.db.run(move |db| db.activity(since)).await {
        Ok(activity) => activity,
        Err(error) => {
            tracing::warn!(%error, "cannot count recent requests");
            Activity::default()
        }
    }
}

/// Assemble the overview as `peer` sees it.
fn build(
    snapshot: Snapshot,
    activity: &Activity,
    peer: Peer,
    insecure_skip_whois: bool,
    now: DateTime<Utc>,
) -> Overview {
    let users = users(&snapshot, activity);
    let subscriptions = snapshot
        .accounts
        .into_iter()
        .map(|view| {
            let manageable = can_manage(
                peer.login.as_deref(),
                view.owner.as_deref(),
                insecure_skip_whois,
            );
            subscription(view, activity, manageable)
        })
        .collect();
    Overview {
        now,
        me: Me {
            login: peer.login,
            node: peer.node,
        },
        subscriptions,
        daemons: snapshot.daemons,
        users,
    }
}

/// One account as the API returns it.
pub fn subscription(view: AccountView, activity: &Activity, can_manage: bool) -> Subscription {
    let report = view.report;
    let requests_last_hour = activity
        .by_account
        .get(report.id.as_str())
        .copied()
        .unwrap_or(0);
    Subscription {
        id: report.id,
        provider: report.provider,
        label: report.label,
        owner: view.owner,
        node: view.node,
        online: view.online,
        paused: report.paused,
        reserve: report.reserve,
        models: report.models,
        available_models: view.available_models,
        windows: report.windows,
        admission: view.admission,
        cooldown_until: view.cooldown_until,
        in_flight: view.in_flight,
        requests_last_hour,
        score: view.score,
        can_manage,
        updated_at: report.updated_at,
    }
}

/// Subscription owners, plus every login that made requests in the period.
/// Logins are the same user whatever their case. Busiest first, then by login.
fn users(snapshot: &Snapshot, activity: &Activity) -> Vec<UserActivity> {
    let mut users = BTreeMap::new();
    for view in &snapshot.accounts {
        if let Some(owner) = &view.owner {
            user_entry(&mut users, owner).subscriptions += 1;
        }
    }
    for count in &activity.by_login {
        user_entry(&mut users, &count.login).requests_last_hour += count.requests;
    }
    let mut list: Vec<UserActivity> = users.into_values().collect();
    list.sort_by_key(|user| Reverse(user.requests_last_hour));
    list
}

fn user_entry<'a>(
    users: &'a mut BTreeMap<String, UserActivity>,
    login: &str,
) -> &'a mut UserActivity {
    users
        .entry(login.to_ascii_lowercase())
        .or_insert_with(|| UserActivity {
            login: login.to_owned(),
            subscriptions: 0,
            requests_last_hour: 0,
        })
}
