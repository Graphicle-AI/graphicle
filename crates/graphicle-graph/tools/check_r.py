#!/usr/bin/env python3
"""Check R extraction and sourced-call resolution against pinned Graphify."""
import importlib.metadata
import random
from language_harness import Harness
from tree_sitter_language_pack import get_language

harness = Harness('r', 'r', resolver='r_sourced_calls')
try:
    tests = harness.upstream('tests/test_r_extractor.py',
                             deselect={'test_r_missing_parser_reports_install_hint'})
    rng = random.Random(2026092702)
    sources = [
        '', 'x <- 1\ny = 2\n3 -> z\nf <<- function() NULL\n',
        'helper <- function(x) x + 1\nrun <- function(x) helper(x)\n',
        'function(x) x -> right_assigned\nfunction(y) y ->> deep\n',
        'library(dplyr)\nrequire("ggplot2")\nlibrary(pkg = "stats")\nsource("helpers.R")\nsource(paste0("a", "b"))\nsource(dir$file)\n',
        'outer <- function(x) {\n  inner <- function(y) y\n  inner(x)\n  shared(x)\n  pkg::shared(x)\n  get("shared")(x)\n  print(x)\n}\n',
        'Person <- R6Class("Person", inherit = BasePerson, public = list(run = function(x) outer(x), n = 1), private = list(hide = function() NULL))\n',
        'Acc <- setRefClass("Account", fields = list(total = "numeric"), methods = list(deposit = function(x) total <<- total + x))\n',
        'setClass("Shape", representation("VIRTUAL"))\nsetGeneric("area", function(shape) standardGeneric("area"))\nsetMethod("area", "Shape", function(shape) 0)\nsetMethod("area", "Circle", function(shape) pi)\n',
        'Geom <- ggproto("Geom", Base, draw = function(self) NULL)\nKid <- R6Class(inherit = Geom)\n',
        'if (TRUE) {\n  nested <- function() 1\n}\nlocal({\n  hidden <- function() nested()\n})\n',
        'f <- function(x) {\n  g <- function() f(x)\n  g()\n}\n`odd name` <- function() `odd name`()\n',
        'broken <- function(x) {\n  valid()\n# ghost <- function() 1\n', 'π <- function() café()\ncafé <- function() 1\n',
    ]
    names = ['run', 'helper', 'double', 'shared', 'go']
    for _ in range(400):
        lines = []
        for _ in range(rng.randrange(1, 6)):
            name = rng.choice(names)
            body = rng.choice([f'{rng.choice(names)}(x)', '{\n  ' + '\n  '.join(rng.choices([f'{rng.choice(names)}(x)', 'print(x)', 'stats::median(x)', 'x <- 1', f'inner <- function(y) {rng.choice(names)}(y)'], k=rng.randrange(1, 4))) + '\n}', 'x'])
            assign = rng.choice(['<-', '=', '<<-'])
            lines.append(rng.choice([f'{name} {assign} function(x) {body}', f'function(x) {body} -> {name}', f'{name} {assign} {rng.randrange(9)}',
                                     f'library({rng.choice(names)})', f'source("{rng.choice(names)}.R")',
                                     f'{name.title()} <- R6Class("{name.title()}", public = list({name} = function(x) {body}))',
                                     f'setMethod("{name}", "{rng.choice(names).title()}", function(x) {body})']))
        text = '\n'.join(lines) + '\n'
        if rng.random() < .2:
            text = text[:rng.randrange(len(text))]
        if rng.random() < .1:
            text = text.replace('\n', '\r\n')
        sources.append(text)
    sources += [b'\xff x <- 1\n', b'\xef\xbb\xbff <- function() 1\n', b'f <- function() {\x00 g() }\n']
    harness.files('.R', sources, names=['analysis.r', 'résumé.R', 'two spaces.R', 'dir/nested.R'])
    corpora = [
        {'main.R': 'source("helpers.R")\nsource("other.R")\nrun <- function(x) shared(x) + only(x) + dup(x)\n',
         'helpers.R': 'shared <- function(x) x\ndup <- function(x) x\n',
         'other.R': 'only <- function(x) x\ndup <- function(x) x\n', 'unsourced.R': 'lonely <- function() shared()\n'},
        {'a.r': 'source("sub/b.r")\nf <- function() g()\n', 'sub/b.r': 'g <- function() f()\n'},
        {'self.R': 'source("self.R")\nloop <- function() loop()\n'},
    ]
    for corpus in corpora:
        harness.corpus(corpus)
finally:
    harness.close()
grammar = get_language('r')
harness.receipt('graphify/extractors/r.py', ['src/extractors/r.rs', 'vendor/tree-sitter-r/SOURCE.json'],
                upstream_tests=tests,
                grammar={'python_package': 'tree-sitter-language-pack ' + importlib.metadata.version('tree-sitter-language-pack'),
                         'rust_crate': 'vendored graphicle-tree-sitter-r (same generated parser)',
                         'node_kinds': grammar.node_kind_count, 'parse_states': grammar.parse_state_count, 'abi': grammar.abi_version},
                comparisons='Exact nodes, edges, raw calls and errors for assignments in every direction, nested functions, R6/Reference/ggproto/S4 classes and methods, packages, source() imports and lexical/namespace calls; the sourced-call resolver on every upstream pipeline run and on multi-file corpora.',
                adaptations=['The pinned grammar is compiled into the native binary; the optional-dependency install hint (test_r_missing_parser_reports_install_hint) has no native counterpart.',
                             'Native lexical-scope walks are bounded when punctuation-only quoted names normalize to their owner ID. The pinned implementation loops on those scope cycles; tests/r_scope_cycles.rs checks termination and preservation of unresolved calls without running the hanging oracle case.'])
