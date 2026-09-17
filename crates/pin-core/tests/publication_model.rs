//! Bounded publication and slot-reuse model, not a PostgreSQL locking or WAL proof.
//! Source contracts and independently checked graph size: docs/api-evidence.md.
#![forbid(unsafe_code)]

use std::collections::{HashMap, hash_map::Entry};

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
            let current_live = if s.merge_published {
                s.new_live
            } else {
                s.old_live
            };
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
    let current_live = if s.merge_published {
        s.new_live
    } else {
        s.old_live
    };
    (!s.published || s.fragments == 3) && !(s.reused && (current_live || (s.reader && s.old_live)))
}

struct Node {
    state: State,
    predecessor: Option<(usize, Action)>,
}

struct Exploration {
    nodes: Vec<Node>,
    transitions: usize,
    actions_seen: [bool; ACTIONS.len()],
    violation: Option<usize>,
}

impl Exploration {
    fn counterexample(&self) -> Option<Vec<Action>> {
        let mut cursor = self.violation?;
        let mut path = Vec::new();
        while let Some((previous, action)) = self.nodes[cursor].predecessor {
            path.push(action);
            cursor = previous;
        }
        path.reverse();
        Some(path)
    }
}

fn explore(reconcile: bool, remove_retired: bool) -> Exploration {
    let initial = State::default();
    let mut seen = HashMap::from([(initial, 0)]);
    let mut result = Exploration {
        nodes: vec![Node {
            state: initial,
            predecessor: None,
        }],
        transitions: 0,
        actions_seen: [false; ACTIONS.len()],
        violation: None,
    };
    let mut cursor = 0;
    while cursor < result.nodes.len() {
        let state = result.nodes[cursor].state;
        if !invariant(state) {
            result.violation = Some(cursor);
            return result;
        }
        for (index, action) in ACTIONS.into_iter().enumerate() {
            let Some(next) = step(state, action, reconcile, remove_retired) else {
                continue;
            };
            if next == state {
                continue;
            }
            result.transitions += 1;
            result.actions_seen[index] = true;
            if let Entry::Vacant(entry) = seen.entry(next) {
                entry.insert(result.nodes.len());
                // retain one predecessor per state, not one copied path per edge.
                result.nodes.push(Node {
                    state: next,
                    predecessor: Some((cursor, action)),
                });
            }
        }
        cursor += 1;
    }
    result
}

#[test]
fn publication_requires_both_fragments() {
    for fragments in 0..3 {
        let state = State {
            fragments,
            ..State::default()
        };
        assert!(step(state, Action::Publish, true, true).is_none());
    }
}

#[test]
fn checked_model_explores_every_reachable_state() {
    let graph = explore(true, true);
    assert!(graph.violation.is_none(), "{:?}", graph.counterexample());
    assert_eq!(graph.nodes.len(), 37);
    assert_eq!(graph.transitions, 76);
    assert!(graph.actions_seen.into_iter().all(|seen| seen));
    assert!(graph.nodes.iter().any(|n| {
        n.state.reader && n.state.retired && n.state.reused && !n.state.old_live
    }));
}

fn assert_counterexample(reconcile: bool, remove_retired: bool) {
    let graph = explore(reconcile, remove_retired);
    let path = graph.counterexample().expect("negative control must fail");
    let mut state = State::default();
    for action in &path {
        assert!(invariant(state), "trace continued after a violation");
        state = step(state, *action, reconcile, remove_retired).expect("valid trace step");
    }
    assert!(!invariant(state));
    println!("counterexample: {path:?}");
}

#[test]
fn stale_merge_copy_has_a_counterexample() {
    assert_counterexample(false, true);
}

#[test]
fn active_only_vacuum_has_a_counterexample() {
    assert_counterexample(true, false);
}
