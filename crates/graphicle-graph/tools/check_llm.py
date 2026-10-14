#!/usr/bin/env python3
"""Native llm.py compared with unchanged Python contracts and local fixtures."""
import ast,base64,copy,contextlib,dataclasses,inspect,io,json,os,random,socket,sys,tempfile,threading,types,subprocess
from pathlib import Path
from unittest.mock import patch,MagicMock
from http.server import ThreadingHTTPServer,BaseHTTPRequestHandler
from parity import Native,SOURCE
from naming import to_native,to_upstream
from upstream_fixes import oracle_source
import pytest,networkx as nx
import graphify.llm as reference
from graphify.file_slice import FileSlice

oracle=types.ModuleType('reference_llm');oracle.__package__='graphify';sys.modules[oracle.__name__]=oracle
with patch.dict(os.environ,{},clear=True):
    exec(compile(oracle_source('graphify/llm.py'),'<pinned llm.py with reviewed fixes>','exec'),oracle.__dict__)
native=Native('llm');rng=random.Random(72001);counts={};saved={}
BASE=copy.deepcopy(oracle.BACKENDS)
KEYS={key:'offline-fixture-key' for conf in BASE.values() for key in conf.get('env_keys',[conf.get('env_key')]) if key}
KEYS['AZURE_OPENAI_ENDPOINT']='https://fixture.openai.azure.com'
KEYS['PATH']=''
def encode(v):
    if isinstance(v,Path):return str(v)
    if dataclasses.is_dataclass(v):return {k:encode(x) for k,x in dataclasses.asdict(v).items()}
    if isinstance(v,bytes):return list(v)
    if isinstance(v,dict):return {str(k):encode(x) for k,x in v.items()}
    if isinstance(v,(list,tuple,set)):return [encode(x) for x in v]
    return v
def raw(function,**kwargs):
    native.use_working_directory(os.getcwd());kwargs.setdefault('env',{})
    # Provider tables name GRAPHIFY_* model settings; the engine reads GRAPHICLE_*.
    if 'backends' in kwargs:kwargs['backends']=to_native(encode(kwargs['backends']))
    return to_upstream(native.result('llm',function=function,**encode(kwargs)))
def call(function,**kwargs):
    result=raw(function,**kwargs)
    if result.get('error'):raise RuntimeError(result['error'])
    return result['value']
def compare(function,expected,**kwargs):
    actual=call(function,**kwargs);expected=encode(expected)
    counts[function]=counts.get(function,0)+1
    if actual!=expected:native.fail(function=function,inputs=encode(kwargs),expected=expected,actual=actual)
    return actual
def quiet(f,*args,**kwargs):
    with contextlib.redirect_stderr(io.StringIO()),contextlib.redirect_stdout(io.StringIO()):return f(*args,**kwargs)
def graph_json(g):return nx.node_link_data(g,edges='edges')
def unit_decode(v):return FileSlice(**{**v,'path':Path(v['path'])}) if isinstance(v,dict) else Path(v)

PURE=[
    '_parse_llm_json','_sanitize_fragment','_balanced_object','_json_object_candidates','_json_fragment_candidates',
    '_response_is_hollow','_mark_hollow','_neutralise_injection_sentinels','_wrap_untrusted','_label_identifiers',
    '_extraction_system','_is_vision_image','_partition_semantic_files','_build_image_refs','_strip_pixels',
    '_image_notes','_with_image_notes','_anthropic_content','_openai_content','_bedrock_content',
    '_resolve_ollama_base_url','_resolve_max_tokens','_resolve_max_retries','_resolve_max_retry_depth','_resolve_api_timeout',
    '_model_requires_default_temperature','_resolve_temperature','_backend_supports_vision','_backend_env_keys',
    '_format_backend_env_keys','_default_model_for_backend','backend_detection_env_vars','_thinking_disabled_via_env',
    '_envelope_after_preamble','_claude_cli_envelope','_claude_cli_error','_parse_label_response',
    '_resolve_ollama_seed','_stamp_semantic_origin',
]
def wrapper(name,original):
    signature=inspect.signature(original)
    def invoke(*args,**kwargs):
        b=signature.bind(*args,**kwargs);b.apply_defaults();args=dict(b.arguments)
        args['env']={k:v for k,v in os.environ.items() if k.startswith(('GRAPHIFY_','OLLAMA_','OPENAI_','ANTHROPIC_','AWS_','GEMINI_','GOOGLE_','MOONSHOT_','DEEPSEEK_','AZURE_'))}
        args['backends']=reference.BACKENDS
        result=raw(name,**args)
        for line in result.get('stderr',[]):print(line,file=sys.stderr)
        value=result['value']
        if name=='_sanitize_fragment':b.arguments['parsed'].clear();b.arguments['parsed'].update(value);value=b.arguments['parsed']
        if name in ('_mark_hollow','_stamp_semantic_origin'):b.arguments['result'].clear();b.arguments['result'].update(value);value=b.arguments['result']
        if name in ('_build_image_refs','_strip_pixels'):value=[reference._ImageRef(Path(x['path']),x['rel'],x['media_type'],None if x['raw'] is None else bytes(x['raw'])) for x in value]
        if name=='_partition_semantic_files':value=([unit_decode(u) for u in value[0]],[Path(p) for p in value[1]])
        if name=='_bedrock_content':
            for item in value:
                if 'image' in item:item['image']['source']['bytes']=bytes(item['image']['source']['bytes'])
        if name=='backend_detection_env_vars':value=tuple(value)
        if name=='_parse_label_response':value={int(k):v for k,v in value.items()}
        return value
    return invoke
