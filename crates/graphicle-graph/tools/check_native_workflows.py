"""End-to-end qualification of the shipped native executable.

Only the explicitly named oracle commands run Python. The native process has
no Python path or interpreter dependency. All mutations use temporary projects,
all HTTP traffic targets loopback, and executable bytes are pinned for the run.
"""
import contextlib, copy, hashlib, http.client, json, os, re, selectors, shutil, signal, socket, subprocess, sys, tempfile, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from parity import Native, ROOT, SOURCE, BINARY, sha
import naming
from naming import NATIVE_OUT, UPSTREAM_OUT, to_upstream

# The native executable runs at its true defaults (.graphicle output, GRAPHICLE_*
# settings, ~/.graphicle-graph configuration); the reference keeps its own names.
native=Native('native-workflows',layout='native');counts={};commands=[]
KEY_NAMES=('OPENAI_API_KEY','ANTHROPIC_API_KEY','GEMINI_API_KEY','GOOGLE_API_KEY','KIMI_API_KEY','DEEPSEEK_API_KEY','OPENROUTER_API_KEY','AZURE_OPENAI_API_KEY','OPENAI_BASE_URL','ANTHROPIC_BASE_URL','GRAPHIFY_BACKEND','GRAPHIFY_MODEL','PYTHONPATH')
def isolated_env(root):
    env={k:v for k,v in os.environ.items() if k not in KEY_NAMES and not k.startswith((naming.UPSTREAM_ENV,naming.NATIVE_ENV))}
    # Both sides get the same settings, each under its own prefix.
    env.update(HOME=str(root/'home'),USERPROFILE=str(root/'home'),RUST_BACKTRACE='0')
    env.update(naming.native_env({'GRAPHIFY_MAX_WORKERS':'1','GRAPHIFY_MAX_RETRIES':'0','GRAPHIFY_NO_INCREMENTAL_CACHE':'0'},'native'))
    return env
def cli(args,cwd,env,*,oracle=False,input=None,ok=True):
    argv=[sys.executable,'-m','graphify'] if oracle else [str(BINARY)]
    child_env={**env,'PYTHONPATH':str(SOURCE)} if oracle else env
    r=subprocess.run(argv+list(map(str,args)),cwd=cwd,env=child_env,input=input,text=True,capture_output=True,timeout=120)
    commands.append({'args':list(map(str,args)),'oracle':oracle,'exit_code':r.returncode,'stdout_sha256':hashlib.sha256(r.stdout.encode()).hexdigest(),'stderr_sha256':hashlib.sha256(r.stderr.encode()).hexdigest()})
    if ok and r.returncode:native.fail(command=list(map(str,args)),oracle=oracle,stdout=r.stdout,stderr=r.stderr,returncode=r.returncode)
    if not ok and r.returncode==0:native.fail(command=list(map(str,args)),expected='nonzero exit',stdout=r.stdout,stderr=r.stderr)
    return r
def canonical(value):
    if isinstance(value,dict):
        result={k:canonical(v) for k,v in value.items()}
        for key in ['nodes','edges','links','hyperedges','failed_sources','extracted_sources']:
            if isinstance(result.get(key),list):result[key].sort(key=lambda v:json.dumps(v,sort_keys=True))
        return result
    if isinstance(value,list):return [canonical(v) for v in value]
    return value
def same(name,expected,actual):
    if canonical(expected)!=canonical(actual):native.fail(check=name,expected=expected,actual=actual)
    counts[name]=counts.get(name,0)+1
def graph(path):return json.loads(path.read_text())
def fixture(root):
    root.mkdir(parents=True,exist_ok=True)
    (root/'service.py').write_text('from helpers import normalize\n\ndef handle(value):\n    return normalize(value)\n')
    (root/'helpers.py').write_text('def normalize(value):\n    return value.strip()\n')
    (root/'client.ts').write_text('export function request(x: string) { return render(x); }\nfunction render(x: string) { return x; }\n')
    (root/'README.md').write_text('# Service architecture\nThe handle function delegates normalization to helpers.\n')

