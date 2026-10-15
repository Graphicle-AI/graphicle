"""Provider, checkpoint, labeling and actual local transport qualification."""
import copy,contextlib,dataclasses,io,json,os,random,sys,tempfile,threading,time,types
from pathlib import Path
from unittest.mock import patch
from http.server import ThreadingHTTPServer,BaseHTTPRequestHandler
import networkx as nx

def install(h):
    globals().update({k:getattr(h,k) for k in ('native','oracle','call','raw','compare','quiet','encode','BASE','KEYS','counts','graph_json','rng')})

def obj(value):
    if isinstance(value,dict):return types.SimpleNamespace(**{k:obj(v) for k,v in value.items()})
    if isinstance(value,list):return [obj(x) for x in value]
    return value

def response(backend,answer,inp=23,out=31,finish='stop',null_usage=False):
    text=json.dumps(answer) if not isinstance(answer,str) else answer
    # 0.9.86 (#4325): gateways that report null token counts.
    if null_usage:inp=out=None
    if backend=='claude':
        return {'content':[{'type':'thinking','thinking':'untrusted reasoning'}, {'type':'text','text':text}],'usage':{'input_tokens':inp,'output_tokens':out},'stop_reason':'max_tokens' if finish=='length' else 'end_turn'}
    if backend=='bedrock':
        return {'output':{'message':{'role':'assistant','content':[{'reasoningContent':{'reasoningText':{'text':'reasoning','signature':'fixture'}}},{'text':text}]}},'usage':{'inputTokens':inp,'outputTokens':out,'totalTokens':None if null_usage else inp+out},'stopReason':'max_tokens' if finish=='length' else 'end_turn','metrics':{'latencyMs':1}}
    if backend=='ollama-native':
        # 0.9.91 (#4413): Ollama's native /api/chat reply.
        return {'message':{'role':'assistant','content':text},'prompt_eval_count':inp,'eval_count':out,'done_reason':'length' if finish=='length' else 'stop'}
    if backend=='claude-cli':
        return {'type':'result','result':text,'usage':{'input_tokens':inp-5,'cache_read_input_tokens':3,'cache_creation_input_tokens':2,'output_tokens':out},'modelUsage':{'fixture-cli-model':{}},'stop_reason':'max_tokens' if finish=='length' else 'end_turn'}
    return {'choices':[{'message':{'role':'assistant','content':text},'finish_reason':finish}], 'usage':{'prompt_tokens':inp,'completion_tokens':out}}

def python_provider(backend,env,backends,answer,*,plain,model,root,files,prompt,null_usage=False):
    import openai,anthropic,boto3,botocore.config
    # 0.9.91 (#4413): Ollama goes to /api/chat through the OpenAI client's post()
    # unless the provider sets its own extra_body.
    native_chat=backend=='ollama' and backends[backend].get('extra_body') is None
    captured={};resp=response('ollama-native' if native_chat else backend,answer,null_usage=null_usage)
    def create(**kwargs):captured['body']=kwargs;return obj(resp)
    def post(path,*,body,cast_to):captured['url']=path;captured['body']=body;return copy.deepcopy(resp)
    def factory(**kwargs):
        captured['client']=kwargs
        return types.SimpleNamespace(chat=types.SimpleNamespace(completions=types.SimpleNamespace(create=create)),messages=types.SimpleNamespace(create=create),post=post)
    def session(**kwargs):
        captured['session']=kwargs
        def client(*args,**kwargs):
            captured['aws_client']=kwargs
            def converse(**kwargs):captured['body']=kwargs;return resp
            return types.SimpleNamespace(converse=converse)
        return types.SimpleNamespace(client=client)
    def cli_run(argv,**kwargs):
        if '--help' in argv:return types.SimpleNamespace(stdout='--json-schema',stderr='',returncode=0)
        captured['argv']=argv;captured['stdin']=kwargs['input']
        return types.SimpleNamespace(stdout=json.dumps(resp),stderr='',returncode=0)
    before=oracle.BACKENDS
    oracle.BACKENDS=backends
    try:
        with patch.dict(os.environ,env,clear=True),patch.object(openai,'OpenAI',factory),patch.object(openai,'AzureOpenAI',factory),patch.object(anthropic,'Anthropic',factory),patch.object(boto3,'Session',session),patch('shutil.which',return_value='/fixture/claude'),patch('subprocess.run',side_effect=cli_run):
            if plain:
                usage={}
                expected=quiet(oracle._call_llm,prompt,backend=backend,model=model,max_tokens=777,usage_out=usage)
            else:
                usage=None
                expected=quiet(oracle.extract_files_direct,files,backend=backend,model=model,root=root)
    finally:oracle.BACKENDS=before
    return expected,usage,captured,resp

