"""Explore generation/VM count schedules, with a supplied PostgreSQL visibility oracle.

This is a finite protocol model, not an MVCC implementation or native test. The
reclamation horizon and correct ordinary bitmap fallback are explicit assumptions.
"""
from __future__ import annotations

import argparse
from collections import deque
from dataclasses import asdict, dataclass, replace
import json
from pathlib import Path


@dataclass(frozen=True)
class Case:
    name: str
    visible: bool = False
    published: bool = True
    live: bool = True
    vm: bool = False
    cleanup: bool = True


@dataclass(frozen=True)
class State:
    reader: int = 0
    writer: int = 0
    shared: bool = False
    exclusive: bool = False
    live: bool = True
    heap_present: bool = True
    vm: bool = False
    copied: bool | None = None
    observed_vm: bool | None = None
    private_count: int | None = None
    result: int | None = None
    fallback: bool = False
    cancelled: bool = False
    heap_changed: bool = False


CASES = (
    Case('deleted_before_snapshot'),
    Case('retained_old_snapshot', visible=True, vm=True),
    Case('live_dirty_page', visible=True, cleanup=False),
    Case('retired_before_acquisition', live=False, vm=True),
    Case('incomplete_publication', published=False, cleanup=False),
    Case('uncommitted_published_root', cleanup=False),
)
MODES = ('correct', 'ignore_guard', 'copy_before_guard', 'cached_vm', 'publish_partial')


def oracle(case: Case) -> int:
    return int(case.visible and case.published and case.live)


def transitions(case: Case, state: State, mode: str):
    actions = ('guard', 'copy', 'vm', 'count', 'release')
    if mode == 'copy_before_guard':
        actions = ('copy', 'guard', 'vm', 'count', 'release')
    if state.reader < len(actions):
        action = actions[state.reader]
        following = replace(state, reader=state.reader + 1)
        if action == 'guard':
            if state.exclusive:
                following = replace(following, reader=5, result=oracle(case), fallback=True)
            else:
                following = replace(following, shared=True)
        elif action == 'copy':
            following = replace(following, copied=state.live and case.published)
        elif action == 'vm':
            following = replace(following, observed_vm=True if mode == 'cached_vm' else state.vm)
        elif action == 'count':
            value = int(bool(state.copied and (state.observed_vm or
                                              (state.heap_present and case.visible))))
            following = replace(following, private_count=value,
                                result=value if mode == 'publish_partial' else None)
        else:
            following = replace(following, shared=False, result=state.private_count)
        yield f'reader:{action}', following
        yield 'reader:cancel', replace(state, reader=5, shared=False, private_count=None,
                                        result=None, cancelled=True)

    # an update/delete after the snapshot may clear VM without changing its visible row.
    if case.visible and not state.heap_changed:
        yield 'heap:hot_or_delete_clear_vm', replace(state, heap_changed=True, vm=False)
    if not case.cleanup:
        return
    horizon = not case.visible or state.reader == 5
    if state.writer == 0 and horizon and (not state.shared or mode == 'ignore_guard'):
        yield 'writer:exclusive', replace(state, writer=1, exclusive=True)
    elif state.writer == 1:
        yield 'vacuum:retire_membership', replace(state, writer=2, live=False)
    elif state.writer == 2:
        yield 'vacuum:remove_heap', replace(state, writer=3, heap_present=False)
    elif state.writer == 3:
        yield 'vacuum:all_visible_empty_page', replace(state, writer=4, vm=True)
    elif state.writer == 4:
        yield 'writer:release', replace(state, writer=5, exclusive=False)
    elif state.writer == 5:
        # the reused root belongs to a new incarnation and is too new for this snapshot.
        yield 'writer:reuse_with_vm_clear', replace(state, writer=6, vm=False)


def violation(case: Case, state: State):
    if state.result is not None and state.reader < 5:
        return 'partial count was published before completion'
    if state.cancelled and (state.shared or state.result is not None):
        return 'cancelled reader retained protection or a result'
    if state.reader == 5 and state.shared:
        return 'completed reader leaked protection'
    if state.result is not None and state.result != oracle(case):
        return f'count {state.result} differs from independent snapshot oracle {oracle(case)}'
    return None


def explore(case: Case, mode: str = 'correct') -> dict:
    if mode not in MODES:
        raise ValueError(mode)
    initial = State(live=case.live, heap_present=case.live, vm=case.vm)
    parents = {initial: None}
    pending = deque([initial])
    edges, fallback_states = 0, 0
    failure = None
    while pending:
        state = pending.popleft()
        fallback_states += state.fallback
        problem = violation(case, state)
        if problem and failure is None:
            trace = []
            cursor = state
            while parents[cursor] is not None:
                previous, action = parents[cursor]
                trace.append(action)
                cursor = previous
            failure = {'reason': problem, 'trace': list(reversed(trace)), 'state': asdict(state)}
        for action, following in transitions(case, state, mode):
            edges += 1
            if following not in parents:
                parents[following] = (state, action)
                pending.append(following)
    return {'case': case.name, 'mode': mode, 'states': len(parents), 'transitions': edges,
            'fallback_states': fallback_states, 'failure': failure}


def generation_oracle() -> dict:
    # a retired A posting and a new B posting at one CTID must never prove A AND B.
    owners = ((1, frozenset({'a'}), False), (2, frozenset({'b'}), True))
    exact = any(live and {'a', 'b'} <= terms for _, terms, live in owners)
    broken = {'a', 'b'} <= frozenset().union(*(terms for _, terms, _ in owners))
    if exact or not broken:
        raise AssertionError('generation negative control did not discriminate')
    return {'same_ctid_generations': [1, 2], 'exact_and': exact, 'broken_tid_union_and': broken}


def run() -> dict:
    correct = [explore(case) for case in CASES]
    if any(item['failure'] is not None for item in correct):
        raise AssertionError(correct)
    negative = [explore(CASES[0], mode) for mode in ('ignore_guard', 'copy_before_guard')]
    negative.extend((explore(CASES[-1], 'cached_vm'), explore(CASES[1], 'publish_partial')))
    if any(item['failure'] is None for item in negative):
        raise AssertionError('a broken protocol escaped its negative control')
    return {'correct': correct, 'negative_controls': negative, 'generation': generation_oracle()}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path)
    args = parser.parse_args()
    text = json.dumps(run(), indent=2) + '\n'
    if args.output:
        args.output.write_text(text)
    else:
        print(text, end='')


if __name__ == '__main__':
    main()