class Reports:
    passed=0;skipped=0;excluded=0;excluded_by_file={}
    def pytest_collection_modifyitems(self,config,items):
        chosen=[];other=[]
        for item in items:
            source=inspect.getsource(item.obj)
            calls=any(name in source for name in PURE)
            excluded=any(s in source for s in ('_call_','extract_files_direct','extract_corpus_parallel','_MAX_IMAGE_BYTES','_file_to_text','_TOKENIZER','importlib.reload','_PDF_ESTIMATE_CACHE'))
            (chosen if calls and not excluded else other).append(item)
        items[:]=chosen;self.excluded=len(other);config.hook.pytest_deselected(items=other)
        for item in other:
            name=Path(str(item.path)).name;self.excluded_by_file[name]=self.excluded_by_file.get(name,0)+1
    def pytest_runtest_logreport(self,report):
        if report.when=='call' and report.passed:self.passed+=1
        if report.skipped:self.skipped+=1
reports=Reports()

def check_configuration():
    compare('configuration',{'backends':BASE,'detection_env_vars':oracle.backend_detection_env_vars()})
    hosts=['localhost','LOCALHOST','llm.local:','127.0.0.1','0.0.0.0',':11434','11434','  host.local  ','https://host:443/proxy/','user:p@HOST/thing','[::1]','[::1]:11434','http://[::1]/foo','http://HOST','ftp://host','bad:port','http://localhost:80/v1/','http://localhost/?x=1','']
    for host in hosts:
        env={'OLLAMA_HOST':host}
        with patch.dict(os.environ,env,clear=True):expected=oracle._resolve_ollama_base_url('fallback')
        compare('_resolve_ollama_base_url',expected,default='fallback',env=env)
    values=['','0','1','-1','3','3.0','+15',' 2 ','bad','1e3','١٢','2_000','2__3','_2','1_','0008']
    for key,name,default in [('GRAPHIFY_MAX_OUTPUT_TOKENS','_resolve_max_tokens',8192),('GRAPHIFY_MAX_RETRIES','_resolve_max_retries',6),('GRAPHIFY_MAX_RETRY_DEPTH','_resolve_max_retry_depth',3),('GRAPHIFY_API_TIMEOUT','_resolve_api_timeout',600.0)]:
        for value in values:
            with patch.dict(os.environ,{key:value},clear=True):expected=getattr(oracle,name)(default)
            compare(name,expected,default=default,env={key:value})
    for model in ['o1','o1x','o3-mini','o4','GATEWAY/OPENAI/GPT-5-mini','gpt-5','gpt4','claude','gpt-5.2','foo/o4-mini','gpt-50']:
        compare('_model_requires_default_temperature',oracle._model_requires_default_temperature(model),model=model)
        for value in ['','0','.2','none','omit','DEFAULT','bad',' 1.5 ','١.٢','1_000']:
            with patch.dict(os.environ,{'GRAPHIFY_LLM_TEMPERATURE':value},clear=True):expected=quiet(oracle._resolve_temperature,0,model)
            compare('_resolve_temperature',expected,default=0,model=model,env={'GRAPHIFY_LLM_TEMPERATURE':value})
    for backend in BASE:
        compare('_backend_env_keys',oracle._backend_env_keys(backend),backend=backend)
        compare('_format_backend_env_keys',oracle._format_backend_env_keys(backend),backend=backend)
        compare('_default_model_for_backend',oracle._default_model_for_backend(backend),backend=backend)
        for _ in range(20):
            inp,out=rng.randrange(100000),rng.randrange(100000)
            compare('estimate_cost',oracle.estimate_cost(backend,inp,out),backend=backend,input_tokens=inp,output_tokens=out)
    envs=[{},*[{k:'offline'} for k in oracle.backend_detection_env_vars() if k not in ('OLLAMA_BASE_URL','OLLAMA_HOST')],{'OLLAMA_HOST':'localhost'},{'OLLAMA_BASE_URL':'http://localhost:11434/v1'},dict(KEYS)]
    for env in envs:
        with patch.dict(os.environ,env,clear=True):expected=quiet(oracle.detect_backend)
        compare('detect_backend',expected,env=env)

