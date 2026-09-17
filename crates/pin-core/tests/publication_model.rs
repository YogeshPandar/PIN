//! Bounded model of publication, retired-source liveness, and slot reuse.
//! This models protocol preconditions, not PostgreSQL locks, WAL, or MVCC.
#![forbid(unsafe_code)]

use std::collections::{HashSet, VecDeque};

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
struct State {
    fragments: u8,
    published: bool,
    retired: bool,
    reader: bool,
    vacuum_authorized: bool,
    old_live: bool,
    new_live: bool,
    merge_started: bool,
    merge_published: bool,
    reused: bool,
}

#[derive(Clone, Copy, Debug)]
enum Action {
    WriteFirst,
    WriteSecond,
    Publish,
    ReaderEnter,
    ReaderExit,
    StartMerge,
    AuthorizeVacuum,
    RemoveOld,
    RemoveNew,
    PublishMerge,
    Reuse,
}

const ACTIONS: [Action; 11] = [
    Action::WriteFirst,
    Action::WriteSecond,
    Action::Publish,
    Action::ReaderEnter,
    Action::ReaderExit,
    Action::StartMerge,
    Action::AuthorizeVacuum,
    Action::RemoveOld,
    Action::RemoveNew,
    Action::PublishMerge,
    Action::Reuse,
];

fn step(mut s: State, action: Action, reconcile: bool, remove_retired: bool) -> Option<State> {
    match action {
        Action::WriteFirst if !s.published => s.fragments |= 1,
        Action::WriteSecond if !s.published => s.fragments |= 2,
        Action::Publish if s.fragments == 3 && !s.published => {
            s.published = true;
            s.old_live = true;
        }
        Action::ReaderEnter if s.published && !s.retired && !s.reused => s.reader = true,
        Action::ReaderExit if s.reader => s.reader = false,
        Action::StartMerge if s.published && !s.merge_started => {
            s.merge_started = true;
            s.new_live = s.old_live;
        }
        Action::AuthorizeVacuum if s.published => s.vacuum_authorized = true,
        Action::RemoveOld if s.vacuum_authorized => s.old_live = false,
        Action::RemoveNew if s.vacuum_authorized && s.merge_published => s.new_live = false,
        Action::PublishMerge if s.merge_started && !s.merge_published => {
            if reconcile {
                s.new_live &= s.old_live;
            }
            s.merge_published = true;
            s.retired = true;
        }
        Action::Reuse if s.vacuum_authorized && !s.reused => {
            let current_live = if s.merge_published { s.new_live } else { s.old_live };
            let reachable_old_live = remove_retired && s.reader && s.old_live;
            if current_live || reachable_old_live {
                return None;
            }
            s.reused = true;
        }
        _ => return None,
    }
    Some(s)
}

fn invariant(s: State) -> bool {
    let current_live = if s.merge_published { s.new_live } else { s.old_live };
    (!s.published || s.fragments == 3)
        && (!s.reused || (!current_live && !(s.reader && s.old_live)))
}

fn explore(reconcile: bool, remove_retired: bool) -> (usize, Option<Vec<Action>>) {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([(State::default(), Vec::new())]);
    while let Some((state, path)) = queue.pop_front() {
        if !seen.insert(state) {
            continue;
        }
        if !invariant(state) {
            return (seen.len(), Some(path));
        }
        for action in ACTIONS {
            if let Some(next) = step(state, action, reconcile, remove_retired) {
                let mut next_path = path.clone();
                next_path.push(action);
                queue.push_back((next, next_path));
            }
        }
    }
    (seen.len(), None)
}

#[test]
fn publication_requires_both_fragments() {
    for fragments in 0..3 {
        let state = State { fragments, ..State::default() };
        assert!(step(state, Action::Publish, true, true).is_none());
    }
}

#[test]
fn checked_model_explores_every_reachable_state() {
    let (states, counterexample) = explore(true, true);
    assert!(states > 50, "model stopped exploring: {states}");
    assert!(counterexample.is_none(), "protocol violation: {counterexample:?}");
}

#[test]
fn stale_merge_copy_has_a_counterexample() {
    let (_, counterexample) = explore(false, true);
    assert!(counterexample.is_some(), "model must detect lost deletion reconciliation");
}

#[test]
fn active_only_vacuum_has_a_counterexample() {
    let (_, counterexample) = explore(true, false);
    assert!(counterexample.is_some(), "model must detect stale retired-reader liveness");
}
