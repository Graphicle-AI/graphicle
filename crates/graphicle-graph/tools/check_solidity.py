#!/usr/bin/env python3
"""Check Solidity extraction and type-reference resolution against pinned Graphify."""
import importlib.metadata
import random
import tree_sitter_solidity
from language_harness import Harness
from tree_sitter import Language

harness = Harness('solidity', 'solidity', resolver='solidity_type_references')
try:
    tests = harness.upstream('tests/test_solidity_extractor.py',
                             deselect={'test_solidity_missing_parser_reports_install_hint'})
    rng = random.Random(2026092703)
    sources = [
        '', 'pragma solidity ^0.8.0;\n',
        'import "./A.sol";\nimport \'../B.sol\';\nimport "https://x.io/C.sol";\nimport {X as Y} from "./D.sol";\nimport * as Z from "./E.sol";\n',
        'contract A is B, C.D, E(1) {\n using SafeMath for uint;\n using {f} for uint;\n using L.M for *;\n}\n',
        'interface I { function f(uint a, address b) external returns (bool); event E(uint indexed a); error Bad(uint); }\n',
        'library L { struct S { uint a; mapping(uint => uint) m; } enum K { A, B } function g(S storage s) internal { h(); } function h() private {} }\n',
        'contract C {\n uint public x;\n address owner;\n event Moved(uint a, uint b);\n error Nope();\n modifier only() { _; }\n modifier guard(uint n) { _; }\n'
        ' constructor(uint a) { x = a; }\n receive() external payable {}\n fallback(bytes calldata d) external returns (bytes memory) {}\n'
        ' function f() public only guard(1) { g(); g(1); this.g(); other.g(); emit Moved(1, 2); revert Nope(); x(); Moved(1, 2); }\n'
        ' function g() internal {}\n function g(uint a) internal {}\n}\n',
        'contract Broken {\n function valid() public {}\n // function ghost() public {}\n function invalid(\n',
        'contract Names {\n // π café comment\n string s = unicode"Ω π";\n function run() public { emit Log("café"); helper(); }\n function helper() internal {}\n}\n',
        # File-level free functions and the calls they make (0.9.73).
        'pragma solidity ^0.8.0;\nfunction free(uint a) pure returns (uint) { return helper(a); }\nfunction helper(uint a) pure returns (uint) { return a; }\n'
        'function twice() { twice(); twice(1); helper(1); helper(); Lib.helper(1); this.helper(1); Missing(); }\nfunction twice(uint) {}\n'
        'function dup() {}\nfunction dup() {}\nfunction useDup() { dup(); }\ncontract C { function f() public { free(1); } function g() internal { helper(2); } }\n',
        'function noBody() ;\nfunction (uint a) {}\n',
    ]
    names = ['run', 'record', 'helper', 'touch']
    for _ in range(400):
        members = []
        for _ in range(rng.randrange(1, 7)):
            name = rng.choice(names)
            arity = rng.randrange(3)
            params = ', '.join(f'uint p{i}' for i in range(arity))
            calls = ' '.join(rng.choices([f'{rng.choice(names)}({", ".join(["1"] * rng.randrange(3))});', 'this.run();', 'lib.touch(1);', 'emit Done(1);', 'value = 1;'], k=rng.randrange(1, 4)))
            members.append(rng.choice([
                f'function {name}({params}) public {rng.choice(["", "guarded", "guarded()"])} {{ {calls} }}',
                'uint public value;', 'event Done(uint a);', 'error Failed();', 'modifier guarded() { _; }',
                'struct Item { uint value; }', 'enum State { On, Off }', 'constructor() {}', 'using Lib for uint;',
            ]))
        kind = rng.choice(['contract', 'interface', 'library', 'abstract contract'])
        base = rng.choice(['', ' is Base', ' is Base, Other'])
        text = f'pragma solidity ^0.8.0;\nimport "./Base.sol";\n{kind} {rng.choice(["Counter", "Token", "Lib"])}{base} {{\n' + '\n'.join(members) + '\n}\n'
        for _ in range(rng.choice([0, 0, 1, 2, 3])):
            name = rng.choice(names)
            arity = rng.randrange(3)
            params = ', '.join(f'uint p{i}' for i in range(arity))
            calls = ' '.join(rng.choices([f'{rng.choice(names)}({", ".join(["1"] * rng.randrange(3))});', 'Lib.touch(1);', 'value = 1;'], k=rng.randrange(1, 4)))
            free = f'function {name}({params}) pure {{ {calls} }}\n'
            text = free + text if rng.random() < .5 else text + free
        if rng.random() < .2:
            text = text[:rng.randrange(len(text))]
        if rng.random() < .1:
            text = text.replace('\n', '\r\n')
        sources.append(text)
    sources += [b'\xff contract A {}\n', b'\xef\xbb\xbfcontract A { function f() public {} }\n', b'contract A { function f() public {\x00 g(); } }\n']
    harness.files('.sol', sources, names=['Token.sol', 'résumé.sol', 'two spaces.sol', 'dir/nested.sol'])
    corpora = [
        {'Counter.sol': 'import "./Base.sol";\ncontract Counter is Base, Missing { using Lib for uint; using Local for uint; }\ncontract Local {}\n',
         'Base.sol': 'import "./Lib.sol";\ncontract Base {}\n', 'Lib.sol': 'library Lib {}\n'},
        {'a/A.sol': 'import "../b/B.sol";\ncontract A is B {}\n', 'b/B.sol': 'contract B {}\ninterface B2 {}\n',
         'c/C.sol': 'contract B {}\n'},
        {'Dup.sol': 'import "./One.sol";\nimport "./Two.sol";\ncontract Dup is Shared {}\n',
         'One.sol': 'contract Shared {}\n', 'Two.sol': 'contract Shared {}\n'},
        {'Free.sol': 'import "./Lib.sol";\nfunction run() { Lib.helper(1); helper(); }\nfunction helper() {}\n',
         'Lib.sol': 'library Lib { function helper(uint a) internal {} }\nfunction helper(uint a) {}\n'},
    ]
    for corpus in corpora:
        harness.corpus(corpus)
finally:
    harness.close()
grammar = Language(tree_sitter_solidity.language())
harness.receipt('graphify/extractors/solidity.py', ['src/extractors/solidity.rs', 'vendor/tree-sitter-solidity/SOURCE.json'],
                upstream_tests=tests,
                grammar={'python_package': 'tree-sitter-solidity ' + importlib.metadata.version('tree-sitter-solidity'),
                         'rust_crate': 'vendored graphicle-tree-sitter-solidity (same generated parser)',
                         'node_kinds': grammar.node_kind_count, 'parse_states': grammar.parse_state_count, 'abi': grammar.abi_version},
                comparisons='Exact nodes, edges, raw calls and errors for imports, contracts/interfaces/libraries, inheritance, using directives, structs, enums, events, errors, state variables, overloads, modifiers, file-level free functions and calls; the type-reference resolver on every upstream pipeline run and on multi-file corpora.',
                adaptations=['The pinned grammar is compiled into the native binary; the optional-dependency install hint (test_solidity_missing_parser_reports_install_hint) has no native counterpart.'],
                interface_differences=['Non-ASCII characters inside identifiers are not Solidity syntax; tree-sitter 0.27 (Rust) and py-tree-sitter 0.25.2 recover from that error differently (e.g. `contract Ω { ... café() ... }`). Valid sources, including Unicode strings and comments, parse identically and are compared exactly.'])