def extraction(root,env):
    project=root/'corpus';fixture(project)
    nat=root/'native-output';ref=root/'oracle-output';nat.mkdir();ref.mkdir()
    gp=nat/NATIVE_OUT/'graph.json';rp=ref/UPSTREAM_OUT/'graph.json'
    def both(extra=()):
        cli(['extract','.', '--code-only','--no-cluster','--out',ref,'--max-workers','1',*extra],project,env,oracle=True)
        cli(['extract','.', '--code-only','--no-cluster','--out',nat,'--max-workers','1',*extra],project,env)
        same('raw_cli_graph',graph(rp),graph(gp))
    both();before=gp.read_bytes()
    # Graph content is identical: the native file under .graphicle holds the same
    # JSON document as the reference's graphify-out file, in the same list order
    # (one edge serializes `context` at another key position, as before the
    # rename), and the native wrote no reference-named output folder.
    if graph(gp)!=graph(rp):native.fail(check='graph_document',expected=rp.read_text(),actual=gp.read_text())
    assert not (nat/UPSTREAM_OUT).exists() and not (ref/NATIVE_OUT).exists()
    counts['graph_document_identical']=1
    both();assert gp.read_bytes()==before
    (project/'service.py').write_text('from helpers import normalize\n\ndef handle(value):\n    return normalize(value)\n\ndef status():\n    return "ok"\n')
    both();assert any(n.get('label')=='status()' for n in graph(gp)['nodes'])
    (project/'client.ts').unlink();both();assert all(n.get('source_file')!='client.ts' for n in graph(gp)['nodes'])
    (project/'helpers.py').write_text('def normalize(value):\n    return value.strip()\n\ndef unused():\n    return 0\n');both()
    for name in ('.graphifyignore','.graphicleignore'):(project/name).write_text('helpers.py\n')
    both();assert all(n.get('source_file')!='helpers.py' for n in graph(gp)['nodes'])
    # Files omitted by failed extraction must remain eligible for retry.
    broken=root/'failure-corpus';broken.mkdir();empty=broken/'empty.json';empty.write_text('{}');missing=broken/'missing.py'
    failed=native.result('extract',action='file',path=str(missing),root=str(broken));assert failed.get('error')
    for per,wanted in [({'nodes':[],'edges':[],'error':'fixture failure'},True),({'nodes':[],'edges':[]},True),({'nodes':[],'edges':[],'skipped':'intentionally empty'},False)]:
        result=native.result('extract',action='assemble',paths=[str(empty)],per_file=[per],root=str(broken),use_cache=False)
        assert (str(empty) in result['failed_sources'])==wanted,result
    f=broken/'retry.py';first=native.result('extract',paths=[str(f)],root=str(broken),parallel=False);assert str(f) in first['failed_sources']
    f.write_text('def recovered():\n    return True\n')
    second=native.result('extract',paths=[str(f)],root=str(broken),parallel=False);assert not second['failed_sources'];assert any(n.get('label')=='recovered()' for n in second['nodes'])
    counts['native_extraction_failure_retry']=6
    # Destructive replacement needs explicit force; malformed graphs are retained.
    original=gp.read_bytes();gp.write_text('{broken graph')
    (project/'service.py').write_text((project/'service.py').read_text()+'\ndef changed_for_repair():\n    return True\n')
    cli(['extract','.', '--code-only','--no-cluster','--out',nat],project,env,ok=False);assert gp.read_text()=='{broken graph';gp.write_bytes(original)
    for flag,value in [('--max-workers','0'),('--api-timeout','nan'),('--token-budget','0'),('--max-concurrency','-1')]:
        cli(['extract','.', '--code-only',flag,value],project,env,ok=False)
    counts['extraction_validation']=5
    # Full native rebuild, unchanged fast path and explicit deletion.
    watch=root/'watched';fixture(watch)
    cli(['update','.', '--no-cluster'],watch,env)
    wg=watch/NATIVE_OUT/'graph.json';cold=graph(wg)
    # Each output folder leaves the corpus before the other side scans it.
    saved=root/'native-watch-baseline';(watch/NATIVE_OUT).rename(saved)
    cli(['update','.', '--no-cluster'],watch,env,oracle=True)
    same('cold_watch_graph',graph(watch/UPSTREAM_OUT/'graph.json'),cold)
    shutil.rmtree(watch/UPSTREAM_OUT);saved.rename(watch/NATIVE_OUT)
    # Like the reference, the first reconciliation removes extraction-only
    # metadata. Subsequent unchanged rebuilds preserve the exact file.
    cli(['update','.', '--no-cluster'],watch,env)
    old=wg.read_bytes();stamp=wg.stat().st_mtime_ns
    cli(['update','.', '--no-cluster'],watch,env);assert wg.read_bytes()==old and wg.stat().st_mtime_ns==stamp
    (watch/'client.ts').unlink();cli(['update','.', '--no-cluster'],watch,env);assert not any(n.get('source_file')=='client.ts' for n in graph(wg)['nodes'])
    counts['native_rebuild_workflow']=4
    if os.name=='posix':
        import fcntl
        source=watch/'service.py';source.write_text(source.read_text()+'\ndef queued():\n    return True\n')
        with (watch/NATIVE_OUT/'.rebuild.lock').open('a+') as lock:
            fcntl.flock(lock,fcntl.LOCK_EX)
            queued=native.result('watch',action='rebuild',path=str(watch),changed_paths=[str(source)],no_cluster=True)
            assert not queued['success'] and any('queued' in m for m in queued['messages'])
        result=native.result('watch',action='rebuild',path=str(watch),changed_paths=[str(source)],no_cluster=True)
        assert result['success'] and any(n.get('label')=='queued()' for n in graph(wg)['nodes'])
        counts['native_lock_contention_recovery']=1
    with server_process(['watch','.', '--debounce','0.05'],watch,env) as proc:
        sel=selectors.DefaultSelector();sel.register(proc.stderr,selectors.EVENT_READ)
        assert sel.select(timeout=5),'watch failed to announce startup';ready=proc.stderr.readline();assert 'Watching' in ready,ready;sel.close()
        # Pure reads do not trigger the polling observer.
        before=wg.stat().st_mtime_ns;(watch/'service.py').read_text();time.sleep(.6);assert wg.stat().st_mtime_ns==before
        source=watch/'service.py';source.write_text(source.read_text()+'\ndef observed():\n    return True\n')
        for _ in range(50):
            if any(n.get('label')=='observed()' for n in graph(wg)['nodes']):break
            time.sleep(.1)
        else:raise AssertionError('Native watcher missed a code change')
        doc=watch/'README.md';doc.write_text(doc.read_text()+'Document changed.\n')
        for _ in range(40):
            if (watch/NATIVE_OUT/'needs_update').exists():break
            time.sleep(.1)
        else:raise AssertionError('Native watcher missed a semantic change')
        counts['native_watcher_events']=3
    # A shared absolute output must identify its actual corpus across callers.
    shared=root/'shared-output';shared_env={**env,'GRAPHICLE_OUT':str(shared.resolve())}
    for name in ['shared-corpus-a','shared-corpus-b']:
        corpus=root/name;fixture(corpus)
        cli(['update','.', '--no-cluster'],corpus,shared_env)
        assert (shared/'.graphicle_root').read_text().strip()==str(corpus.resolve())
        assert (shared/'graph.json').is_file()
        counts['shared_absolute_output_root']=counts.get('shared_absolute_output_root',0)+1
    return project,gp

