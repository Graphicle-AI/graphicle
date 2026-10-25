#!/usr/bin/env python3
"""Qualify native PR commands and rendering without contacting GitHub."""
import contextlib, copy, dataclasses, inspect, io, json, os, random, sys, tempfile, types
from datetime import datetime,timezone,timedelta
from pathlib import Path
from unittest.mock import MagicMock,patch
from parity import Native,SOURCE
from naming import to_upstream
import graphify.prs as reference
import graphify.llm as llm
import networkx as nx
import pytest

oracle=types.ModuleType('reference_prs');oracle.__package__='graphify';sys.modules[oracle.__name__]=oracle
exec(compile((SOURCE/'graphify/prs.py').read_text(),str(SOURCE/'graphify/prs.py'),'exec'),oracle.__dict__)
native=Native('prs');saved={};comparisons={};native_commands=0
NOW=datetime(2026,9,23,12,0,0,tzinfo=timezone.utc)
class FrozenDatetime(datetime):
    @classmethod
    def now(cls,tz=None):return NOW
def plain(v):
    if isinstance(v,nx.Graph):return nx.node_link_data(v,edges='links')
    if dataclasses.is_dataclass(v):return plain(dataclasses.asdict(v))
    if isinstance(v,datetime):return v.isoformat()
    if isinstance(v,Path):return str(v)
    if isinstance(v,dict):return {str(k):plain(v) for k,v in v.items()}
    if isinstance(v,(list,tuple,set)):return [plain(x) for x in v]
    return v
def call(function,**kw):
    kw.setdefault('env',{})
    result=native.result('prs',function=function,**{k:plain(v) for k,v in kw.items()})
    return to_upstream(result)
def value(function,**kw):return call(function,**kw)['value']
def wrap(name):
    signature=inspect.signature(getattr(reference,name))
    def invoke(*args,**kw):
        bound=signature.bind(*args,**kw);bound.apply_defaults()
        params=dict(bound.arguments);params['no_color']=bool(reference._NO_COLOR)
        result=value(name,**params)
        if name=='compute_pr_impact':return tuple(result)
        if name=='build_community_labels':return {int(k):v for k,v in result.items()}
        if name.startswith('render_'):print(result,end='');return None
        return result
    return invoke
class Reports:
    passed=0;skipped=0;deselected=[]
    def pytest_collection_modifyitems(self,config,items):
        excluded={'TestFetchWorktrees','TestDetectDefaultBranch','TestSubprocessOutputEncoding','TestSubprocessStdinIsolation'}
        omitted=[i for i in items if i.cls and i.cls.__name__ in excluded]
        items[:]=[i for i in items if i not in omitted];self.deselected=[i.nodeid for i in omitted];config.hook.pytest_deselected(items=omitted)
    def pytest_runtest_logreport(self,report):
        if report.when=='call' and report.passed:self.passed+=1
        if report.skipped:self.skipped+=1
reports=Reports()
def compare(name,**kw):
    out=io.StringIO()
    with contextlib.redirect_stdout(out):expected=getattr(oracle,name)(**kw)
    if name.startswith('render_'):expected=out.getvalue()
    actual=value(name,**kw,now_micros=int(NOW.timestamp()*1e6),no_color=bool(oracle._NO_COLOR))
    comparisons[name]=comparisons.get(name,0)+1
    if actual!=plain(expected):native.fail(function=name,arguments=plain(kw),expected=plain(expected),actual=actual)
    return actual
def make_pr(i,rng):
    return oracle.PRInfo(number=i,title=rng.choice(['Improve API','Fix auth','docs: فارسی 🏆','A'*90]),branch='branch-'+str(i),base_branch=rng.choice(['main','develop']),
        author=rng.choice(['alice','Bob','東京']),is_draft=rng.choice([False,True]),review_decision=rng.choice(['','APPROVED','CHANGES_REQUESTED']),
        ci_status=rng.choice(['SUCCESS','FAILURE','PENDING','NONE']),updated_at=NOW-timedelta(days=rng.randrange(-2,26),seconds=rng.randrange(3600)),expected_base='main',
        worktree_path=rng.choice([None,'/tmp/feature']),communities_touched=rng.sample([0,1,2,3],rng.randrange(5)),nodes_affected=rng.choice([0,1,2,70]),
        files_changed=[f'src/{j}.rs' for j in range(rng.randrange(15))])
