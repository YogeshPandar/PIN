"""Independent, packed-state reachability oracle for the G0 protocol model."""
from collections import deque
import json

# two fragment bits followed by nine protocol flags.
PUBLISHED, RETIRED, READER, VACUUM, OLD, NEW, STARTED, MERGED, REUSED = (
    1 << bit for bit in range(2, 11)
)
ACTIONS = (
    "write_first", "write_second", "publish", "reader_enter", "reader_exit",
    "start_merge", "authorize_vacuum", "remove_old", "remove_new",
    "publish_merge", "reuse",
)


def successor(state, action, reconcile=True, remove_retired=True):
    """Return a changed successor, or None when disabled or idempotent."""
    nxt = state
    if action == 0 and not state & PUBLISHED:
        nxt |= 1
    elif action == 1 and not state & PUBLISHED:
        nxt |= 2
    elif action == 2 and state & 3 == 3 and not state & PUBLISHED:
        nxt |= PUBLISHED | OLD
    elif action == 3 and state & PUBLISHED and not state & (RETIRED | REUSED):
        nxt |= READER
    elif action == 4 and state & READER:
        nxt &= ~READER
    elif action == 5 and state & PUBLISHED and not state & STARTED:
        nxt = (nxt | STARTED | NEW) if state & OLD else (nxt | STARTED) & ~NEW
    elif action == 6 and state & PUBLISHED:
        nxt |= VACUUM
    elif action == 7 and state & VACUUM:
        nxt &= ~OLD
    elif action == 8 and state & VACUUM and state & MERGED:
        nxt &= ~NEW
    elif action == 9 and state & STARTED and not state & MERGED:
        nxt |= MERGED | RETIRED
        if reconcile and not state & OLD:
            nxt &= ~NEW
    elif action == 10 and state & VACUUM and not state & REUSED:
        current = bool(state & (NEW if state & MERGED else OLD))
        retained = remove_retired and state & READER and state & OLD
        if not current and not retained:
            nxt |= REUSED
    return nxt if nxt != state else None


def invariant(state):
    complete = not state & PUBLISHED or state & 3 == 3
    current = bool(state & (NEW if state & MERGED else OLD))
    retained = bool(state & READER and state & OLD)
    return complete and not (state & REUSED and (current or retained))


def explore(reconcile=True, remove_retired=True):
    parents = {0: None}
    queue = deque([0])
    edges = [0] * len(ACTIONS)
    while queue:
        state = queue.popleft()
        if not invariant(state):
            trace = []
            while parents[state] is not None:
                state, action = parents[state]
                trace.append(ACTIONS[action])
            return parents, edges, list(reversed(trace))
        for action in range(len(ACTIONS)):
            nxt = successor(state, action, reconcile, remove_retired)
            if nxt is None:
                continue
            edges[action] += 1
            if nxt not in parents:
                parents[nxt] = state, action
                queue.append(nxt)
    return parents, edges, None


def fixed_point():
    """Scan the entire 11-bit domain instead of traversing a queue."""
    reached = {0}
    while True:
        previous = reached.copy()
        for state in range(1 << 11):
            if state not in previous:
                continue
            reached.update(
                nxt for action in range(len(ACTIONS))
                if (nxt := successor(state, action)) is not None
            )
        if previous == reached:
            return reached


def report():
    parents, edges, failure = explore()
    reference = fixed_point()
    assert failure is None
    assert set(parents) == reference
    assert all(invariant(state) for state in reference)
    assert all(edges)
    stale = explore(False, True)[2]
    retired = explore(True, False)[2]
    assert stale is not None and retired is not None
    return {
        "states": len(parents), "non_stuttering_edges": sum(edges),
        "edges_by_action": dict(zip(ACTIONS, edges)),
        "fixed_point_matches": True,
        "lost_reconciliation_counterexample": stale,
        "missed_retired_source_counterexample": retired,
        "rust_executed": False,
    }


if __name__ == "__main__":
    print(json.dumps(report(), indent=2))
