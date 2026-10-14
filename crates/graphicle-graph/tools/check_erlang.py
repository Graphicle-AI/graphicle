#!/usr/bin/env python3
"""Check Erlang extraction and remote-call resolution against pinned Graphify."""
import importlib.metadata
import random
from language_harness import Harness
from tree_sitter_language_pack import get_language

harness = Harness('erlang', 'erlang', resolver='erlang_remote_calls')
try:
    tests = harness.upstream('tests/test_erlang_extractor.py',
                             deselect={'test_erlang_missing_parser_reports_install_hint'})
    rng = random.Random(2026092701)
    sources = [
        '', '-module(a).\n', "-module('q m').\n-export(['q m'/0]).\n'q m'() -> ok.\n",
        '-module(w).\n-export([run/1, run/2, missing/0]).\nrun(X) -> run(X, 1).\nrun(X, Y) -> helper(X) + Y.\nhelper(X) -> X.\n',
        '-module(b).\n-behaviour(gen_server).\n-behavior(supervisor).\n-include("x.hrl").\n-include_lib("kernel/include/file.hrl").\n',
        '-module(r).\n-record(state, {a, b = 1}).\n-type t() :: ok.\n-type pair(A, B) :: {A, B}.\n-opaque o() :: term().\n-define(M, 1).\n-define(F(X), X + 1).\n',
        '-module(c).\nf(0) -> 0;\nf(N) -> f(N - 1).\ng() -> lists:map(fun f/1, [1]), io:format("~p", [x]), ?MODULE:f(1), M:f(2).\n',
        '-module(e).\n-export([f/16#1]).\nf(_) -> ok.\n', '-module(u).\n-export([f/1_0]).\n',
        '#!/usr/bin/env escript\nmain(_) -> run().\nrun() -> ok.\n',
        '-module(d).\nf() -> fun() -> g() end.\ng() -> case x of _ -> h() end.\nh() -> receive _ -> f() end.\n',
        '-module(broken).\nvalid() -> ok.\n% ghost() -> ok.\ninvalid(\n', '-module(Ω).\nπ() -> ok.\n',
    ]
    atoms = ['run', 'helper', 'go', "'weird name'", 'init', 'handle_call']
    for _ in range(400):
        lines = [f'-module({rng.choice(["m", "worker", "other", "sample"])}).']
        if rng.random() < .7:
            lines.append('-export([' + ', '.join(f'{rng.choice(atoms)}/{rng.randrange(3)}' for _ in range(rng.randrange(1, 4))) + ']).')
        if rng.random() < .3:
            lines.append(rng.choice(['-behaviour(gen_server).', '-include("h.hrl").', '-include_lib("a/b.hrl").', '-record(s, {x}).', '-type t(A) :: A.', '-define(D, 1).']))
        for _ in range(rng.randrange(1, 5)):
            name = rng.choice(atoms)
            arity = rng.randrange(3)
            args = ', '.join(f'A{i}' for i in range(arity))
            calls = ', '.join(rng.choices([f'{rng.choice(atoms)}({", ".join(["x"] * rng.randrange(3))})', 'other:go(1)', 'lists:reverse([])', '?MODULE:run()', 'ok'], k=rng.randrange(1, 4)))
            lines.append(f'{name}({args}) -> {calls}.')
        text = '\n'.join(lines) + '\n'
        if rng.random() < .2:
            text = text[:rng.randrange(len(text))]
        if rng.random() < .1:
            text = text.replace('\n', '\r\n')
        sources.append(text)
    sources += [b'\xff-module(a).\n', b'\xef\xbb\xbf-module(a).\nf() -> ok.\n', b'-module(z).\nf() ->\x00 ok.\n']
    harness.files('.erl', sources, names=['résumé.erl', 'two spaces.hrl', 'x.escript', 'dir/nested.erl'])
    corpora = [
        {'a.erl': '-module(a).\n-export([f/0]).\nf() -> b:g(1), b:g(), c:h(1).\n',
         'b.erl': '-module(b).\n-export([g/1]).\ng(X) -> X.\ng() -> ok.\n',
         'c.erl': '-module(c).\nh(X) -> X.\n', 'd.erl': '-module(c).\nh(Y) -> Y.\n'},
        {'w.escript': '#!/usr/bin/env escript\n-module(w).\n-include("w.hrl").\nrun() -> x:y().\n',
         'w.hrl': '-define(V, 1).\n', 'x.erl': "-module(x).\ny() -> w:run().\n"},
        {'self.erl': '-module(self).\nf() -> self:f().\n', 'q.erl': "-module('q').\n'go'() -> self:f().\n"},
    ]
    for corpus in corpora:
        harness.corpus(corpus)
finally:
    harness.close()
grammar = get_language('erlang')
harness.receipt('graphify/extractors/erlang.py', ['src/extractors/erlang.rs', 'vendor/tree-sitter-erlang/SOURCE.json'],
                upstream_tests=tests,
                grammar={'python_package': 'tree-sitter-language-pack ' + importlib.metadata.version('tree-sitter-language-pack'),
                         'rust_crate': 'vendored graphicle-tree-sitter-erlang (same generated parser)',
                         'node_kinds': grammar.node_kind_count, 'parse_states': grammar.parse_state_count, 'abi': grammar.abi_version},
                comparisons='Exact nodes, edges, raw remote calls and errors for modules, exports, behaviours, includes, records, types, macros, clauses and arity-bound calls; the remote-call resolver on every upstream pipeline run and on multi-file corpora.',
                adaptations=['The pinned grammar is compiled into the native binary; the optional-dependency install hint (test_erlang_missing_parser_reports_install_hint) has no native counterpart.'])
