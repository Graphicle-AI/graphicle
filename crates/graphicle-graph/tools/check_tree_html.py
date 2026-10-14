#!/usr/bin/env python3
"""Original tree viewer tests and exact hierarchy/HTML/file comparisons."""
import functools,importlib.util,inspect,json,os,random,tempfile
from pathlib import Path
from unittest.mock import patch
from contextlib import ExitStack
from parity import Native,SOURCE,upstream_tests
from naming import to_upstream
from upstream_cli import in_process_graphify
import graphify.tree_html as reference
spec=importlib.util.spec_from_file_location('_tree_oracle',SOURCE/'graphify/tree_html.py');oracle=importlib.util.module_from_spec(spec);spec.loader.exec_module(oracle)
native=Native('tree_html');counts={};files=0;errors=0
def wrapper(name,op):
    original=getattr(oracle,name);sig=inspect.signature(original)
    @functools.wraps(getattr(reference,name))
    def call(*args,**kwargs):
        global files,errors
        native.use_working_directory(os.getcwd());bound=sig.bind(*args,**kwargs);bound.apply_defaults();req={k:str(v) if isinstance(v,Path) else v for k,v in bound.arguments.items()}
        counts[op]=counts.get(op,0)+1
        try:expected=original(*args,**kwargs)
        except ValueError as e:
            actual=to_upstream(native.call(op,**req))
            if actual.get('ok') or str(e)!=actual.get('error'):native.fail(operation=op,request=req,expected_error=str(e),actual=actual)
            errors+=1;raise
        before=Path(expected).read_bytes() if name=='write_tree_html' else None
        actual=to_upstream(native.result(op,**req));wanted=str(expected) if isinstance(expected,Path) else expected
        if actual!=wanted:native.fail(operation=op,request=req,expected=wanted,actual=actual)
        if before is not None:
            files+=1
            # The written page is compared byte for byte after the name translation.
            if to_upstream(Path(actual).read_bytes().decode('utf-8')).encode('utf-8')!=before:native.fail(operation=op,expected=before.decode(),actual=Path(actual).read_text())
            return Path(actual)
        return actual
    return call
fns={name:wrapper(name,'tree.'+op) for name,op in {'build_tree':'build','emit_html':'emit','write_tree_html':'write','_common_root':'common_root','_make_truncation_leaf':'truncation'}.items()}
try:
    with ExitStack() as stack:
        for name,fn in fns.items():stack.enter_context(patch.object(reference,name,fn))
        stack.enter_context(in_process_graphify());tests=upstream_tests('tests/test_tree_html.py','tests/test_html_script_breakout.py::test_tree_html_labels_cannot_break_out_of_script')
    rng=random.Random(2026092353)
    paths=['','/','//','///','a','a/b.py','./a/b.py','../b/c.py','/a/b/c.py','A\\B\\f.py','file.py','a/../b.py','日本語.py','a/.hidden','a//b///c']
    labels=['plain','_private','A','a','é','中','x\u2028y','</script><script>alert(1)</script>','{title}',"'\"<&>"]
    for _ in range(300):
        p=rng.choices(paths,k=rng.randrange(12));fns['_common_root'](p)
        graph={'nodes':[{'id':str(i),'label':rng.choice(labels),'source_file':rng.choice(paths),'file_type':rng.choice(['code','document'])} for i in range(rng.randrange(40))]}
        try:tree=fns['build_tree'](graph,root=rng.choice([None,None,None,'','a','/','/abs']),max_children=rng.choice([-2,0,1,2,200]),project_label=rng.choice([None,'','Project <x>']))
        except ValueError:continue
        fns['emit_html'](tree,title=rng.choice(labels),header=rng.choice(labels),svg_width=rng.randrange(-2,8000),svg_height=rng.randrange(10000))
    for n in [-5,0,1,200]:fns['_make_truncation_leaf'](n)
    with tempfile.TemporaryDirectory() as tmp:
        root=Path(tmp);gp=root/'graph.json';out=root/'out/tree.html'
        for i in range(20):
            graph={'nodes':[{'id':str(n),'label':'S'+str(n),'source_file':'src/f.py','file_type':'code'} for n in range(i)]}
            gp.write_text(json.dumps(graph));fns['write_tree_html'](gp,out,max_children=i//2)
    native.receipt('graphify/tree_html.py',['src/tree_html.rs','src/assets/tree.html.template'],upstream_tests=tests,operations=counts,expected_errors=errors,exact_written_files=files,
        adaptations=['The static upstream HTML/CSS/JavaScript template is compiled as an asset; all Python tree building/rendering/writing logic is native.', 'Original CLI fixtures route their tree generation calls to Rust; complete native CLI migration is tracked separately.'])
finally:native.close()