def check_seed_and_origin():
    # 0.9.86 (#4168): the opt-in Ollama seed; invalid values fail with the reference message.
    for value in [None,'','   ','0','  00042  ','9223372036854775807','-1','+1','1.0','1e3','0x10','1 2','\u0661','9223372036854775808']:
        env={} if value is None else {'GRAPHIFY_OLLAMA_SEED':value}
        with patch.dict(os.environ,env,clear=True):
            try:expected,error=oracle._resolve_ollama_seed(),None
            except ValueError as e:expected,error=None,str(e)
        if error is None:compare('_resolve_ollama_seed',expected,env=env);continue
        try:actual=call('_resolve_ollama_seed',env=env)
        except ValueError as e:actual=to_upstream(str(e))
        counts['_resolve_ollama_seed']=counts.get('_resolve_ollama_seed',0)+1
        if actual!=error:native.fail(function='_resolve_ollama_seed',inputs=env,expected_error=error,actual=actual)
    # 0.9.87 (#2843): semantic results are stamped, existing stamps kept.
    items=[{'id':'a'},{'id':'b','_origin':'ast'},{'_origin':None},'x',3,None,[]]
    for _ in range(80):
        result={k:rng.choice([None,'text',{'k':1},[copy.deepcopy(rng.choice(items)) for _ in range(rng.randrange(4))]]) for k in rng.sample(['nodes','edges','hyperedges','extra'],rng.randrange(1,4))}
        compare('_stamp_semantic_origin',oracle._stamp_semantic_origin(copy.deepcopy(result)),result=result)

def check_parser():
    for _ in range(500):
        fragment={k:rng.choice([None,{},'bad',[None,[],{'id':'note_中','nodes':[{'id':'a'},'a',None,{'bad':'x'},[3]]},False],[],[{'id':'a"\\b','label':'UTF8 😀 é'}]]) for k in ('nodes','edges','hyperedges')}
        raw_json=json.dumps(fragment,ensure_ascii=False)
        fence=chr(96)*3
        raw_text=rng.choice([raw_json,'<think> { distractor } </think> '+raw_json,'noise {bad}\n'+fence+'python\n{}\n'+fence+'\n'+fence+'JSON\n'+raw_json+'\n'+fence,'{"nodes": ["a"]} then '+raw_json,'other '+raw_json+' end',raw_json[:rng.randrange(len(raw_json)+1)],'prose '+json.dumps([fragment])])
        compare('_parse_llm_json',quiet(oracle._parse_llm_json,raw_text),raw=raw_text)
        compare('_json_object_candidates',oracle._json_object_candidates(raw_text),text=raw_text)
        compare('_json_fragment_candidates',list(oracle._json_fragment_candidates(raw_text)),text=raw_text)
        sample=rng.choice([raw_json,'',' \n','{}",\"','unrelated'])
        compare('_response_is_hollow',oracle._response_is_hollow(sample,fragment),raw_content=sample,parsed=fragment)
    for raw_text in ['[{"type":"system"},{"type":"result","result":"old"},{"type":"result","result":"new"}]','trace before {"type":"result","result":"ok"}\ntrace after','[[1],4]', '[{"result":"a"},4]','null','plain no-json','[]']:
        try:expected=oracle._claude_cli_envelope(raw_text)
        except Exception:continue
        if isinstance(expected,dict):compare('_claude_cli_envelope',expected,stdout=raw_text)
    for error in [{'is_error':True,'result':'  bad  '},{'is_error':True,'result':[]},{'is_error':1,'result':''},{'is_error':False,'result':'valid'}]:compare('_claude_cli_error',oracle._claude_cli_error(json.dumps(error)),stdout=json.dumps(error))
    for _ in range(250):
        x=rng.choice(['<|im_start|>','<|start_header_id|>','</untrusted_source>','[INST]','<<SYS>>','## System','\n### instructions:\n','<|x'*20,'ordinary'])+rng.choice(['😀',' a\n',' {"x"} ','<|endofprompt|>'])
        compare('_neutralise_injection_sentinels',oracle._neutralise_injection_sentinels(x),text=x)
        compare('_wrap_untrusted',oracle._wrap_untrusted('folder/file.md',x),rel='folder/file.md',content=x)