def navigation(root,project,gp,env):
    # The command boundary enforces platform choices before changing files.
    for arguments in [['install','codex','--platform','claude','--project'],['install','--unknown=value']]:
        cli(arguments,project,env,ok=False)
    cli(['install','codex','--platform','codex','--project'],project,env)
    cli(['uninstall','codex','--project'],project,env)
    counts['native_install_cli_validation']=4
    # Cluster/report and every local export operate on a real native extraction.
    cli(['cluster-only',project,'--graph',gp,'--no-label','--no-viz'],project,env)
    assert gp.with_name('GRAPH_REPORT.md').exists();assert all('community' in n for n in graph(gp)['nodes'])
    def stable(text):
        lines=text.splitlines();return [s for s in lines if not s.startswith('EDGE ')]+sorted(s for s in lines if s.startswith('EDGE '))
    for args in [['query','handle'],['query','handle','--dfs'],['query','handle','--context','call'],['query','handle','--seed','helpers.py'],['path','handle','status'],['explain','handle'],['explain','handle','--all'],['affected','handle'],['god-nodes','--json']]:
        args += ['--graph',gp]
        a=cli(args,project,env,oracle=True);b=cli(args,project,env)
        same('native_cli_navigation',stable(a.stdout),stable(b.stdout))
    # 0.9.86 (#4320): lookups exit 3 for an ambiguous name and 4 for no match.
    for args in [['explain','zzqqxx'],['path','handle','zzqqxx'],['query','zzqqxx']]:
        args += ['--graph',gp]
        a=cli(args,project,env,oracle=True,ok=False);b=cli(args,project,env,ok=False)
        if a.returncode!=4 or b.returncode!=4:native.fail(command=list(map(str,args)),oracle_exit=a.returncode,native_exit=b.returncode)
        same('native_cli_lookup_refusals',stable(a.stdout),stable(b.stdout))
    for kind in ['html','callflow-html','svg','graphml','obsidian','wiki','neo4j','falkordb']:
        cli(['export',kind,'--graph',gp],project,env)
        counts['native_export_formats']=counts.get('native_export_formats',0)+1
    cli(['tree','--graph',gp],project,env);assert gp.with_name('GRAPH_TREE.html').exists()
    cli(['diagnose','multigraph','--graph',gp,'--json'],project,env)
    cli(['benchmark',gp],project,env)
    memory=root/'memory';reflection=root/'lessons.md'
    for i in range(2):cli(['save-result','--question',f'Where is handle {i}?','--answer','The handle function is in service.py.','--nodes','handle','--outcome','useful','--memory-dir',memory],project,env)
    cli(['reflect','--memory-dir',memory,'--graph',gp,'--out',reflection],project,env);assert reflection.exists() and len(list(memory.glob('*.md')))==2
    for args in [['global','add',gp,'--as','fixture'],['global','list'],['global','path'],['global','remove','fixture'],['provider','list']]:cli(args,project,env)
    provider=['provider','add','fixture','--base-url','https://api.example.com/v1','--default-model','fixture','--env-key','FIXTURE_API_KEY','--pricing-input','1.5','--pricing-output','2.5']
    cli(provider,project,env,oracle=True);expected=graph(root/'home/.graphify/providers.json')
    cli(provider,project,env);same('provider_configuration',expected,to_upstream(graph(root/'home/.graphicle-graph/providers.json'),layout='native'))
    same('provider_show',cli(['provider','show','fixture'],project,env,oracle=True).stdout,to_upstream(cli(['provider','show','fixture'],project,env).stdout,layout='native'))
    cli(['provider','remove','fixture'],project,env)
    cli(['provider','show','fixture'],project,env,ok=False)
    cli([*provider[:2],'openai',*provider[3:]],project,env,ok=False)
    fragment={'nodes':[{'id':'one','label':'One','file_type':'document','source_file':'guide.md'}],'edges':[],'hyperedges':[],'input_tokens':10,'output_tokens':5}
    first=root/'chunk-one.json';second=root/'chunk-two.json';bad=root/'chunk-bad.json'
    first.write_text(json.dumps(fragment));fragment['nodes']=[{'id':'two','label':'Two','file_type':'document','source_file':'guide.md'}];second.write_text(json.dumps(fragment));bad.write_text('{bad')
    cm=root/'chunks.json';cr=root/'chunks-reference.json'
    cli(['merge-chunks',first,second,bad,'--out',cr],project,env,oracle=True)
    cli(['merge-chunks',first,second,bad,'--out',cm],project,env);same('merge_chunks',graph(cr),graph(cm))
    cli(['merge-chunks',bad,'--out',cm],project,env,ok=False);same('invalid_chunks_preserve_output',graph(cr),graph(cm))
    cli(['merge-semantic','--cached',first,'--new',second,'--out',cr],project,env,oracle=True)
    cli(['merge-semantic','--cached',first,'--new',second,'--out',cm],project,env);same('merge_semantic',graph(cr),graph(cm))
    other=root/'other.json';other.write_text(json.dumps({'nodes':[{'id':'extra','label':'ExternalExtra','file_type':'code'}],'links':[],'directed':False,'multigraph':False,'graph':{}}))
    merged=root/'merged.json';cli(['merge-graphs',gp,other,'--out',merged],project,env);assert len(graph(merged)['nodes'])>len(graph(gp)['nodes'])
    ours=root/'ours.json';ours.write_bytes(gp.read_bytes());cli(['merge-driver',gp,ours,other],project,env);assert any(n['id']=='extra' for n in graph(ours)['nodes'])
    for command in ['hook-check','--help','--version']:cli([command],project,env)
    cli(['unknown-native-command'],project,env,ok=False)
    counts['native_cli_other_workflows']=18
    guard=root/'guard';guard.mkdir();src=guard/'indexed.py';src.write_text('x=1\n');out=guard/NATIVE_OUT;out.mkdir();(out/'manifest.json').write_text(json.dumps({'indexed.py':{}}));time.sleep(.01);(out/'graph.json').write_bytes(gp.read_bytes())
    payload={'session_id':'native-session','tool_name':'Read','tool_input':{'file_path':'indexed.py'}}
    first=cli(['hook-guard','read','--strict'],guard,env,input=json.dumps(payload)).stdout
    assert 'deny' in first.lower(),first
    second=cli(['hook-guard','read','--strict'],guard,env,input=json.dumps(payload)).stdout;assert 'deny' not in second.lower() and second
    for command,wanted in [('rg needle .',True),('echo "rg needle"',False),('git status',False)]:
        value=cli(['hook-guard','search'],guard,env,input=json.dumps({'tool_input':{'command':command}})).stdout;assert bool(value)==wanted,(command,value)
    assert cli(['check-update','.'],guard,env).stdout==''
    (out/'needs_update').write_text('1');assert 'Pending non-code' in cli(['check-update','.'],guard,env).stdout
    counts['native_assistant_guard_workflows']=7