def check_providers(root):
    document=root/'provider.md';document.write_text('The billing system uses PaymentService and an idempotency table.\n')
    image=root/'provider.png';image.write_bytes(b'\x89PNG\r\nfixture-only')
    answer={'nodes':[{'id':'payments','label':'PaymentService','source_file':'provider.md','file_type':'code'}],'edges':[],'hyperedges':[]}
    for backend in BASE:
        for plain in [False,True]:
            # 4: the Claude CLI model setting without an explicit model (0.9.86,
            # plain and extraction calls); 5: null token counts (#4325).
            for variant in range(6 if backend!='claude-cli' else 5):
                env=dict(KEYS);backends=copy.deepcopy(BASE)
                model=None if variant in (0,4,5) else 'gateway/gpt-5' if variant==1 else 'fixture-model'
                if variant==4:env.update({'GRAPHIFY_CLAUDE_CLI_MODEL':'haiku'})
                if variant==2:env.update({'GRAPHIFY_LLM_TEMPERATURE':'.25','GRAPHIFY_MAX_OUTPUT_TOKENS':'2222','GRAPHIFY_DISABLE_THINKING':'true','GRAPHIFY_API_TIMEOUT':'11','GRAPHIFY_MAX_RETRIES':'2'})
                if variant==3:
                    backends[backend]['extra_body']={'chat_template_kwargs':{'enable_thinking':False}}
                    env.update({'GRAPHIFY_CLAUDE_CLI_MODEL':'haiku','GRAPHIFY_OLLAMA_VISION':'1','GRAPHIFY_OLLAMA_NUM_CTX':'4444'})
                expected,usage,captured,resp=python_provider(backend,env,backends,answer if not plain else 'Concise result',plain=plain,model=model,root=root,files=[document,image],prompt='Name this cluster',null_usage=variant==5)
                if plain:
                    actual=raw('_call_llm',prompt='Name this cluster',backend=backend,model=model,max_tokens=777,env=env,backends=backends,responses=[resp])
                else:
                    actual=raw('extract_files_direct',files=[document,image],backend=backend,model=model,root=root,env=env,backends=backends,responses=[resp])
                counts['provider_variants']=counts.get('provider_variants',0)+1
                if actual.get('error') or actual['value']!=encode(expected) or (usage is not None and actual['usage']!=usage):native.fail(function='provider_result',backend=backend,plain=plain,variant=variant,expected=expected,usage=usage,actual=actual)
                sent=actual['calls'][0]
                if backend=='claude-cli':
                    argv=captured['argv'][1:]
                    if '--json-schema' in argv:
                        i=argv.index('--json-schema');assert argv[i+1]==(Path(__file__).parents[1]/'src/assets/llm/extraction_schema.json').read_text();argv=argv[:i]+argv[i+2:]
                    if sent['cli_args']!=argv or sent['cli_input']!=captured['stdin']:native.fail(function='cli_plan',backend=backend,expected=captured,actual=sent)
                else:
                    body=copy.deepcopy(captured['body']);body.update(body.pop('extra_body',{}))
                    if encode(body)!=sent['body']:native.fail(function='provider_request',backend=backend,plain=plain,variant=variant,expected=encode(body),actual=sent)
                    if 'url' in captured:
                        if sent.get('url')!=captured['url']:native.fail(function='ollama_native_url',plain=plain,variant=variant,expected=captured['url'],actual=sent)
                        counts['ollama_native_requests']=counts.get('ollama_native_requests',0)+1
                    if backend=='bedrock':
                        config=captured['aws_client']['config']
                        if sent['timeout']!=config.read_timeout or sent['retries']!=config.retries['max_attempts']:native.fail(function='bedrock_configuration',actual=sent)
                    elif sent['timeout']!=captured['client']['timeout'] or sent['retries']!=captured['client']['max_retries']:native.fail(function='client_configuration',expected=encode(captured['client']),actual=sent)