def check_sources(root):
    paths=[]
    for i in range(60):
        p=root/f'dir{i%3}'/f'file{i}.md';p.parent.mkdir(exist_ok=True)
        text=rng.choice(['words β漢字\n','😀new tokens endoftext ',"func present_name() {}\n"])*rng.randrange(1,1200);p.write_text(text);paths.append(p)
    for i in range(4):
        p=root/f'image{i}.PNG';p.write_bytes(bytes(range(i*80)));paths.append(p)
    outside=root.parent/'outside.txt';outside.write_text('private');link=root/'linked.md';link.symlink_to(outside)
    paths.append(link)
    warnings=0
    for _ in range(60):
        chosen=rng.sample(paths,rng.randrange(1,9));err=io.StringIO()
        with contextlib.redirect_stderr(err),contextlib.redirect_stdout(io.StringIO()):expected=oracle._read_files(chosen,root)
        # Both sides warn once per run about a file past the character cap (#3773).
        actual=raw('_read_files',units=chosen,root=root);counts['_read_files']=counts.get('_read_files',0)+1
        if actual.get('error') or actual['value']!=encode(expected) or actual.get('stderr',[])!=err.getvalue().splitlines():native.fail(function='_read_files',inputs=encode({'units':chosen,'root':root}),expected=encode(expected),expected_stderr=err.getvalue().splitlines(),actual=actual)
        warnings+=sum('characters and was truncated' in line for line in err.getvalue().splitlines())
        for bpe in [False,True]:
            tokenizer=oracle._TOKENIZER;oracle._TOKENIZER=tokenizer if bpe else None
            for p in chosen[:3]:compare('_estimate_file_tokens',oracle._estimate_file_tokens(p),unit=p,use_tokenizer=bpe)
            budget=rng.randrange(200,12000);compare('_pack_chunks_by_tokens',oracle._pack_chunks_by_tokens(chosen,budget),files=chosen,token_budget=budget,use_tokenizer=bpe)
            oracle._TOKENIZER=tokenizer
    assert warnings<=1,warnings;counts['_read_files.truncation_warnings']=warnings
    selected=paths[:3];result={'nodes':[{'id':'not_real','file_type':'code','label':'ImaginarySymbol','source_file':str(selected[0])},{'id':'beta','file_type':'paper','label':'Other','source_file':str(selected[0])}], 'edges':[]}
    expected=copy.deepcopy(result);n=oracle._bind_node_evidence(expected,selected,root);compare('_bind_node_evidence',{'result':expected,'count':n},result=result,text_units=selected,root=root)
    pics=[p for p in paths if oracle._is_vision_image(p)]
    for read_bytes in [False,True]:
        refs=oracle._build_image_refs(pics,root,read_bytes=read_bytes);compare('_build_image_refs',refs,image_files=pics,root=root,read_bytes=read_bytes)
        for name in ['_anthropic_content','_openai_content','_bedrock_content']:
            compare(name,getattr(oracle,name)('look at this',refs),user_message='look at this',refs=refs)
    return paths

