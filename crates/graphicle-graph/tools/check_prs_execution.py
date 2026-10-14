"""Local command and provider-stream qualification for the native PR module."""
import contextlib,json,os,subprocess,tempfile,threading,time
from pathlib import Path
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from unittest.mock import patch

def check_execution(native,raw,prs,now):
    commands=0;http_calls=[];sse_cases=0
    def call(function,**kw):
        return native.result('prs',function=function,env=kw.pop('env',{}),now_micros=now,**kw)
    with tempfile.TemporaryDirectory(prefix='graphify-prs-') as temporary:
        root=Path(temporary);repo=root/'repo';repo.mkdir();other=root/'worktree'
        def git(*args):
            return subprocess.run(['git','-C',str(repo),*args],capture_output=True,text=True,check=True)
        git('init','-q');git('checkout','-qb','main')
        git('-c','user.name=Graphify Fixture','-c','user.email=fixture@example.invalid','commit','--allow-empty','-qm','fixture')
        git('worktree','add','-qb','feature',str(other))
        result=call('fetch_worktrees',cwd=str(repo),execute=True)['value'];commands+=1
        assert result=={'main':str(repo.resolve()),'feature':str(other.resolve())},result
        git('symbolic-ref','refs/remotes/origin/HEAD','refs/remotes/origin/main')
        graph=root/'graph.json'
        graph.write_text(json.dumps({'nodes':[{'id':'a','source_file':'src/api.rs','community':0,'label':'API'},
                                            {'id':'b','source_file':'src/café.rs','community':1,'label':'Read'}]}))
        fixture=root/'bin';fixture.mkdir()
        listfile=root/'prs.json';listfile.write_text(json.dumps(raw,ensure_ascii=False))
        viewfile=root/'pr-view.json';viewfile.write_text(json.dumps({**raw[0],'number':2,'state':'OPEN','baseRefName':'develop'},ensure_ascii=False))
        branchfile=root/'repo.json';branchfile.write_text(json.dumps({'defaultBranchRef':{'name':'main'}}))
        calls=root/'commands.log';stdin=root/'triage-input.txt';cli_reply=root/'cli.json';cli_reply.write_text(json.dumps({'result':'#1 — Review API. فارسی 🏆'}))
        gh=fixture/'gh';gh.write_text('#!/bin/sh\nprintf "%s\\n" "$@" >> "$COMMAND_LOG"\n/bin/cat >/dev/null\ncase "$1 $2" in\n  "repo view") /bin/cat "$FIXTURE_REPO_JSON" ;;\n  "pr list") /bin/cat "$FIXTURE_PR_JSON" ;;\n  "pr view") /bin/cat "$FIXTURE_PR_VIEW_JSON" ;;\n  "pr diff") printf "src/api.rs\\nsrc/café.rs\\n" ;;\nesac\nexit "$FIXTURE_EXIT"\n');gh.chmod(0o755)
        claude=fixture/'claude';claude.write_text('#!/bin/sh\n/bin/cat > "$TRIAGE_INPUT"\n/bin/cat "$FIXTURE_CLAUDE_JSON"\nexit "$FIXTURE_EXIT"\n');claude.chmod(0o755)
        settings={'PATH':str(fixture)+os.pathsep+os.environ.get('PATH',''),'COMMAND_LOG':str(calls),'FIXTURE_REPO_JSON':str(branchfile),
                  'FIXTURE_PR_JSON':str(listfile),'FIXTURE_PR_VIEW_JSON':str(viewfile),'FIXTURE_CLAUDE_JSON':str(cli_reply),'FIXTURE_EXIT':'0','TRIAGE_INPUT':str(stdin)}
        with patch.dict(os.environ,settings):
            native.cwd=None;native.use_working_directory(root)
            got=call('fetch_prs',repo='owner/repo',execute=True)['value'];commands+=1
            assert got[0]['title']==raw[0]['title'] and got[0]['expected_base']=='main'
            got=call('fetch_pr_files',number=1,repo='owner/repo',execute=True)['value'];commands+=1
            assert got==['src/api.rs','src/café.rs']
            got=call('attach_graph_impact',prs=prs,graph_path=str(graph),execute=True)['value'];commands+=1
            assert all(p['nodes_affected']==2 and p['communities_touched']==[0,1] for p in got['prs'])
            got=call('cmd_prs',argv=['--repo','owner/repo','--base','main','--graph',str(graph),'1'],execute=True)['value'];commands+=1
            assert got['code']==0 and 'Graph impact:' in got['stdout'] and '2 nodes' in got['stdout'],got
            # 0.9.87 (#3829): a PR beyond the open list is fetched, and its impact shown even with the wrong base.
            got=call('cmd_prs',argv=['--repo','owner/repo','--base','main','--graph',str(graph),'2'],execute=True)['value'];commands+=1
            assert got['code']==0 and 'PR #2' in got['stdout'] and 'Graph impact:' in got['stdout'],got
            got=call('triage_with_opus',prs=prs,base='main',execute=True,env={'GRAPHIFY_TRIAGE_BACKEND':'claude-cli'})['value'];commands+=1
            assert got['stderr']=='' and 'Review API. فارسی 🏆' in got['stdout'],got
            assert 'PR #1' in stdin.read_text() and 'PR #2' in stdin.read_text()
            assert 'owner/repo' in calls.read_text()
            branchfile.write_text('not JSON')
            got=call('_gh',args=['repo','view'],execute=True)['value'];commands+=1
            assert got is None
        native.cwd=None;native.use_working_directory(Path.cwd())
        # Native reads must reject oversized/corrupt graph files without fetching diffs.
        got=call('attach_graph_impact',prs=prs,graph_path=str(graph),max_graph_bytes=8,command_responses=[])
        assert got['value']['labels']=={} and got['command_calls']==[]
        # Real native HTTP sends use only this local server and fixture credentials.
        class Handler(BaseHTTPRequestHandler):
            def log_message(self,*args):pass
            def do_POST(self):
                body=json.loads(self.rfile.read(int(self.headers['content-length'])))
                http_calls.append({'path':self.path,'body':body,'authorization':self.headers.get('authorization'),'api_key':self.headers.get('x-api-key')})
                if self.path.startswith('/retry/') and sum(c['path']==self.path for c in http_calls)==1:
                    self.send_response(429);data=b'{"error":{"message":"fixture rate limit"}}'
                elif self.path.startswith('/denied/'):
                    self.send_response(401);data=b'{"error":{"message":"fixture refused"}}'
                else:
                    self.send_response(200)
                    if '/anthropic/' in self.path:
                        data=('data: '+json.dumps({'type':'content_block_delta','delta':{'type':'text_delta','text':'#1 — API\n'}})+'\n\n'+
                              'data: '+json.dumps({'type':'message_stop'})+'\n\n').encode()
                    elif self.path.startswith('/partial/'):
                        data=('data: '+json.dumps({'choices':[{'delta':{'content':'partial'}}]})+'\n\n'+
                              'data: '+json.dumps({'error':{'message':'fixture interrupted'}})+'\n\n').encode()
                    else:
                        data=('data: '+json.dumps({'choices':[{'delta':{'content':'#1 — API\n'}}]})+'\n\n'+
                              'data: '+json.dumps({'choices':[{'delta':{'content':'#2 — Docs'}}]})+'\n\ndata: [DONE]\n\n').encode()
                self.send_header('Content-Length',str(len(data)));self.send_header('Content-Type','text/event-stream');self.end_headers()
                self.wfile.write(data)
        server=ThreadingHTTPServer(('127.0.0.1',0),Handler);worker=threading.Thread(target=server.serve_forever,daemon=True);worker.start()
        address='http://127.0.0.1:'+str(server.server_port)
        try:
            for backend,key,path in [('openai','OPENAI_API_KEY','normal'),('claude','ANTHROPIC_API_KEY','anthropic'),('kimi','MOONSHOT_API_KEY','normal'),
                                     ('gemini','GEMINI_API_KEY','normal'),('ollama','OLLAMA_API_KEY','normal'),('openai','OPENAI_API_KEY','retry'),
                                     ('openai','OPENAI_API_KEY','denied'),('openai','OPENAI_API_KEY','partial')]:
                env={key:'fixture-key','GRAPHIFY_TRIAGE_BACKEND':backend,'GRAPHIFY_TRIAGE_MODEL':'fixture-model'}
                result=call('triage_with_opus',prs=prs,base='main',execute=True,env=env,backend_overrides={backend:{'base_url':address+'/'+path}})['value']
                if path in ('denied','partial'):
                    assert 'Triage failed:' in result['stderr'],result
                    if path=='partial':assert 'partial' in result['stdout']
                else:assert 'API' in result['stdout'] and not result['stderr'],result
            for request in http_calls:
                body=request['body']
                assert body['max_tokens']==1024 and body['model']=='fixture-model' and body['stream'] is True
                assert body['messages'][0]['role']=='user' and 'PR #1' in body['messages'][0]['content']
                assert request['authorization']=='Bearer fixture-key' or request['api_key']=='fixture-key'
            assert sum('/retry/' in c['path'] for c in http_calls)==2
        finally:server.shutdown();worker.join(timeout=3);server.server_close()
        for anthropic,text in [
            (False,': comment\r\ndata: {"choices":[{"delta":{"content":"東京"}}]}\r\n\r\ndata: [DONE]\r\n\r\n'),
            (True,'event: content_block_delta\ndata: {"type":"content_block_delta",\ndata: "delta":{"type":"text_delta","text":"東京"}}\n\n'),
        ]:
            result=call('consume_sse',text=text,anthropic=anthropic)['value'];sse_cases+=1
            assert result=={'text':'東京','chunks':['東京'],'error':None},result
        result=call('consume_sse',text='data: {malformed}\n\n')['value'];sse_cases+=1
        assert result['error'] and result['text']==''
    return {'actual_command_scenarios':commands,'loopback_http_requests':len(http_calls),'sse_framing_scenarios':sse_cases,
            'live_github_requests':0,'live_model_requests':0,'partial_streams_retried':False}
