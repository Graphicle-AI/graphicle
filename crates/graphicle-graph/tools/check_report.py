#!/usr/bin/env python3
"""Compare native report rendering with the retained upstream implementation."""
import functools,importlib.util,inspect,json,os,random,re,tempfile
from pathlib import Path
from contextlib import ExitStack
from unittest.mock import patch
from datetime import date
import networkx as nx
from parity import Native,SOURCE,upstream_tests
from naming import to_native,to_upstream
import graphify.report as reference
spec=importlib.util.spec_from_file_location('_graphify_report_oracle',SOURCE/'graphify/report.py')
oracle=importlib.util.module_from_spec(spec);oracle.__package__='graphify';spec.loader.exec_module(oracle)
native=Native('report');counts={};normalized_cycles=0
def normalize(text):
    # Cycle discovery order is explicitly deterministic in the native analysis
    # module. Normalize only complete cycle lines, not any report wording.
    lines=text.split('\n');positions=[];values=[]
    for i,line in enumerate(lines):
        if re.match(r'- \d+-file cycle: `',line):
            parts=line.split('`')[1].split(' -> ')[:-1]
            if parts:
                shift=min(range(len(parts)),key=lambda i:parts[i:]+parts[:i]);parts=parts[shift:]+parts[:shift]
                line=f'- {len(parts)}-file cycle: `'+ ' -> '.join(parts+[parts[0]])+'`'
            positions.append(i);values.append(line)
    for i,v in zip(positions,sorted(values)):lines[i]=v
    return '\n'.join(lines)
def wrapper(name,op):
    original=getattr(oracle,name);sig=inspect.signature(original)
    @functools.wraps(getattr(reference,name))
    def call(*args,**kwargs):
        global normalized_cycles
        native.use_working_directory(os.getcwd())
        bound=sig.bind(*args,**kwargs);bound.apply_defaults();request=dict(bound.arguments)
        if name=='generate':
            g=request.pop('G');request['graph']=nx.node_link_data(g,edges='links')
            request['neighbor_order']=[[n,list(g[n])] for n in g];request['today']=date.today().isoformat()
        if 'graph_path' in request:request['graph_path']=str(request['graph_path'])
        if name=='_portable_root_label':request['root']=str(request['root'])
        if name=='_learning_section':
            expected=list(bound.arguments['lines']);original(expected,bound.arguments['learning'],bound.arguments['top_n'])
        else:expected=original(*args,**kwargs)
        actual=to_upstream(native.result(op,**request));counts[op]=counts.get(op,0)+1
        if actual!=expected:
            if name=='generate' and normalize(actual)==normalize(expected):normalized_cycles+=1
            else:native.fail(operation=op,request=request,expected=expected,actual=actual)
        if name=='_learning_section':bound.arguments['lines'][:]=actual;return None
        return actual
    return call
