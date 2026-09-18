//! bounded model of publication, retired-source liveness, and slot reuse.
//! this models protocol preconditions, not postgresql locks, wal, or mvcc.
#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque, hash_map::Entry};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
    let previous = s;
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
    (s != previous).then_some(s)
}

fn invariant(s: State) -> bool {
    let current_live = if s.merge_published {
        s.new_live
    } else {
        s.old_live
    };
    (!s.published || s.fragments == 3) && !(s.reused && (current_live || (s.reader && s.old_live)))
}

struct Exploration {
    states: Vec<u16>,
    edges: [usize; ACTIONS.len()],
    counterexample: Option<Vec<Action>>,
}

fn state_key(s: State) -> u16 {
    let flags = [
        s.published,
        s.retired,
        s.reader,
        s.vacuum_authorized,
        s.old_live,
        s.new_live,
        s.merge_started,
        s.merge_published,
        s.reused,
    ];
    flags
        .iter()
        .enumerate()
        .fold(u16::from(s.fragments), |key, (bit, flag)| {
            key | (u16::from(*flag) << (bit + 2))
        })
}

fn explore(reconcile: bool, remove_retired: bool) -> Exploration {
    let mut parents = HashMap::from([(State::default(), None)]);
    let mut queue = VecDeque::from([State::default()]);
    let mut edges = [0; ACTIONS.len()];
    let mut counterexample = None;
    while let Some(state) = queue.pop_front() {
        if !invariant(state) {
            // reconstruct one trace instead of cloning a path for every edge.
            let mut path = Vec::new();
            let mut cursor = state;
            while let Some((previous, action)) = parents[&cursor] {
                path.push(action);
                cursor = previous;
            }
            path.reverse();
            counterexample = Some(path);
            break;
        }
        for (index, action) in ACTIONS.into_iter().enumerate() {
            if let Some(next) = step(state, action, reconcile, remove_retired) {
                edges[index] += 1;
                if let Entry::Vacant(entry) = parents.entry(next) {
                    entry.insert(Some((state, action)));
                    queue.push_back(next);
                }
            }
        }
    }
    let mut states: Vec<_> = parents.keys().copied().map(state_key).collect();
    states.sort_unstable();
    Exploration {
        states,
        edges,
        counterexample,
    }
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
    let result = explore(true, true);
    let expected: Vec<u16> = include_str!("fixtures/publication-states.txt")
        .split_whitespace()
        .map(|key| key.parse().unwrap())
        .collect();
    // the independent packed-state fixed-point oracle generates this fixture.
    assert_eq!(result.states, expected);
    assert_eq!(result.edges, [2, 2, 1, 7, 16, 8, 6, 9, 4, 12, 9]);
    assert!(
        result.counterexample.is_none(),
        "{:?}",
        result.counterexample
    );
    println!(
        "publication model: {} states, {} edges",
        result.states.len(),
        result.edges.iter().sum::<usize>()
    );
}

fn assert_counterexample(reconcile: bool, remove_retired: bool) {
    let trace = explore(reconcile, remove_retired).counterexample.unwrap();
    let mut broken = State::default();
    let mut corrected = Some(broken);
    for action in trace {
        broken = step(broken, action, reconcile, remove_retired).unwrap();
        corrected = corrected.and_then(|state| step(state, action, true, true));
    }
    assert!(!invariant(broken));
    assert!(corrected.is_none_or(invariant));
}

#[test]
fn stale_merge_copy_has_a_counterexample() {
    assert_counterexample(false, true);
}

#[test]
fn active_only_vacuum_has_a_counterexample() {
    assert_counterexample(true, false);
}

#[test]
fn idempotent_operations_do_not_count_as_coverage() {
    let state = State {
        fragments: 1,
        ..State::default()
    };
    assert!(step(state, Action::WriteFirst, true, true).is_none());
}