def check_labels():
    for case in range(80):
        graph=nx.Graph();communities={};gods=[]
        for cid in range(rng.randrange(1,10)):
            members=[]
            for i in range(rng.randrange(0,10)):
                nid=f'n{cid}_{i}';members.append(nid)
                if rng.randrange(7):graph.add_node(nid,label=rng.choice(['Order Manager','Payments','(   )','😀 Gateway','x'*90,nid]))
                if rng.randrange(5)==0:gods.append({'id':nid})
            communities[cid]=members
        cap=rng.choice([None,0,1,5,-1]);top=rng.choice([0,1,3,12])
        expected=oracle._community_label_lines(graph,communities,gods,cap,top)
        compare('_community_label_lines',expected,G=graph_json(graph),communities=communities,gods=gods,max_communities=cap,top_k=top)
        reqs=[];used={'input':0,'output':0};before=oracle._call_llm
        def mocked(prompt,*,backend,model=None,max_tokens=200,usage_out=None):
            cids=[int(line.split(':')[0]) for line in prompt.split('\n\n')[-1].splitlines()]
            labels={cid:f'Cluster {cid}' for cid in cids if cid%3!=2};text=json.dumps(labels)
            if case%4==0:text=text[:-1]
            reqs.append({'response':response('openai',text)})
            if usage_out is not None:usage_out['input']=usage_out.get('input',0)+23;usage_out['output']=usage_out.get('output',0)+31
            return text
        oracle._call_llm=mocked
        opts={'backend':'openai','gods':gods,'max_communities':cap,'top_k':top,'batch_size':3,'max_concurrency':1}
        expected_error=None
        try:expected=quiet(oracle.label_communities,graph,communities,**opts,usage_out=used)
        except ValueError as e:expected=None;expected_error={'kind':'value','message':str(e)}
        finally:oracle._call_llm=before
        actual=raw('label_communities',G=graph_json(graph),communities=communities,**opts,env=KEYS,responses=reqs)
        if actual.get('error')!=expected_error or actual['value']!=encode(expected) or actual['usage']!=used:native.fail(function='labels',case=case,expected=expected,used=used,actual=actual)
        counts['label_runs']=counts.get('label_runs',0)+1
    graph=nx.path_graph(['a','b','c','d']);communities={0:['a'],1:['b'],2:['c'],3:['d']}
    reply=['not json', '{"0":"First","1":"Second"}', '{"2":"Third","3":"Fourth"}']
    actual=raw('label_communities',G=graph_json(graph),communities=communities,backend='openai',batch_size=100,max_concurrency=1,env=KEYS,responses=[response('openai',x) for x in reply])
    assert actual['value']=={'0':'First','1':'Second','2':'Third','3':'Fourth'} and actual['usage']=={'input':69,'output':93} and len(actual['calls'])==3
    counts['label_runs']+=1

