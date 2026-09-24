#!/usr/bin/env python3
"""Finite-set oracle for grouped lower bounds, not a Rust or PostgreSQL test.

NOT is deliberately unknown for pruning. A term absent at this group can match
later. Only an exhausted immutable stream supplies the End bound.
"""
from __future__ import annotations
from bisect import bisect_left


def cover(node: tuple) -> set[str] | None:
    op, *args = node
    if op == 'term':
        return {args[0]}
    if op == 'not':
        return None
    left, right = (cover(child) for child in args)
    if op == 'and':
        if left is None:
            return right
        if right is None:
            return left
        return left | right
    if op == 'or':
        return None if left is None or right is None else left | right
    raise ValueError(op)


def matches(node: tuple, terms: dict[str, set[int]], universe: set[int]) -> set[int]:
    op, *args = node
    if op == 'term':
        return terms.get(args[0], set()) & universe
    if op == 'not':
        return universe - matches(args[0], terms, universe)
    left, right = (matches(child, terms, universe) for child in args)
    return left & right if op == 'and' else left | right


def candidates(node: tuple, terms: dict[str, set[int]], universe: set[int]) -> tuple[list[int], int]:
    positive = cover(node)
    if positive is None:
        return sorted(universe), 0
    end = max(universe, default=-1) + 1
    streams = {name: sorted(terms.get(name, set()) & universe) for name in positive}
    positions = dict.fromkeys(positive, 0)
    probes = 0

    def bound(expr: tuple) -> int:
        op, *args = expr
        if op == 'not':
            return -1
        if op == 'term':
            values = streams.get(args[0], [])
            position = positions.get(args[0], 0)
            return values[position] if position < len(values) else end
        left, right = (bound(child) for child in args)
        return max(left, right) if op == 'and' else min(left, right)

    output = []
    limit = sum(len(values) for values in streams.values()) + len(universe) + 1
    for _ in range(limit):
        base = bound(node)
        probes += 1
        if base == end:
            return output, probes
        if base < 0:
            raise AssertionError('finite cover must have a lower bound')
        advanced = False
        for name, values in streams.items():
            old = positions[name]
            new = bisect_left(values, base, old)
            positions[name] = new
            advanced |= old != new
        if advanced:
            continue
        if output and output[-1] >= base:
            raise AssertionError('nonmonotone frontier')
        output.append(base)
        for name, values in streams.items():
            position = positions[name]
            if position < len(values) and values[position] == base:
                positions[name] += 1
    raise AssertionError('unbounded frontier loop')


def phrase_oracle(words: list[str], terms: list[str], budget: int) -> tuple[bool, int] | None:
    cost = 1
    for start in range(max(0, len(words) - len(terms) + 1)):
        equal = True
        for distance, term in enumerate(terms):
            cost += 1
            if cost > budget:
                return None
            if words[start + distance] != term:
                equal = False
                break
        if equal:
            return True, cost
    return (False, cost) if cost <= budget else None


def phrase_ring(words: list[str], terms: list[str], budget: int) -> tuple[bool, int] | None:
    window = [''] * len(terms)
    cursor = 0
    filled = 0
    cost = 1
    if cost > budget:
        return None
    for word in words:
        window[cursor] = word
        cursor = (cursor + 1) % len(terms)
        filled = min(filled + 1, len(terms))
        if filled != len(terms):
            continue
        equal = True
        for actual, term in zip(window[cursor:] + window[:cursor], terms):
            cost += 1
            if cost > budget:
                return None
            if actual != term:
                equal = False
                break
        if equal:
            return True, cost
    return False, cost
