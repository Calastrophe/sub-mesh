use mesh_core::{AccountId, Score};

use super::*;

fn scored(id: &str, value: f64) -> Candidate {
    Candidate {
        id: id.parse::<AccountId>().unwrap(),
        node: "n1".to_owned(),
        owner: None,
        generation: 1,
        headroom: 0.4,
        score: Score {
            value,
            min_room: 0.4,
        },
    }
}

fn ids(list: &[Candidate]) -> Vec<&str> {
    list.iter().map(|c| c.id.as_str()).collect()
}

#[test]
fn orders_by_score_best_first() {
    let list = order(
        vec![scored("late", 0.1), scored("soon", 0.8), scored("mid", 0.2)],
        Warm::default(),
        0.02,
    );
    assert_eq!(ids(&list), ["soon", "mid", "late"]);
}

#[test]
fn ties_go_to_the_lower_id() {
    let forward = order(
        vec![scored("b", 0.2), scored("c", 0.2), scored("a", 0.2)],
        Warm::default(),
        0.02,
    );
    let backward = order(
        vec![scored("c", 0.2), scored("a", 0.2), scored("b", 0.2)],
        Warm::default(),
        0.02,
    );
    assert_eq!(ids(&forward), ["a", "b", "c"]);
    assert_eq!(ids(&backward), ["a", "b", "c"]);
}

#[test]
fn equal_scores_go_to_the_account_with_more_room() {
    let mut roomy = scored("z-roomy", 0.2);
    roomy.score.min_room = 0.3;
    let mut tight = scored("a-tight", 0.2);
    tight.score.min_room = 0.2;
    let list = order(vec![tight, roomy], Warm::default(), 0.02);
    assert_eq!(ids(&list), ["z-roomy", "a-tight"]);
}

fn session(id: &AccountId) -> Warm<'_> {
    Warm {
        session: Some(id),
        user: None,
    }
}

fn user(id: &AccountId) -> Warm<'_> {
    Warm {
        session: None,
        user: Some(id),
    }
}

#[test]
fn session_warmth_holds_at_the_threshold_and_breaks_below_it() {
    let make = |headroom| {
        let mut sticky = scored("sticky", 0.1);
        sticky.headroom = headroom;
        vec![scored("best", 0.8), sticky]
    };
    let sticky: AccountId = "sticky".parse().unwrap();
    assert_eq!(
        ids(&order(make(0.02), session(&sticky), 0.02)),
        ["sticky", "best"]
    );
    assert_eq!(
        ids(&order(make(0.0199), session(&sticky), 0.02)),
        ["best", "sticky"]
    );
    assert_eq!(
        ids(&order(make(0.001), session(&sticky), 0.0)),
        ["sticky", "best"]
    );
}

#[test]
fn a_warm_session_account_that_is_not_a_candidate_is_ignored() {
    let gone: AccountId = "gone".parse().unwrap();
    let list = order(
        vec![scored("b", 0.8), scored("a", 0.1)],
        session(&gone),
        0.02,
    );
    assert_eq!(ids(&list), ["b", "a"]);
}

#[test]
fn a_warm_user_account_does_not_beat_a_much_better_score() {
    let warm: AccountId = "warm".parse().unwrap();
    let list = order(
        vec![scored("best", 0.3), scored("warm", 0.1)],
        user(&warm),
        0.02,
    );
    assert_eq!(ids(&list), ["best", "warm"]);
}

#[test]
fn the_boost_is_exactly_the_documented_factor() {
    let warm: AccountId = "z-warm".parse().unwrap();
    let tied = || vec![scored("a-best", 0.4), scored("z-warm", 0.2)];
    assert_eq!(ids(&order(tied(), user(&warm), 0.02)), ["a-best", "z-warm"]);
    let mut roomy = tied();
    roomy[1].score.min_room = 0.5;
    assert_eq!(ids(&order(roomy, user(&warm), 0.02)), ["z-warm", "a-best"]);
}

#[test]
fn a_warm_user_account_leads_the_failover_order() {
    let warm: AccountId = "warm".parse().unwrap();
    let list = order(
        vec![scored("a", 0.30), scored("b", 0.25), scored("warm", 0.2)],
        user(&warm),
        0.02,
    );
    assert_eq!(ids(&list), ["warm", "a", "b"]);
}

#[test]
fn a_warm_session_account_goes_before_a_warm_user_account() {
    let sticky: AccountId = "sticky".parse().unwrap();
    let warm: AccountId = "warm".parse().unwrap();
    let list = order(
        vec![
            scored("best", 0.3),
            scored("warm", 0.2),
            scored("sticky", 0.1),
        ],
        Warm {
            session: Some(&sticky),
            user: Some(&warm),
        },
        0.02,
    );
    assert_eq!(ids(&list), ["sticky", "warm", "best"]);
}