def check_adaptive(root):
    files=[root/f'a{i}.md' for i in range(7)]
    for p in files:p.write_text('line of source\n'*80)
    def error_data(error):return {'kind':'timeout' if isinstance(error,subprocess.TimeoutExpired) else 'runtime','message':str(error)}
    for case in range(100):
        chunk=files[:rng.randrange(1,7)]
        if case%7==0:chunk=[FileSlice(files[0],0,700,0,1)]
        depth=rng.randrange(4);max_depth=rng.randrange(4);records=[];sleep=[];count=[0]
        def extract(units,**kwargs):
            i=count[0];count[0]+=1
            mode=rng.choice(['ok','ok','length','hollow','context','timeout','error']) if i<12 else 'ok'
            if mode in ('context','timeout','error'):
                error=subprocess.TimeoutExpired(['fixture'],1) if mode=='timeout' else RuntimeError('maximum context limit exceeded' if mode=='context' else 'access denied')
                records.append({'files':encode(units),'error':error_data(error)});raise error
            result={'nodes':([] if mode=='hollow' else [{'id':f'n{i}','source_file':str(oracle.unit_path(units[0]))}]),'edges':[],'hyperedges':[],'input_tokens':7,'output_tokens':8,'model':'fixture','finish_reason':'stop' if mode=='ok' else mode}
            records.append({'files':encode(units),'result':copy.deepcopy(result)});return result
        old_extract,old_time=oracle.extract_files_direct,oracle.time
        oracle.extract_files_direct=extract;oracle.time=types.SimpleNamespace(sleep=lambda n:sleep.append(n))
        expected_error=None
        try:expected=quiet(oracle._extract_with_adaptive_retry,chunk,'openai',None,'fixture',root,max_depth,depth)
        except Exception as e:expected_error=error_data(e);expected=None
        finally:oracle.extract_files_direct=old_extract;oracle.time=old_time
        actual=raw('adaptive_results',chunk=chunk,backend='openai',model='fixture',root=root,max_depth=max_depth,depth=depth,results=records)
        counts['adaptive']=counts.get('adaptive',0)+1
        if actual['value']!=encode(expected) or actual.get('error')!=expected_error or actual['sleeps']!=sleep or actual['calls']!=[r['files'] for r in records]:native.fail(function='adaptive',case=case,records=records,expected=encode(expected),error=expected_error,sleeps=sleep,actual=actual)

def run():
    for name in PURE:
        original=getattr(reference,name);saved[name]=original;setattr(reference,name,wrapper(name,original))
    names=['test_llm_parser.py','test_llm_parser_reasoning.py','test_semantic_fragment_sanitize.py','test_llm_backends.py','test_image_vision.py','test_ollama.py','test_semantic_origin_stamp.py',
           # 0.9.86-0.9.88: their cases that only drive mocked Python SDKs or the
           # Claude CLI through _call_* are deselected like the rest; check_llm_execution
           # compares those contracts natively (null token counts, CLI model env).
           'test_claude_cli_backend.py','test_llm_null_token_counts.py',
           # First run here on 14 October 2026 (their cases that reach the pure helpers).
           'test_injection_sentinel_coverage.py','test_evidence_binding.py']
    status=pytest.main(['-q','-p','no:cacheprovider',*[str(SOURCE/'tests'/n) for n in names]],plugins=[reports]);assert status==0,status
    for name,original in saved.items():setattr(reference,name,original)
    check_configuration();check_seed_and_origin();check_parser()
    with tempfile.TemporaryDirectory() as temp:
        root=Path(temp)/'corpus';root.mkdir();check_sources(root);check_adaptive(root)
    import check_llm_execution
    check_llm_execution.run(sys.modules[__name__])
    files=sorted(str(p.relative_to(Path(__file__).parents[1])) for p in (Path(__file__).parents[1]/'src').glob('llm*.rs'))
    files+=['src/process.rs']+sorted(str(p.relative_to(Path(__file__).parents[1])) for p in (Path(__file__).parents[1]/'src/assets/llm').iterdir())
    native.receipt('graphify/llm.py',files,original_tests={'files':['tests/'+n for n in names],'passed':reports.passed,'skipped':reports.skipped,'deselected_python_sdk_cli_and_limit_mocks':reports.excluded,'deselected_by_file':reports.excluded_by_file},comparisons=counts,native_provider_transports=['OpenAI-compatible HTTP','Anthropic HTTP','Azure HTTP','AWS SDK Converse with SigV4','official Claude CLI subprocess'],live_cloud_requests=0,adaptations=['Python provider SDKs are replaced by native HTTP and the AWS Rust SDK. Python-package installation hints become linked-feature availability; no Python runtime fallback.','cl100k tokenization is bundled; the original optional character estimator remains separately available. Existing native PDF whitespace qualifications apply.','The official Claude CLI remains an external prerequisite. Native requests use bounded total HTTP deadlines and finite timeout values. SDK-specific exception names and OS error text differ; typed timeout/context recovery is retained.','Bedrock uses the native SDK credential/profile chain, adaptive retries and signing. Only loopback protocol fixtures were exercised, not production accounts or every credential source.','Standard JSON values and valid UTF-8 are required. The native provider URL parser rejects malformed HTTP authorities earlier than urllib.','Reported adaptive-extraction token totals follow upstream and omit discarded attempts; they are not a complete billing ledger. Labeling usage includes failed parsing attempts.'])
    print(json.dumps({'phase':'all','original_tests':reports.passed,'skipped':reports.skipped,'excluded':reports.excluded,'comparisons':counts,'native_calls':native.calls}))
if __name__=='__main__':
    try:run()
    finally:
        for name,original in saved.items():setattr(reference,name,original)
        native.close()
