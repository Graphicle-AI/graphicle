#!/usr/bin/env python3
"""Whole Bash extractor parity, real source fixtures and path-boundary cases."""
import ast
import importlib.metadata
import os
import random
import tempfile
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
import graphify.extract as facade
import graphify.extractors.bash as reference
import tree_sitter_bash
from tree_sitter import Language

native=Native('bash');original=reference.extract_bash;cases=0;helpers=0
def extract(path):
    global cases
    native.use_working_directory(os.getcwd())
    expected=original(path);actual=native.result('extract.bash',path=str(path));cases+=1
    if expected!=actual:native.fail(path=str(path),cwd=os.getcwd(),source=path.read_bytes().hex() if path.is_file() else None,expected=expected,actual=actual)
    return actual
try:
    selected=['tests/test_extract.py::'+n.name for n in ast.parse((SOURCE/'tests/test_extract.py').read_text()).body if isinstance(n,ast.FunctionDef) and (n.name.startswith('test_extract_bash_') or n.name.startswith('test_bash_var_sourced_')) and n.name!='test_extract_bash_missing_grammar_returns_error']
    dispatch={k:extract for k,v in facade._DISPATCH.items() if v is original}
    shebangs={k:extract for k,v in facade._SHEBANG_DISPATCH.items() if v is original}
    with patch.object(reference,'extract_bash',extract),patch.object(facade,'extract_bash',extract),patch.dict(facade._DISPATCH,dispatch),patch.dict(facade._SHEBANG_DISPATCH,shebangs):
        # Every selected integration fixture has fewer files than the upstream
        # process-pool threshold or explicitly requests sequential extraction.
        tests=upstream_tests(*selected)
    rng=random.Random(2026092320)
    values=['${DIR}/lib/x.sh','$D/lib/x.sh','${D}/${E}/x.sh','$D/../x.sh','$D/../../x.sh','${D}/$OTHER/x.sh','${D}','${D%/*}/x.sh','foo','//foo','${D}//x.sh','', '$(dirname "$D")/x.sh']
    for _ in range(1000):values.append(''.join(rng.choices(list('$abcXYZ_{}./09 ')+['\x1c','é','字'],k=rng.randrange(45))))
    for value in values:
        for allowed in [False,True]:
            expected=reference._bash_source_suffix(value,allowed);actual=native.result('bash.suffix',raw=value,allow_dotdot=allowed);helpers+=1
            if actual!=expected:native.fail(raw=value,allow_dotdot=allowed,expected=expected,actual=actual)
    for _ in range(700):
        ceiling=rng.choice(['/project/bin','/','.', './a', '//srv/repo','../project']);target=rng.choice(['/project/bin/file','/project/bin/../file','/project2/file','./a/file','./a/../x','../project/src/file','/else','//srv/repo/file'])
        expected=reference._within_tree(Path(ceiling),Path(target));actual=native.result('bash.within',ceiling=ceiling,target=target);helpers+=1
        if expected!=actual:native.fail(ceiling=ceiling,target=target,expected=expected,actual=actual)
    assignments=['"$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"','"$(cd "$(dirname "$0")/../.." && pwd)"','./lib','/abs/lib','../lib','"$OTHER"','`pwd`','',"''",' dirname $0 ', 'foo dirname BASH_SOURCE)../../../','\x1c./lib\x1f']
    for value in assignments+values[:100]:
        for directory in ['.','/','/project/bin','src/bin','//server/project','../root/bin']:
            expected=reference._bash_assignment_base(value,Path(directory));expected=str(expected) if expected is not None else None
            actual=native.result('bash.assignment_base',value=value,directory=directory);helpers+=1
            if actual!=expected:native.fail(value=value,directory=directory,expected=expected,actual=actual)
    with tempfile.TemporaryDirectory(prefix='graphify-bash-') as tmp:
        root=Path(tmp);(root/'lib').mkdir();(root/'bin').mkdir();(root/'bin/lib').mkdir()
        for relative in ['helpers.sh','lib/utils.sh','bin/lib/utils.sh','bin/child.sh']:(root/relative).write_text('#!/bin/bash\nhelper() { :; }\n')
        path=root/'bin/run.sh'
        lines=['source ../helpers.sh','source "${DIR}/lib/utils.sh"','source lib/utils.sh','source missing.sh','source "$SCRIPT/../lib/utils.sh"',
               'SCRIPT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"','ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"',
               'source "$(dirname "$SCRIPT")/lib/utils.sh"','source "$ROOT/lib/utils.sh"','bash ./child.sh','"$DIR/child.sh"',
               'a() { b; external; }','b() { a; }','main() { nested() { b; }; nested; a; }','main',
               'result=$(a)','$(a)','cat <(b)','export NAME=value','readonly OTHER=x','source() { a; }','source helper',
               'function weird-name() { :; }','weird-name','a > out.txt','a | b','if a; then b; fi',
               'source "${UNKNOWN}/../lib/utils.sh"','source "${UNKNOWN}/../../outside.sh"', 'source "${UNKNOWN}/$LATER"',
               'cat <<EOF\na\nEOF','# a; source ../helpers.sh','echo "$(a)"','x=$(b)']
        for _ in range(450):
            source='#!/usr/bin/env bash\n'+'\n'.join(rng.choices(lines,k=rng.randrange(1,22)))
            if rng.random()<.15:source=source[:rng.randrange(len(source))]
            path.write_text(source,encoding='utf-8');extract(path)
        path.write_bytes(b'\xffa() { :; }\na\x00');extract(path)
        saved=os.getcwd()
        try:
            os.chdir(root)
            for name in ['bin/run.sh','./bin/run.sh','bin/../helpers.sh']:
                extract(Path(name))
        finally:os.chdir(saved)
        extract(root/'absent.sh');extract(root)
finally:native.close()
grammar=Language(tree_sitter_bash.language())
native.receipt('graphify/extractors/bash.py',['src/extractors/bash.rs'],extraction_cases=cases,helper_comparisons=helpers,upstream_tests=tests,
    grammar={'python_package':importlib.metadata.version('tree-sitter-bash'),'rust_crate':'=0.25.1','node_kinds':grammar.node_kind_count,'parse_states':grammar.parse_state_count,'abi':grammar.abi_version},
    comparisons='Exact entrypoints, functions, calls, source resolution, inference/confidence, metadata, tracked assignment bases, expansion contexts, shadowing, nested functions, path hints, invalid/missing inputs and lexical traversal gates.',
    test_scope='Original multi-file fixtures retain Python extraction orchestration and ID remapping but route all Bash file extraction through Rust. The Rust code parses source; it does not execute input shell scripts.',
    adaptations=['The missing-Python-package mock test is excluded because the Bash grammar is a pinned native build dependency.','Stateless oracle subprocesses restart when an original relative-path fixture changes its working directory.'])