@contextlib.contextmanager
def server_process(args,cwd,env):
    p=subprocess.Popen([str(BINARY),*map(str,args)],cwd=cwd,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    try:yield p
    finally:
        if p.poll() is None:p.terminate()
        try:p.communicate(timeout=10)
        except subprocess.TimeoutExpired:p.kill();p.communicate(timeout=5)

def mcp(root,project,gp,env):
    # A deterministic protocol fixture keeps traversal checks independent of
    # clustering choices while the prior tests use the extracted corpus.
    gp=root/'mcp.json';gp.write_text(json.dumps({'directed':True,'multigraph':False,'graph':{},'nodes':[{'id':'a','label':'Alpha','community':0},{'id':'b','label':'Beta','community':0}],'links':[{'source':'a','target':'b','relation':'calls','confidence':'EXTRACTED','_src':'a','_tgt':'b'}]}))
    messages=[{'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2025-03-26','capabilities':{},'clientInfo':{'name':'qualification','version':'1'}}},{'jsonrpc':'2.0','method':'notifications/initialized'}, {'jsonrpc':'2.0','id':2,'method':'tools/list'}, {'jsonrpc':'2.0','id':3,'method':'tools/call','params':{'name':'query_graph','arguments':{'question':'Alpha'}}},{'jsonrpc':'2.0','id':4,'method':'resources/list'},{'jsonrpc':'2.0','id':5,'method':'resources/read','params':{'uri':'graphicle-graph://stats'}},{'jsonrpc':'2.0','id':6,'method':'missing_method'}, {}, {'jsonrpc':'2.0','id':7,'method':'ping'}]
    result=cli(['serve',gp],project,env,input='\n'.join(map(json.dumps,messages))+'\n')
    replies=[json.loads(line) for line in result.stdout.splitlines()]
    assert len(replies)==len(messages)-1 and replies[0]['result']['protocolVersion']=='2025-03-26'
    assert len(replies[1]['result']['tools'])==10 and 'Alpha' in replies[2]['result']['content'][0]['text']
    assert replies[-3]['error']['code']==-32601 and replies[-2]['error']['code']==-32600 and replies[-1]['result']=={}
    counts['stdio_mcp_requests']=len(messages)
    for stateless in [False,True]:
        with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
        args=['serve',gp,'--transport','http','--host','127.0.0.1','--port',str(port),'--api-key','fixture-key','--path','/custom-mcp','--session-timeout','2']
        if stateless:args+=['--stateless','--json-response']
        with server_process(args,project,env) as proc:
            for _ in range(100):
                try:
                    with socket.create_connection(('127.0.0.1',port),timeout=.1):break
                except OSError:
                    if proc.poll() is not None:raise AssertionError(proc.stderr.read())
                    time.sleep(.03)
            else:raise AssertionError('HTTP server did not start')
            def request(message=None,*,headers=None,path='/custom-mcp',method='POST',chunked=False):
                h={'Content-Type':'application/json','Accept':'application/json, text/event-stream','Authorization':'bEaReR fixture-key',**(headers or {})}
                h={k:v for k,v in h.items() if v is not None}
                body=json.dumps(message).encode() if message is not None else b''
                if chunked:body=iter([body[:7],body[7:]])
                c=http.client.HTTPConnection('127.0.0.1',port,timeout=5);c.request(method,path,body=body,headers=h,encode_chunked=chunked);r=c.getresponse();data=r.read().decode();status=r.status;rh={k.lower():v for k,v in r.getheaders()};c.close()
                if data.startswith('event:'):data=next(line[6:] for line in data.splitlines() if line.startswith('data: '))
                return status,rh,json.loads(data) if data else None
            assert request(messages[0],headers={'Authorization':None})[0]==401
            assert request(messages[0],headers={'Authorization':'Bearer wrong'})[0]==401
            assert request(messages[0],path='/missing')[0]==404
            status,h,data=request(messages[0],chunked=True);assert status==200 and 'result' in data,(status,h,data)
            session={} if stateless else {'Mcp-Session-Id':h['mcp-session-id']}
            assert request(messages[1],headers=session)[0]==202
            status,h,data=request(messages[2],headers=session);assert status==200 and len(data['result']['tools'])==10
            assert all('project_path' in t['inputSchema']['properties'] for t in data['result']['tools'])
            # Per-project routing, mtime invalidation, and error recovery.
            alt=root/f'mcp-project-{stateless}';(alt/NATIVE_OUT).mkdir(parents=True);altgraph=alt/NATIVE_OUT/'graph.json';altgraph.write_bytes(gp.read_bytes())
            q={'jsonrpc':'2.0','id':10,'method':'tools/call','params':{'name':'query_graph','arguments':{'question':'Gamma','project_path':str(alt)}}}
            raw=graph(altgraph);raw['nodes'][0]['label']='Gamma';altgraph.write_text(json.dumps(raw))
            status,_,data=request(q,headers=session);assert status==200 and 'Gamma' in data['result']['content'][0]['text'],data
            altgraph.write_text('{malformed');status,_,data=request(q,headers=session);assert 'Error' in data['result']['content'][0]['text']
            altgraph.write_text(json.dumps(raw));assert 'Gamma' in request(q,headers=session)[2]['result']['content'][0]['text']
            if not stateless:
                assert request(method='DELETE',headers=session)[0]==200
                assert request(messages[2],headers=session)[0]==404
                status,h,_=request(messages[0]);assert status==200
                expired={'Mcp-Session-Id':h['mcp-session-id']};time.sleep(2.1)
                assert request(messages[2],headers=expired)[0]==404
            counts['http_mcp_workflows']=counts.get('http_mcp_workflows',0)+13

