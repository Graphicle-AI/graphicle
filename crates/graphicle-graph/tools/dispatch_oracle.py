"""The pinned interface-dispatch mechanism, taking implementers in arrival order.

Since 0.9.89 (#3800) `resolve_interface_dispatch` links an interface member to
every implementer, and it walks each interface's implementers as a Python
`set()` of node ids. A set of strings iterates in hash order, which changes
from run to run (PYTHONHASHSEED), so the reference's order of the new
`dispatches_to` edges varies between runs, and so does which implementer's file
and line an edge carries when two implementers list the same member node. The
engine takes implementers in the order their `implements` edges arrive.

The comparison oracle is the pinned module, unedited, run with that order: its
`set()` calls build insertion-ordered sets. Nothing else changes; the upstream
test files compare the edges as sets and run against the unmodified reference
with the native resolver substituted.
"""
import types

from parity import SOURCE


class InsertionOrderedSet(dict):
    """A `set()` stand-in that iterates in insertion order (a dict's keys).

    Membership, length, iteration and `add` are what the module uses; equal
    elements (1, 1.0, True) collapse as they do in a set, and an unhashable
    element raises the same TypeError. Like `set(iterable)` it can be built
    from an iterable (0.9.95's `_overload_pairs` builds `set(pairs.values())`).
    """

    def __init__(self, iterable=()):
        super().__init__()
        for value in iterable:
            self[value] = None

    def add(self, value):
        self[value] = None


def mechanism():
    """A fresh copy of graphify/interface_dispatch.py whose `set()` is ordered."""
    module = types.ModuleType('_graphify_interface_dispatch_ordered')
    module.__package__ = 'graphify'
    module.set = InsertionOrderedSet
    path = SOURCE / 'graphify/interface_dispatch.py'
    exec(compile(path.read_text(), f'<{path} with insertion-ordered sets>', 'exec'), module.__dict__)
    return module


ORDER_NOTE = ('Upstream walks each interface\'s implementers as a Python set (string-hash order, '
              'which varies between runs); the engine takes them in the order their implements edges '
              'arrive. The comparison runs the pinned module with insertion-ordered sets '
              '(tools/dispatch_oracle.py), so both sides use that order and edges compare exactly.')
