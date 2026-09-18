"""Bounded schedules for the count/owner/VM proof, not an MVCC emulator.

PostgreSQL supplies snapshot visibility and the reclamation horizon as inputs.
The model explores each legal interleaving rather than using timing or threads.
Negative controls intentionally violate one implementation obligation at a time.
"""
from __future__ import annotations

import argparse
import json
from collections import deque
from dataclasses import asdict, dataclass, replace
from typing import Iterator


@dataclass(frozen=True)
class Case:
    name: str
    visible_to_snapshot: bool = False
    live: bool = True
    published: bool = True
    vm: bool = False
    sealed: bool = True
    cleanup: bool = True
    old_cached_vm: bool = True


@dataclass(frozen=True)
class State:
    reader: int = 0
    cleaner: int = 0
    pinned: bool = False
    live: bool = True
    heap_present: bool = True
    vm: bool = False
    copied_live: bool | None = None
    observed_vm: bool | None = None
    result: int | None = None
    cancelled: bool = False
    certified: bool = False


MODES = ("correct", "ignore_owner_pin", "copy_before_pin", "detached_liveness", "cached_vm")
CASES = (
    Case("deleted_before_snapshot"),
    Case("old_snapshot", visible_to_snapshot=True, vm=True),
    Case("removed_before_reader", live=False, vm=True),
    Case("failed_publication", published=False, cleanup=False),
    Case("uncommitted_sealed", cleanup=False),
    Case("mutable_source", visible_to_snapshot=True, vm=True, sealed=False, cleanup=False),
)


def transitions(case: Case, state: State, mode: str) -> Iterator[tuple[str, State]]:
    if state.reader < 5 and not state.cancelled:
        step = state.reader
        if mode == "copy_before_pin":
            actions = ("copy", "pin", "vm", "count", "release")
        else:
            actions = ("pin", "copy", "vm", "count", "release")
        action = actions[step]
        next_state = replace(state, reader=step + 1)
        if action == "pin":
            next_state = replace(next_state, pinned=True)
        elif action == "copy":
            live = case.live if mode == "detached_liveness" else state.live
            next_state = replace(next_state, copied_live=live and case.published)
        elif action == "vm":
            observed = case.old_cached_vm if mode == "cached_vm" else state.vm
            next_state = replace(next_state, observed_vm=observed)
        elif action == "count":
            certified = bool(state.copied_live and case.sealed and state.observed_vm)
            heap_match = state.heap_present and case.visible_to_snapshot
            result = int(bool(state.copied_live and (certified or heap_match)))
            next_state = replace(next_state, result=result, certified=certified)
        else:
            next_state = replace(next_state, pinned=False)
        yield f"reader:{action}", next_state
        yield "reader:cancel", replace(
            state, reader=5, pinned=False, result=None, cancelled=True
        )

    if not case.cleanup:
        return
    horizon_allows = not case.visible_to_snapshot or state.reader == 5
    if state.cleaner == 0 and horizon_allows:
        if not state.pinned or mode == "ignore_owner_pin":
            yield "vacuum:remove_owner", replace(state, cleaner=1, live=False)
    elif state.cleaner == 1:
        yield "vacuum:remove_heap", replace(state, cleaner=2, heap_present=False)
    elif state.cleaner == 2:
        yield "vacuum:set_vm_empty", replace(state, cleaner=3, vm=True)
    elif state.cleaner == 3:
        yield "writer:reuse_slot", replace(state, cleaner=4, vm=False)


def violation(case: Case, state: State) -> str | None:
    if state.cancelled and (state.pinned or state.result is not None):
        return "cancelled reader retained a pin or returned a partial count"
    if state.reader == 5 and state.pinned:
        return "completed reader retained its owner pin"
    if state.result is not None:
        expected = int(case.visible_to_snapshot and case.live and case.published)
        if state.result != expected:
            return f"count {state.result} differs from snapshot oracle {expected}"
    if state.certified and not case.sealed:
        return "mutable source was certified"
    return None


def explore(case: Case, mode: str = "correct") -> dict:
    if mode not in MODES:
        raise ValueError(f"unknown model mode: {mode}")
    initial = State(live=case.live, heap_present=case.live, vm=case.vm)
    parents: dict[State, tuple[State, str] | None] = {initial: None}
    pending = deque([initial])
    edges = 0
    first_failure = None
    terminal = 0
    while pending:
        state = pending.popleft()
        problem = violation(case, state)
        if problem is not None and first_failure is None:
            trace = []
            cursor = state
            while parents[cursor] is not None:
                previous, action = parents[cursor]
                trace.append(action)
                cursor = previous
            first_failure = {
                "reason": problem,
                "trace": list(reversed(trace)),
                "state": asdict(state),
            }
        successors = list(transitions(case, state, mode))
        terminal += not successors
        for action, following in successors:
            edges += 1
            if following not in parents:
                parents[following] = (state, action)
                pending.append(following)
    return {
        "case": case.name,
        "mode": mode,
        "states": len(parents),
        "transitions": edges,
        "terminal": terminal,
        "failure": first_failure,
    }


def run() -> dict:
    good = [explore(case) for case in CASES]
    if any(result["failure"] for result in good):
        raise AssertionError(good)
    negative = [
        explore(CASES[0], "ignore_owner_pin"),
        explore(CASES[0], "copy_before_pin"),
        explore(CASES[0], "detached_liveness"),
        explore(CASES[4], "cached_vm"),
    ]
    if any(result["failure"] is None for result in negative):
        raise AssertionError("a broken protocol escaped its negative control")
    return {"correct": good, "negative_controls": negative}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", help="write graph summary and counterexamples")
    args = parser.parse_args()
    result = json.dumps(run(), indent=2) + "\n"
    if args.output:
        from pathlib import Path

        Path(args.output).write_text(result)
    else:
        print(result, end="")


if __name__ == "__main__":
    main()
