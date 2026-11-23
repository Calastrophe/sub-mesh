//! Order eligible accounts for a new request. The best score goes first
//! (see [`mesh_core::Score`]). A warm prompt cache wins over the score in
//! two ways: the session's own account goes first, and the caller's user
//! account gets a boost.

use mesh_core::AccountId;

use super::Candidate;

/// Score multiplier for the account with the caller's warm shared prefix.
pub const USER_WARM_BOOST: f64 = 2.0;

/// The accounts that hold a warm prompt cache for a request.
#[derive(Clone, Copy, Debug, Default)]
pub struct Warm<'a> {
    /// The session's account. It holds the whole conversation.
    pub session: Option<&'a AccountId>,
    /// The user's account. It holds the prefix that the user's sessions
    /// share.
    pub user: Option<&'a AccountId>,
}

/// Put the warm session account, if it keeps at least `min_headroom`,
/// first. The rest go by score, with the warm user account boosted by
/// [`USER_WARM_BOOST`], then by tightest-window room, then by account id.
#[must_use]
pub fn order(mut candidates: Vec<Candidate>, warm: Warm<'_>, min_headroom: f64) -> Vec<Candidate> {
    let sticky = |c: &Candidate| warm.session == Some(&c.id) && c.headroom >= min_headroom;
    candidates.sort_unstable_by(|a, b| {
        sticky(b)
            .cmp(&sticky(a))
            .then_with(|| boosted(b, warm.user).total_cmp(&boosted(a, warm.user)))
            .then_with(|| b.score.min_room.total_cmp(&a.score.min_room))
            .then_with(|| a.id.cmp(&b.id))
    });
    candidates
}

/// The score that `candidate` is ordered by.
fn boosted(candidate: &Candidate, user: Option<&AccountId>) -> f64 {
    let boost = if user == Some(&candidate.id) {
        USER_WARM_BOOST
    } else {
        1.0
    };
    candidate.score.value * boost
}

#[cfg(test)]
mod tests;