def semantic(root,env):
    corpus=root/'semantic';corpus.mkdir();doc=corpus/'guide.md';doc.write_text('# Guide\nPayments use an idempotency key and a payment ledger.\n')
    state={'calls':0,'mode':'good','label_requests':[]}
    class Handler(BaseHTTPRequestHandler):
        def log_message(self,*args):pass
        def do_POST(self):
            state['calls']+=1;payload=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            answer={'nodes':[{'id':'payment-ledger','label':'PaymentLedger','file_type':'document','source_file':'guide.md'},{'id':'idempotency','label':'IdempotencyKey','file_type':'document','source_file':'guide.md'}],'edges':[{'source':'payment-ledger','target':'idempotency','relation':'uses','confidence':'INFERRED','source_file':'guide.md'}],'hyperedges':[]}
            if state['mode']=='labels':
                prompt=payload['messages'][-1]['content'];ids=re.findall(r'(?m)^(-?\d+): ',prompt)
                state['label_requests'].append(ids);answer={cid:'Fixture label '+cid for cid in ids}
            if state['mode']=='empty':answer={'nodes':[],'edges':[],'hyperedges':[]}
            if state['mode']=='shrink':answer['nodes']=answer['nodes'][:1];answer['edges']=[]
            data=json.dumps({'choices':[{'message':{'role':'assistant','content':json.dumps(answer)},'finish_reason':'stop'}],'usage':{'prompt_tokens':31,'completion_tokens':17}}).encode()
            self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
    http=ThreadingHTTPServer(('127.0.0.1',0),Handler);thread=threading.Thread(target=http.serve_forever,daemon=True);thread.start()
    try:
        e={**env,'OPENAI_API_KEY':'fixture-only','OPENAI_BASE_URL':f'http://127.0.0.1:{http.server_port}/v1','OPENAI_MODEL':'fixture-model','GRAPHIFY_MAX_RETRIES':'0','GRAPHICLE_MAX_RETRIES':'0'}
        args=['extract','.', '--backend','openai','--model','fixture-model','--no-cluster','--max-concurrency','1']
        nat=root/'semantic-native';ref=root/'semantic-oracle';nat.mkdir();ref.mkdir()
        gp=nat/NATIVE_OUT/'graph.json';rp=ref/UPSTREAM_OUT/'graph.json'
        def both(ok=True):
            cli([*args,'--out',ref],corpus,e,oracle=True,ok=ok)
            cli([*args,'--out',nat],corpus,e,ok=ok)
            same('semantic_cli_graph',graph(rp),graph(gp))
        both();assert any(n['id']=='payment-ledger' for n in graph(gp)['nodes']) and state['calls']>0
        old_calls=state['calls'];before=gp.read_bytes();both();assert state['calls']==old_calls and gp.read_bytes()==before
        # The reference retains the prior graph when a hollow response gives
        # no replacement nodes. It returns success but leaves the file eligible
        # for retry; only a smaller replacement is rejected by the shrink guard.
        doc.write_text(doc.read_text()+'The guide changed.\n');state['mode']='empty';both()
        same('hollow_preserves_nodes',[{**n,'_origin':'semantic'} for n in json.loads(before)['nodes']],graph(gp)['nodes'])
        old_calls=state['calls'];state['mode']='good';both();assert state['calls']>=old_calls+2
        before=gp.read_bytes();doc.write_text(doc.read_text()+'One more change.\n');state['mode']='shrink';both(ok=False);assert gp.read_bytes()==before
        state['mode']='good';old_calls=state['calls']
        # Deliberate repair beyond the pinned release: rejected semantic results must not
        # poison the cache. The original requires --force to escape this loop.
        cli([*args,'--out',nat],corpus,e);assert state['calls']>old_calls
        same('rejected_cache_retry_restores_graph',json.loads(before),graph(gp))
        cli([*args,'--out',ref,'--force'],corpus,e,oracle=True)
        counts['native_semantic_cache_retry']=6
        state['mode']='labels'
        label_root=root/'labels-project';label_dir=label_root/NATIVE_OUT;label_dir.mkdir(parents=True)
        label_graph=label_dir/'graph.json'
        label_graph.write_text(json.dumps({'directed':False,'nodes':[{'id':nid,'label':'Symbol '+nid,'file_type':'code','source_file':nid+'.py'} for nid in ['a','b','c','d']],'links':[{'source':'a','target':'b','relation':'calls','confidence':'EXTRACTED'},{'source':'c','target':'d','relation':'calls','confidence':'EXTRACTED'}]}))
        label_args=['label','.', '--backend','openai','--model','fixture-model','--no-viz']
        cli(label_args,label_root,e)
        label_file=label_dir/'.graphicle_labels.json';labels=graph(label_file)
        assert len(labels)>=2 and all(v.startswith('Fixture label ') for v in labels.values()),labels
        kept,missing=list(labels)[:2];labels[kept]='Retain this reviewed name';labels[missing]='Community '+missing
        label_file.write_text(json.dumps(labels));before_calls=len(state['label_requests'])
        cli([*label_args,'--missing-only'],label_root,e)
        updated=graph(label_file);assert updated[kept]=='Retain this reviewed name' and updated[missing]=='Fixture label '+missing
        assert state['label_requests'][before_calls:]==[[missing]],state['label_requests']
        before_calls=len(state['label_requests']);cli([*label_args,'--missing-only'],label_root,e)
        assert len(state['label_requests'])==before_calls
        counts['native_label_cli_missing_only']=3
    finally:http.shutdown();http.server_close();thread.join(timeout=2)