def check_corpus(root):
    for concurrent in [1,4]:
        files=[root/f'case{i}.md' for i in range(6)]
        for i,p in enumerate(files):p.write_text(f'case{i} expected source\n')
        foreign=root/'not_dispatched.md';foreign.write_text('Must not be attributed to this file')
        records=[]
        for i,p in enumerate(files):
            nodes=[{'id':f'n{i}','source_file':str(p)}]
            if i==2:nodes.append({'id':'foreign','source_file':str(foreign)})
            records.append({'files':[str(p)],'result':{'nodes':nodes,'edges':[{'source':f'n{i}','target':'foreign'}] if i==2 else [],'hyperedges':[],'input_tokens':2,'output_tokens':3,'model':'fixture','finish_reason':'stop'},'delay_ms':20 if i==0 else 1})
        recorded=[];old=oracle._extract_with_adaptive_retry
        def fake(chunk,*args,**kwargs):
            idx=int(chunk[0].stem[4:]);return copy.deepcopy(records[idx]['result'])
        oracle._extract_with_adaptive_retry=fake
        try:
            with patch.dict(os.environ,{'GRAPHIFY_NO_INCREMENTAL_CACHE':'1'},clear=True):expected=quiet(oracle.extract_corpus_parallel,files,backend='openai',root=root,token_budget=None,chunk_size=1,max_concurrency=concurrent,on_chunk_done=lambda *e:recorded.append(e))
        finally:oracle._extract_with_adaptive_retry=old
        actual=raw('corpus_results',files=files,backend='openai',root=root,token_budget=None,chunk_size=1,max_concurrency=concurrent,results=records,env={'GRAPHIFY_NO_INCREMENTAL_CACHE':'1'})
        if actual['value']!=encode(expected):native.fail(function='corpus',concurrent=concurrent,expected=expected,actual=actual)
        assert sorted(e['index'] for e in actual['events'])==list(range(6))
        assert all(e['result']['nodes'][0]['id']==f"n{e['index']}" for e in actual['events'])
        if concurrent==4:assert actual['events'][0]['index']!=0
        counts['corpus_runs']=counts.get('corpus_runs',0)+1
    file=root/'checkpoint.md';file.write_text('Checkpoint fact is preserved.\n')
    for partial in [False,True]:
        out=root/f'checkpoint-out-{partial}'
        result={'nodes':[{'id':'fact','source_file':str(file),'label':'Checkpoint fact'}],'edges':[],'hyperedges':[]}
        resp=response('openai',result,finish='length' if partial else 'stop')
        actual=raw('extract_corpus_parallel',files=[file],backend='openai',root=root,cache_root=out,token_budget=None,max_retry_depth=0,env=KEYS,responses=[resp])
        assert not actual.get('error') and actual['value']['failed_chunks']==0 and len(actual['events'])==1
        cachefiles=list(out.rglob('*.json'))
        entries=[json.loads(p.read_text()) for p in cachefiles if p.name!='.stat_cache.json' and json.loads(p.read_text()).get('nodes')]
        assert entries,cachefiles
        if partial:assert actual['value']['nodes'][0]['_partial'] is True and all(e.get('partial') for e in entries)
        else:assert all(not e.get('partial') for e in entries)
        # The semantic cache is keyed by the engine's own extraction prompt.
        prompt=native.result('llm',function='_extraction_system',deep=False,env={})['value']
        loaded=native.result('cache',function='load_cached',arguments={'path':str(file),'root':str(root),'cache_root':str(out),'kind':'semantic','prompt':prompt})
        assert not loaded.get('error'),loaded
        assert (loaded['value'] is None)==partial,loaded
        counts['checkpoints']=counts.get('checkpoints',0)+1