ops={'generate':'generate','_portable_root_label':'root','_safe_community_name':'safe_name','load_learning_for_report':'load_learning','_learning_section':'learning'}
fns={name:wrapper(name,'report.'+op) for name,op in ops.items()}
try:
    with ExitStack() as stack:
        for name,fn in fns.items():stack.enter_context(patch.object(reference,name,fn))
        tests=upstream_tests('tests/test_report.py','tests/test_report_gap_thresholds.py')
    rng=random.Random(2026092351)
    for root in ['.','..','','/','//','///','../','a/..','/missing/a/..','C:\\Users\\p','/tmp/p/','./p']:
        fns['_portable_root_label'](root)
    for label in ['', 'a.md','a.MDX','label.markdown','*#[]^',' \r\n.md ','💡<>A?:B','é\x1c','name.MARKDOWN']:
        fns['_safe_community_name'](label)
    for _ in range(500):
        fns['_safe_community_name'](''.join(rng.choices('abc:<>?/\\\r\n.mdMDX_*#[]^é中\x1c',k=rng.randrange(50))))
        g=rng.choice([nx.Graph,nx.DiGraph,nx.MultiGraph,nx.MultiDiGraph])()
        for i in range(rng.randrange(35)):
            g.add_node(str(i),label=rng.choice([f'Class{i}',f'file{i}.py',f'call{i}()',f'é:{i}']),source_file=rng.choice([f'src/file{i}.py','x.md','','src/README.md']),file_type=rng.choice(['code','document','rationale','concept']),source_location='L1',**({'_origin':rng.choice(['ast','semantic'])} if rng.random()<.5 else {}))
        ns=list(g)
        if ns:
            for i in range(rng.randrange(60)):
                g.add_edge(rng.choice(ns),rng.choice(ns),relation=rng.choice(['calls','imports','references','semantically_similar_to','contains']),confidence=rng.choice(['EXTRACTED','INFERRED','AMBIGUOUS']),confidence_score=rng.random(),source_file='src/a.py')
        if rng.random()<.3:g.graph['hyperedges']=[{'id':'h','nodes':['a','b'],'label':'Together','confidence_score':0.125}]
        communities={};labels={};cohesion={}
        for n in g:
            cid=rng.randrange(8);communities.setdefault(cid,[]).append(n);labels[cid]=f'Community {cid} / A.md';cohesion[cid]=rng.random()
        gods=[{'label':'Core','degree':12}]
        surprises=[] if rng.random()<.5 else [{'source':'Left','target':'Right','relation':'semantically_similar_to','source_files':['a.py','b.py'],'confidence':'INFERRED','confidence_score':0.255,'note':'Unusual'}]
        learning={'overlay':{str(i):{'status':rng.choice(['preferred','tentative','contested']),'uses':rng.randrange(9),'score':rng.choice([0,None,1.25,'2.5']),'label':f'Node{i}','stale':bool(rng.randrange(2))} for i in range(rng.randrange(20))},'dead_ends':[{'question':'Why?','nodes':['A','B']}]*rng.randrange(3)}
        fns['_learning_section'](['preface'],learning,rng.choice([-2,0,1,10,30]))
        fns['generate'](g,communities,cohesion,labels,gods,surprises,{'total_files':30,'total_words':rng.randrange(10000000),'warning':rng.choice(['','Small corpus']),'unclassified':['a.lean','b.lean','noext','c.toml']}, {'input':rng.randrange(10**12),'output':99},'./project',min_community_size=rng.randrange(-1,8),built_at_commit=rng.choice([None,'abcdef0123456789']),learning=learning,obsidian=bool(rng.randrange(2)),suggested_questions=rng.choice([None,[{'type':'no_signal','why':'Sparse'}],[{'question':'Where is X?','why':'Across files'}],
            # 0.9.81 (#4199): AMBIGUOUS questions render under their own heading.
            [{'type':'ambiguous_edge','question':'What is the exact relationship between `a` and `b`?','why':'Edge tagged AMBIGUOUS','low_confidence':True},{'question':'Where is X?','why':'Across files'}],
            [{'type':'ambiguous_edge','question':'What links `c`?','why':'Low','low_confidence':True}]]))
    with tempfile.TemporaryDirectory() as tmp:
        root=Path(tmp);gp=root/'graph.json'
        fns['load_learning_for_report'](gp)
        (root/'memory').mkdir()
        for i in range(3):(root/'memory'/f'{i}.md').write_text('---\nquestion: "Why?"\noutcome: "dead_end"\nsource_nodes: ["A"]\n---\n')
        fns['load_learning_for_report'](gp)
        for data in ['broken','[]','null','{"nodes": []}','{"nodes": {}}']:
            # Each side reads the learning sidecar under its own name.
            for name in {'.graphify_learning.json',to_native('.graphify_learning.json')}:(root/name).write_text(data)
            fns['load_learning_for_report'](gp)
    native.receipt('graphify/report.py',['src/report.rs'],upstream_tests=tests,operations=counts,cycle_order_normalizations=normalized_cycles,
        adaptations=['Import cycle rotation/order follows the native analysis deterministic ordering; complete cycles are compared modulo ordering only.', 'Community and cohesion mapping IDs are integers represented as JSON object keys. The rendering date is pinned to the reference local day.'])
finally:native.close()
