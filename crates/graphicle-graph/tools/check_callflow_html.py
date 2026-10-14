#!/usr/bin/env python3
"""Run original call-flow tests unchanged and compare complete native outputs."""
import contextlib,datetime,functools,importlib.util,inspect,io,json,os,random,tempfile
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
from naming import NATIVE_FUNCTIONS,native_copies,native_fields,to_upstream
from upstream_cli import in_process_graphify
import graphify.callflow_html as reference
spec=importlib.util.spec_from_file_location('_callflow_oracle',SOURCE/'graphify/callflow_html.py');oracle=importlib.util.module_from_spec(spec);spec.loader.exec_module(oracle)
native=Native('callflow_html');counts={};files=0;errors=0;set_tie_normalizations=0
native_home=os.environ.get('HOME')
class Clock(datetime.datetime):
    @classmethod
    def now(cls,tz=None):return cls(2026,9,23,12,0,tzinfo=tz)
oracle.datetime=Clock
_original_refs=oracle.format_node_refs
def deterministic_refs(node_ids,*args,**kwargs):
    global set_tie_normalizations
    original=_original_refs(node_ids,*args,**kwargs)
    stable=_original_refs(sorted(node_ids),*args,**kwargs)
    if original!=stable:set_tie_normalizations+=1
    return stable
oracle.format_node_refs=deterministic_refs
def clean(v):
    if isinstance(v,Path):return str(v)
    if isinstance(v,(tuple,list,set)):return [clean(x) for x in v]
    if isinstance(v,dict):return {str(k):clean(x) for k,x in v.items()}
    if isinstance(v,reference.CallflowOptions):return clean(vars(v))
    return v
def wrapper(name):
    original=getattr(oracle,name);signature=inspect.signature(original)
    @functools.wraps(getattr(reference,name))
    def call(*args,**kwargs):
        global files,errors,native_home
        if native_home!=os.environ.get('HOME'):
            native.cwd=None;native_home=os.environ.get('HOME')
        native.use_working_directory(os.getcwd());bound=signature.bind(*args,**kwargs);bound.apply_defaults();values=list(bound.arguments.values());out=io.StringIO();err=io.StringIO();req={}
        if name=='first_present':values=[values[0],list(values[1]),values[2]]
        if name=='first_list':values=list(values[0])
        if name=='load_graph':
            import graphify.security
            req['cap']=graphify.security._MAX_GRAPH_FILE_BYTES
        if name=='write_callflow_html':values=[dict(bound.arguments)];req['now']='2026-09-23 12:00 UTC'
        # Operation and option names that mirror private upstream names are
        # renamed for the engine (resolve_graphify_paths, graphify_out).
        serialized=native_fields(clean(values));operation=NATIVE_FUNCTIONS.get(name,name);inputs=[]
        if name=='write_callflow_html' and bound.arguments.get('labels') is None:
            options={k:v for k,v in bound.arguments.items() if k!='verbose'}
            inputs=[oracle.resolve_graphify_paths(reference.CallflowOptions(options.pop('project'),**options))['labels']]
        try:
            with contextlib.redirect_stdout(out),contextlib.redirect_stderr(err):expected=original(*args,**kwargs)
        except (ValueError,FileNotFoundError,SystemExit,RuntimeError) as e:
            actual=to_upstream(native.call('callflow',function=operation,args=serialized,**req));errors+=1
            if actual.get('ok'):native.fail(function=name,expected_error=str(e),actual=actual)
            raise
        counts[name]=counts.get(name,0)+1
        before=Path(expected).read_bytes() if name=='write_callflow_html' else None
        # The native call reads the fixture's labels sidecar under its own name.
        with native_copies(*inputs):actual=to_upstream(native.result('callflow',function=operation,args=serialized,**req))
        if name=='write_callflow_html':
            if actual['path']!=str(expected) or actual['stdout']!=out.getvalue().splitlines() or actual['stderr']!=err.getvalue().splitlines():native.fail(function=name,expected=[str(expected),out.getvalue(),err.getvalue()],actual=actual)
            contents=to_upstream(Path(actual['path']).read_bytes().decode('utf-8')).encode('utf-8');files+=1
            if contents!=before:native.fail(function=name,expected=before.decode(),actual=contents.decode())
            print(out.getvalue(),end='');print(err.getvalue(),end='',file=__import__('sys').stderr);return Path(actual['path'])
        if name=='html_anchor_id':wanted=[expected,sorted(values[2])]
        elif name=='section_edge_summary':wanted=[[list(k),clean(v)] for k,v in expected.items()]
        else:wanted=clean(expected)
        if actual!=wanted:native.fail(function=name,args=serialized,expected=wanted,actual=actual)
        return expected if name in ['load_graph','_node_link_payload','section_edge_summary','resolve_graphify_paths','html_anchor_id'] else actual
    return call