def check_http(root):
    logs=[];control={'backend':'openai','status':200,'remaining_failures':0,'delay':0}
    answer={'nodes':[{'id':'http_fixture','source_file':'http.md','label':'fixture'}],'edges':[],'hyperedges':[]}
    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            body=json.loads(self.rfile.read(int(self.headers['content-length'])));logs.append({'path':self.path,'body':body,'headers':dict(self.headers)})
            if control['delay']:time.sleep(control['delay'])
            status=control['status']
            native_chat=self.path.endswith('/api/chat')
            if native_chat and control.get('no_native_chat'):status=404
            if control['remaining_failures']:
                control['remaining_failures']-=1;status=429
            value=response('ollama-native' if native_chat else control['backend'],answer) if status==200 else {'error':{'message':'fixture refusal'},'message':'fixture refusal'}
            if status==404:value={'error':'404 page not found'}
            data=json.dumps(value).encode();self.send_response(status);self.send_header('content-type','application/json');self.send_header('content-length',str(len(data)))
            if status==429:self.send_header('retry-after-ms','1')
            self.end_headers()
            try:self.wfile.write(data)
            except (BrokenPipeError,ConnectionResetError):pass
        def log_message(self,*args):pass
    server=ThreadingHTTPServer(('127.0.0.1',0),Handler);thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
    try:
        endpoint=f'http://127.0.0.1:{server.server_port}'
        for backend in ['openai','claude','azure','bedrock','ollama']:
            control.update(backend=backend,status=200)
            env={**KEYS,'AZURE_OPENAI_ENDPOINT':endpoint,'AWS_ACCESS_KEY_ID':'fixture-key','AWS_SECRET_ACCESS_KEY':'fixture-secret','AWS_REGION':'us-east-1','AWS_EC2_METADATA_DISABLED':'true','AWS_ENDPOINT_URL_BEDROCK_RUNTIME':endpoint,'GRAPHIFY_MAX_RETRIES':'0'}
            backends=copy.deepcopy(BASE);backends[backend]['base_url']=endpoint
            before=len(logs)
            actual=raw('send_plan',backend=backend,user_message='Fixture request',model='fixture-model',env=env,backends=backends,execute=True)
            assert not actual.get('error'),actual
            assert len(logs)==before+1,(backend,logs[before:])
            if backend=='bedrock':assert logs[-1]['path']=='/model/fixture-model/converse' and any(k.lower()=='authorization' for k in logs[-1]['headers'])
            # 0.9.91 (#4413): num_ctx and num_predict reach Ollama through /api/chat.
            if backend=='ollama':assert logs[-1]['path']=='/api/chat' and logs[-1]['body']['options']['num_ctx']>=8192 and logs[-1]['body']['stream'] is False,logs[-1]
            counts['native_http_calls']=counts.get('native_http_calls',0)+1
        # A /v1-only server 404s on /api/chat: one fallback to /v1, then /v1 straight away.
        control.update(backend='ollama',status=200,no_native_chat=True)
        env={**KEYS,'GRAPHIFY_MAX_RETRIES':'0'};backends=copy.deepcopy(BASE);backends['ollama']['base_url']=endpoint+'/v1'
        for expected_paths in [['/api/chat','/v1/chat/completions'],['/v1/chat/completions']]:
            before=len(logs)
            actual=raw('send_plan',backend='ollama',user_message='Fallback fixture',model='fixture-model',env=env,backends=backends,execute=True)
            assert not actual.get('error') and [entry['path'] for entry in logs[before:]]==expected_paths,(actual,logs[before:])
            counts['native_http_calls']+=len(expected_paths)
        control['no_native_chat']=False
        control.update(backend='openai',remaining_failures=1,status=200)
        env={**KEYS,'GRAPHIFY_MAX_RETRIES':'1'};backends=copy.deepcopy(BASE);backends['openai']['base_url']=endpoint
        before=len(logs);actual=raw('send_plan',backend='openai',user_message='Retry fixture',env=env,backends=backends,execute=True)
        assert not actual.get('error') and len(logs)==before+2,actual
        counts['native_http_calls']+=2
        control['status']=400
        actual=raw('send_plan',backend='openai',user_message='Refusal fixture',env=env,backends=backends,execute=True)
        assert actual['error']['kind']=='runtime' and 'HTTP 400' in actual['error']['message']
        counts['native_http_calls']+=1
        control.update(status=200,delay=.15);env.update(GRAPHIFY_MAX_RETRIES='0',GRAPHIFY_API_TIMEOUT='.02')
        actual=raw('send_plan',backend='openai',user_message='Deadline fixture',env=env,backends=backends,execute=True)
        assert actual['error']['kind']=='timeout',actual
        counts['native_http_calls']+=1
    finally:server.shutdown();server.server_close();thread.join()

def check_cli(root):
    folder=root/'fake-cli';folder.mkdir()
    script=folder/'claude'
    script.write_text('#!/bin/sh\nprintf "%s\\n" "$@" >> "$FIXTURE_LOG"\nif [ "$1" = "--help" ]; then printf "%s\\n" "--json-schema"; exit 0; fi\n/bin/cat > "$FIXTURE_INPUT"\n/bin/cat "$FIXTURE_RESPONSE"\nexit "$FIXTURE_EXIT"\n');script.chmod(0o755)
    answer={'nodes':[{'id':'cli','label':'from CLI'}],'edges':[],'hyperedges':[]}
    reply=folder/'reply.json';reply.write_text('MCP diagnostic preamble\n'+json.dumps(response('claude-cli',answer)))
    env={'PATH':str(folder),'FIXTURE_LOG':str(folder/'argv'),'FIXTURE_INPUT':str(folder/'stdin'),'FIXTURE_RESPONSE':str(reply),'FIXTURE_EXIT':'0','GRAPHIFY_API_TIMEOUT':'5'}
    actual=raw('send_plan',backend='claude-cli',user_message='Fixture input 😀',env=env,execute=True)
    assert actual['value']['result']==json.dumps(answer),actual
    assert '--json-schema' in (folder/'argv').read_text() and 'Fixture input 😀' in (folder/'stdin').read_text()
    for exitcode in ['0','1']:
        env['FIXTURE_EXIT']=exitcode;reply.write_text(json.dumps({'is_error':True,'result':'Rate limited'}))
        actual=raw('send_plan',backend='claude-cli',user_message='Fixture input',env=env,execute=True)
        assert actual['error']['kind']=='runtime' and 'Rate limited' in actual['error']['message'],actual
    counts['native_cli_cases']=3

def run(h):
    install(h)
    with tempfile.TemporaryDirectory() as temp:
        root=Path(temp);check_providers(root);check_labels();check_corpus(root);check_http(root);check_cli(root)
