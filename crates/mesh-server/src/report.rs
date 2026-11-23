//! The route where daemons report their accounts.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequest, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mesh_core::DaemonReport;

use crate::access::authorize;
use crate::peer::Role;
use crate::{AppState, ReportOutcome};

/// `POST REPORT_PATH`
pub async fn report(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request,
) -> Response {
    let peer = match authorize(&state, addr, request.headers(), Role::Daemon).await {
        Ok(peer) => peer,
        Err(response) => return *response,
    };
    let body = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        Bytes::from_request(request, &state),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(error)) => return error.into_response(),
        Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    let report: DaemonReport = match serde_json::from_slice(&body) {
        Ok(report) => report,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid report: {error}\n"),
            )
                .into_response();
        }
    };
    if let Err(message) = validate_report(&report) {
        return (
            StatusCode::BAD_REQUEST,
            format!("invalid report: {message}\n"),
        )
            .into_response();
    }
    let owner = resolve_owner(
        peer.login.as_deref(),
        report.owner.as_deref(),
        state.config.insecure_skip_whois,
    );
    tracing::debug!(node = %report.node, ?owner, accounts = report.accounts.len(), "report");
    match state.apply_report(report, owner).await {
        Ok(ReportOutcome::Applied) => StatusCode::NO_CONTENT.into_response(),
        Ok(ReportOutcome::NodeTaken) => (
            StatusCode::CONFLICT,
            "this node name belongs to a daemon of another login\n",
        )
            .into_response(),
        Err(error) => {
            tracing::warn!(%error, "cannot save report");
            (StatusCode::INTERNAL_SERVER_ERROR, "cannot save report\n").into_response()
        }
    }
}

/// The login that owns a daemon's accounts.
///
/// This is the tailnet login that whois found for the reporting peer. The
/// login that the daemon names in its own report is not trusted, and is
/// used only when `insecure_skip_whois` is on and there is no whois.
fn resolve_owner(
    peer_login: Option<&str>,
    reported: Option<&str>,
    insecure_skip_whois: bool,
) -> Option<String> {
    peer_login
        .or(reported.filter(|_| insecure_skip_whois))
        .map(str::trim)
        .filter(|login| !login.is_empty())
        .map(str::to_owned)
}

fn validate_report(report: &DaemonReport) -> Result<(), String> {
    if report.node.trim().is_empty() {
        return Err("empty node name".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    #[test]
    fn rejects_an_empty_node_name() {
        let mut report = DaemonReport {
            node: "n".to_owned(),
            owner: None,
            accounts: vec![],
            sent_at: Utc::now(),
        };
        assert!(validate_report(&report).is_ok());
        report.node = " ".to_owned();
        assert!(validate_report(&report).is_err());
    }

    #[test]
    fn owner_is_the_whois_login() {
        assert_eq!(
            resolve_owner(Some("a@example.com"), Some("b@example.com"), false).as_deref(),
            Some("a@example.com")
        );
        assert_eq!(resolve_owner(None, Some("b@example.com"), false), None);
    }

    #[test]
    fn report_owner_counts_only_without_whois() {
        assert_eq!(
            resolve_owner(None, Some(" b@example.com "), true).as_deref(),
            Some("b@example.com")
        );
        assert_eq!(resolve_owner(None, None, true), None);
        assert_eq!(resolve_owner(None, Some("  "), true), None);
        assert_eq!(
            resolve_owner(Some("a@example.com"), Some("b@example.com"), true).as_deref(),
            Some("a@example.com")
        );
    }
}