try:
    for name in ['_classify','_parse_ci','_path_match','compute_pr_impact','format_prs_text','build_community_labels']:
        saved[name]=getattr(reference,name);setattr(reference,name,wrap(name))
    status=pytest.main(['-q','-p','no:cacheprovider',str(SOURCE/'tests/test_prs.py')],plugins=[reports]);assert status==0,status
    for name,fn in saved.items():setattr(reference,name,fn)
    oracle.datetime=FrozenDatetime
    rng=random.Random(75)
    for _ in range(180):
        prs=[make_pr(i,rng) for i in range(rng.randrange(12))]
        oracle._NO_COLOR=rng.choice([True,False])
        compare('format_prs_text',prs=prs,base='main')
        compare('render_dashboard',prs=prs,base='main',show_wrong_base=rng.choice([True,False]))
        compare('render_worktrees',prs=prs,worktrees={'branch-0':'/tmp/main','detached':'/tmp/detached','branch-2':'/tmp/東京'})
        compare('render_conflicts',prs=prs,base='main',community_labels={0:['API','Reader'],1:['Corpus'],2:['Extraction']})
        for pr in prs[:2]:
            compare('_classify',pr=pr,base='main');compare('render_pr_detail',pr=pr,repo=None)
            actual=value('PRInfo',pr=pr,now_micros=int(NOW.timestamp()*1e6))
            assert actual=={'status':pr.status,'days_old':pr.days_old,'blast_radius':pr.blast_radius},actual
        graph=nx.Graph()
        for i in range(30):graph.add_node(str(i),source_file=rng.choice(['src/api.rs','src/auth.rs','a/api.rs','api.rs','',None]),community=rng.choice([0,1,2,None,'3']))
        compare('compute_pr_impact',files=rng.choices(['api.rs','src/api.rs','auth.rs','unmatched.rs'],k=5),G=graph)
        compare('build_community_labels',data=nx.node_link_data(graph,edges='links'),top_n=rng.choice([-2,0,1,4]))
        s=rng.choice(['','a😀b','\x1b[31mred\x1b[0m','日本語','x'*100])
        compare('_pad',s=s,width=rng.randrange(-2,12));compare('_truncate',s=s,n=rng.randrange(-2,12))
        compare('_parse_ci',rollup=[{'conclusion':rng.choice(['SUCCESS','FAILURE','CANCELLED','TIMED_OUT',None,'SKIPPED']),'status':rng.choice(['IN_PROGRESS','QUEUED','COMPLETED'])} for _ in range(4)])
        # 0.9.93 (#4533): legacy StatusContext entries carry only `state`.
        compare('_parse_ci',rollup=[rng.choice([{'conclusion':rng.choice(['SUCCESS','FAILURE',None,'','SKIPPED']),'status':rng.choice(['IN_PROGRESS','COMPLETED',None])},
                                                {'state':rng.choice(['SUCCESS','FAILURE','ERROR','PENDING','EXPECTED','',None])},
                                                {'conclusion':rng.choice([None,'']),'state':rng.choice(['ERROR','PENDING','SUCCESS'])}]) for _ in range(rng.randrange(0,4))])
    oracle._NO_COLOR=True
    # Recorded PR transport fixtures are selected by their actual argv.
    raw=[{'number':1,'title':'API 🧠','headRefName':'feature','baseRefName':'main','author':{'login':'alice'},'isDraft':False,'reviewDecision':'',
          'statusCheckRollup':[{'conclusion':'SUCCESS'}],'updatedAt':NOW.isoformat()}]
    responses=[{'program':'gh','args':['repo','view','--json','defaultBranchRef'],'stdout':json.dumps({'defaultBranchRef':{'name':'main'}})},
               {'program':'gh','stdout':json.dumps(raw)}]
    got=call('fetch_prs',command_responses=responses,now_micros=int(NOW.timestamp()*1e6))
    with patch.object(oracle,'_gh',side_effect=[{'defaultBranchRef':{'name':'main'}},raw]):
        expected=oracle.fetch_prs()
    assert got['value']==plain(expected),(got,plain(expected))
    assert all(c['input'] is None for c in got['command_calls'])
    # 0.9.87 (#3829): one PR beyond the open list, by its actual argv.
    for state in ['OPEN','CLOSED','MERGED',None]:
        record=None if state is None else {**raw[0],'state':state}
        responses=[{'program':'gh','stdout':json.dumps(record)}]
        got=call('fetch_pr',number=1,base='main',repo='owner/repo',command_responses=responses,now_micros=int(NOW.timestamp()*1e6))
        with patch.object(oracle,'_gh',return_value=record) as gh:expected=oracle.fetch_pr(1,repo='owner/repo',base='main')
        assert got['value']==plain(expected),(got,plain(expected))
        assert got['command_calls'][0]['args']==list(gh.call_args.args),(got,gh.call_args)
        comparisons['fetch_pr']=comparisons.get('fetch_pr',0)+1
    for item in [raw[0],{'number':7,'title':'Minimal PR','headRefName':'patch-1','baseRefName':'main','updatedAt':NOW.isoformat()},
                 {**raw[0],'author':None,'reviewDecision':None,'statusCheckRollup':None},{'title':'No number','updatedAt':NOW.isoformat()}]:
        compare('_parse_pr_dict',item=item,expected_base='main')
    # 0.9.93 (#4534): only the refs/remotes/origin/ prefix is stripped, so a slashed branch survives.
    for data,fallback,repo in [(None,'refs/remotes/origin/develop\n',None),({},'',None),(None,'should not run','other/repo'),(None,'refs/remotes/origin/release/2.x\n',None)]:
        responses=[{'program':'gh','stdout':json.dumps(data)}]
        if repo is None:responses.append({'program':'git','stdout':fallback})
        got=call('_detect_default_branch',repo=repo,command_responses=responses)
        expected=fallback.strip().removeprefix('refs/remotes/origin/') if fallback.startswith('refs/') else 'main'
        assert got['value']==expected,got
        assert len(got['command_calls'])==(2 if repo is None else 1),got
    candidates=[oracle.PRInfo(1,'API','feature','main','alice',False,'','SUCCESS',NOW,expected_base='main'),
                oracle.PRInfo(2,'Docs','docs','main','bob',False,'','SUCCESS',NOW,expected_base='main')]
    for backend in ['claude','kimi','openai','gemini','ollama','claude-cli']:
        chunks=['#1 — Review API.\n','#2 — Review docs.'];model='fixture-model'
        reply=call('triage_with_opus',prs=candidates,base='main',now_micros=int(NOW.timestamp()*1e6),triage={'backend':backend,'model':model,'chunks':chunks})
        out=io.StringIO();error=io.StringIO()
        stream=MagicMock();stream.__enter__.return_value=stream
        if backend=='claude':stream.text_stream=chunks;client=MagicMock();client.messages.stream.return_value=stream;target='anthropic.Anthropic'
        elif backend!='claude-cli':
            stream.__iter__.return_value=iter([types.SimpleNamespace(choices=[types.SimpleNamespace(delta=types.SimpleNamespace(content=s))]) for s in chunks])
            client=MagicMock();client.chat.completions.create.return_value=stream;target='openai.OpenAI'
        if backend=='claude-cli':
            transport=patch('subprocess.run',return_value=types.SimpleNamespace(returncode=0,stdout=json.dumps({'result':''.join(chunks)}),stderr=''))
        else:transport=patch(target,return_value=client)
        with patch.object(oracle,'_resolve_triage_backend',return_value=(backend,model)),patch.object(llm,'_get_backend_api_key',return_value='fixture-key'),transport,contextlib.redirect_stdout(out),contextlib.redirect_stderr(error):
            oracle.triage_with_opus(candidates,'main')
        assert reply['value']=={'stdout':out.getvalue(),'stderr':error.getvalue(),'code':0},reply
        actual_prompt=reply['triage_calls'][0]['prompt']
        assert all(f'PR #{p.number}' in actual_prompt for p in candidates)
        if backend=='claude':expected_prompt=client.messages.stream.call_args.kwargs['messages'][0]['content']
        elif backend!='claude-cli':expected_prompt=client.chat.completions.create.call_args.kwargs['messages'][0]['content']
        else:expected_prompt=actual_prompt
        assert actual_prompt==expected_prompt,(actual_prompt,expected_prompt)
    # Real subprocesses and streaming HTTP are exercised separately, using only loopback/fake CLIs.
    from check_prs_execution import check_execution
    execution=check_execution(native,raw,plain(candidates),int(NOW.timestamp()*1e6))
    native.receipt('graphify/prs.py',['src/prs.rs','src/prs_io.rs','src/prs_render.rs','src/prs_triage.rs','src/prs_api.rs','src/assets/prs/help.txt'],
        upstream_tests={'passed':reports.passed,'skipped':reports.skipped,'deselected':reports.deselected},comparisons=comparisons,native_execution=execution,
        adaptations=['Native typed timestamps preserve UTC age calculations. IO is provided by native bounded subprocesses, not Python subprocess calls.',
                     'The PR module keeps GitHub queries read-only. Original Python subprocess-mocking tests are replaced by independent actual fake-CLI execution and explicit command receipts.',
                     'Triage supports the upstream streaming providers and separately installed official Claude CLI; additional configured backends use the existing qualified native model adapter instead of silently producing no result.',
                     'The typed command entry point is implemented; registration in the full native application is tracked under the remaining CLI files.'])
finally:
    for name,fn in saved.items():setattr(reference,name,fn)
    native.close()