names=[name for name,v in vars(reference).items() if inspect.isfunction(v) and v.__module__==reference.__name__ and name!='main']
fns={n:wrapper(n) for n in names}
try:
    with contextlib.ExitStack() as stack:
        for n,fn in fns.items():stack.enter_context(patch.object(reference,n,fn))
        stack.enter_context(in_process_graphify());tests=upstream_tests('tests/test_callflow_html.py')
    rng=random.Random(2026092356);strings=['','abc','___','A-Name','Graphify','中文','#x|A ->> B','../../evil','<script>x</script>','a\x1db','quotes\'"&<>','é','leading__name_that_is_very_long_with_suffix','foo.py','useAuth','test_x','streamer']
    import pwd
    for project in [None,'.','..','~','~/missing','~'+pwd.getpwuid(os.getuid()).pw_name+'/missing','/','//missing','~graphify_no_such_local_user_xyz']:
        try:fns['resolve_graphify_paths'](reference.CallflowOptions(project))
        except RuntimeError:pass
    for payload in [{'vertices':[{'key':'x'}],'graph':{'edges':[{'src':'x','dst':'y'}]}},{'nodes':[{'id':'a'},{'id':'b'}],'edges':[{'source':'b','target':'a','_src':'a','_tgt':'b'}]},{'nodes':[],'links':[]},{}]:
        fns['_node_link_payload'](payload)
    fns['mermaid_class_defs']()
    for _ in range(120):
        v=rng.choice(strings)
        for fn in ['safe_mermaid_text','html_comment_text','node_mermaid_id','mermaid_section_id','safe_file_path','safe_filename','is_zh','humanize_label']:
            fns[fn]({'id':v} if fn=='node_mermaid_id' else v)
        fns['stable_ascii_id'](v,limit=rng.choice([-3,0,1,48]));fns['truncate_text'](v,rng.randrange(-4,25));fns['html_anchor_id'](v,'fallback',set([v,'overview','fallback']));fns['mermaid_init'](rng.uniform(0.3,2.5));fns['normalize_communities'](rng.choice([None,0,'1,2, 3',[],[0,'x']]));fns['_keyword_score'](v,set(['x','a','abc','py']))
        fns['first_present']({'a':False,'b':0,'c':'','d':None},'c','d','a','b',default='fallback');fns['first_list'](None,{},[],['a']);fns['to_float'](rng.choice([None,1,'1.5','bad',False]));fns['endpoint_id'](rng.choice([None,0,v,{'uid':'u'},{'key':v}]))
        nodes=[]
        for i in range(rng.randrange(1,45)):
            raw={rng.choice(['id','node_id','uid']):str(i),rng.choice(['label','name','title']):rng.choice(strings),rng.choice(['community','cluster']):rng.randrange(9),rng.choice(['source_file','path']):rng.choice(['src/core.py','a/b/c/d.py','tests/test_calls.rs','../m.md','']),rng.choice(['node_type','kind']):rng.choice(['class','module','route','test','hook','function','']),rng.choice(['file_type','content_type']):rng.choice(['code','document','rationale',''])}
            nodes.append(fns['normalize_node'](raw,i))
        edges=[]
        for i in range(rng.randrange(70)):
            raw={rng.choice(['source','src','from']):rng.choice(nodes)['id'],rng.choice(['target','dst','to']):rng.choice(nodes)['id'],rng.choice(['relation','type']):rng.choice(['calls','uses','imports','contains','rationale_for','conceptually_related_to','indirect_call','references']),rng.choice(['confidence','evidence']):rng.choice(['EXTRACTED','INFERRED','AMBIGUOUS']),rng.choice(['confidence_score','weight']):rng.choice([.5,.85,1.0,'0.9'])}
            edge=fns['normalize_edge'](raw,i)
            if edge:edges.append(edge)
        lang=rng.choice(['en','zh-CN','auto']);labels={'0':rng.choice(strings),'3':rng.choice(strings)}
        fns['detect_lang'](lang,nodes,labels);sections=fns['derive_sections_from_communities'](nodes,labels,lang,rng.choice([-1,0,1,2,4,15]));sections=fns['normalize_sections'](sections+['bad',{'id':'overview','name':'Overview'},{'id':'stats','communities':[0]},{'id':'stats','communities':[1]}],lang);idx=fns['build_community_index'](nodes);mapping=fns['build_section_node_map'](sections,idx);classified=fns['classify_edges'](edges,mapping)
        for fn,args in [('preferred_edges',(edges,)),('preferred_edges',(edges,True)),('node_degree_scores',(edges,)),('select_diagram_nodes',(nodes,edges,rng.choice([-2,0,1,4,18]))),('group_nodes_by_file',(nodes,)),('section_edge_summary',(classified,)),('generate_overview_graph',(sections,mapping,classified,labels,lang,1.0)),('generate_section_flowchart',('sec','Section',nodes,edges,lang,1.0,12,20)),('generate_nav',(sections,)),('generate_call_table_rows',(nodes,edges,lang,edges,nodes)),('generate_header',(sections,{},lang)),('derive_flow_chain',(sections,classified)),('generate_overview_cards',({},'',sections,mapping,classified,lang)),('section_keywords',(nodes,)),('generate_section_intro',(sections[-1],nodes,len(edges),lang)),('generate_section_cards',(sections[-1],nodes,edges,lang)),('_community_text',(nodes,'Label')),('label_for_community',('4',labels,nodes,lang)),('_rank_grouped_sections',({'a':{'priority':0,'node_count':4,'id':'a','communities':[0]},'b':{'priority':1,'node_count':2,'id':'b','communities':[1]}},rng.choice([0,1,2]))),('node_in_section',('x',set(['x','y'])))]:fns[fn](*args)
        for n in nodes[:4]:
            for fn in ['node_kind','node_label','node_importance','node_display_name']:fns[fn](n)
            fns['_suggest_tag'](n['label'],n['file_type'],lang);fns['_describe_node'](n['label'],n['source_file'],n['file_type'],lang)
        for e in edges[:4]:fns['should_include_edge'](e);fns['edge_score'](e);fns['relation_label'](e['relation'],lang)
    with tempfile.TemporaryDirectory() as tmp:
        root=Path(tmp);out=root/'graphify-out';out.mkdir();gp=out/'graph.json';lp=out/'.graphify_labels.json';sp=root/'sections.json'
        for i in range(28):
            nodes=[{'id':'n'+str(j),'label':rng.choice(strings),'source_file':rng.choice(['src/a.py','src/b.py','tests/c.py']),'community':j%3} for j in range(30)]
            edges=[{'source':nodes[j]['id'],'target':nodes[(j+1)%30]['id'],'relation':rng.choice(['calls','uses','contains']),'confidence':rng.choice(['EXTRACTED','AMBIGUOUS','INFERRED'])} for j in range(40%len(nodes))]
            g={'nodes':nodes,rng.choice(['links','edges']):edges,'hyperedges':[{'id':'h','label':rng.choice(strings),'nodes':[n['id'] for n in nodes],'relation':'participate_in'}],'graph':{'project_name':'Fixture'},'metadata':{'commit':'abc123456789'}};gp.write_text(json.dumps(g));lp.write_text(json.dumps({'labels':{'0':'API','1':'Docs','2':'Tests'}}));sp.write_text(json.dumps([{'id':'a','name':rng.choice(strings),'communities':[0,1]},{'id':'b','communities':[2]}]));(out/'GRAPH_REPORT.md').write_text('## Summary\n- A & B\n## God Nodes\n1. one\n2. two\n')
            fns['load_graph'](gp);fns['load_labels'](lp);fns['load_sections'](sp);fns['load_report'](out/'GRAPH_REPORT.md');fns['_report_highlights']((out/'GRAPH_REPORT.md').read_text(),'en')
            fns['write_callflow_html'](root,sections=sp if i%2 else None,lang=rng.choice(['en','zh-CN','auto']),output='docs/flow.html',verbose=bool(i%2),diagram_scale=rng.choice([.65,1.,1.8]),max_diagram_nodes=rng.choice([3,18]),max_diagram_edges=rng.choice([0,24]))
        gp.write_text('{bad')
        try:fns['load_graph'](gp)
        except SystemExit:pass
    native.receipt('graphify/callflow_html.py',['src/callflow_data.rs','src/callflow_html.rs','src/assets/callflow/templates.json'],upstream_tests=tests,operations=counts,exact_written_files=files,expected_errors=errors,set_tie_normalizations=set_tie_normalizations,adaptations=['Static CSS/HTML/JavaScript and literal template segments are retained as assets. All normalization, selection, escaping, document generation and writing run in Rust.','Caller/callee references with equal display names use node-ID ordering as a deterministic secondary key; upstream ties depend on Python set hash order. Only those ties are normalized in the comparison.','Original CLI fixtures invoke the native call-flow generator; full package CLI migration is tracked separately. JSON parser diagnostics use Rust wording.'])
finally:native.close()