if __name__=='__main__':
    with tempfile.TemporaryDirectory(prefix='graphify-native-workflows-') as tmp:
        root=Path(tmp);env=isolated_env(root);print('Extraction and native watcher',flush=True);project,gp=extraction(root,env)
        print('CLI navigation and exports',flush=True);navigation(root,project,gp,env)
        print('MCP stdio and HTTP',flush=True);mcp(root,project,gp,env)
        print('Semantic extraction and retry',flush=True);semantic(root,env)
    native.close()
    native.receipt('graphify/__main__.py',['src/main.rs','src/lib.rs']+[str(p.relative_to(ROOT)) for pattern in ['cli*.rs','serve*.rs','watch*.rs'] for p in sorted((ROOT/'src').glob(pattern))],checked=counts,commands=commands,interface_differences=['Native graphicle-graph CLI and explicit --jsonl; no-argument piped JSONL compatibility is preserved for existing embedding clients.','The executable runs at its own defaults: output folder .graphicle (the reference writes graphify-out), configuration ~/.graphicle-graph, GRAPHICLE_* settings, graphicle-graph:// MCP resources, .graphicle_* sidecars. Graph content is compared with the reference output: the fixture graph.json is the same JSON document as the reference\'s (equal after parsing, with the same list order); it is not byte-identical, because one edge serializes `context` at a different key position, as before the rename.','The Python entry point re-execs under PYTHONHASHSEED=0 for clustering commands (#3743/#3780/#3816); native binaries have no per-process hash randomization, so there is nothing to pin.'])
