#!/usr/bin/env python3
"""Compare original Cypher calls; exercise native drivers on loopback protocols."""
import contextlib,io,json,random,socketserver,struct,sys,threading,types
from unittest.mock import patch
from parity import Native,SOURCE
import networkx as nx
import graphify.exporters.graphdb as reference
from falkordb.graph import Graph as FalkorGraph
from neo4j._codec.packstream.v1 import Packer,Unpacker,UnpackableBuffer
from neo4j._codec.packstream import Structure

native=Native('export_graphdb');plans=0;wire_cases=[]
def invoke(function,**kw):
    if isinstance(kw.get('G'),nx.Graph):kw['G']=nx.node_link_data(kw['G'],edges='edges')
    return native.call('exporters.graphdb',function=function,**kw)
def result(function,**kw):
    r=invoke(function,**kw)
    if not r['ok']:raise ValueError(r['error'])
    return r['result']
def reference_plan(G,communities,backend='neo4j',**opts):
    calls=[];connection={}
    class Client:
        def __init__(self,**kw):connection.update(kw)
        def select_graph(self,name):connection['graph_name']=name;return self
        def query(self,q,params=None):calls.append({'query':q,'params':params})
        def run(self,q,**kw):calls.append({'query':q,'params':kw})
        def session(self):return self
        def close(self):connection['closed']=True
        def __enter__(self):return self
        def __exit__(self,*args):return False
    def driver(uri,auth):connection.update(uri=uri,auth=auth);return Client()
    mods={'neo4j':types.SimpleNamespace(GraphDatabase=types.SimpleNamespace(driver=driver)),'falkordb':types.SimpleNamespace(FalkorDB=Client)}
    with patch.dict(sys.modules,mods):
        if backend=='neo4j':counts=reference.push_to_neo4j(G,uri='bolt://localhost:7687',user='fixture',password='fixture',communities=communities)
        else:counts=reference.push_to_falkordb(G,communities=communities,**opts)
    return calls,connection,counts

class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address=True;daemon_threads=True
    def __init__(self,kind,fail_at=None):
        self.kind=kind;self.fail_at=fail_at;self.calls=[];self.auth=[];self.routes=0;self.errors=[];self.query_count=0
        super().__init__(('127.0.0.1',0),Handler)
    def __enter__(self):
        self.thread=threading.Thread(target=self.serve_forever,kwargs={'poll_interval':0.02},daemon=True);self.thread.start();return self
    def __exit__(self,*a):self.shutdown();self.server_close();self.thread.join(2)
class Handler(socketserver.StreamRequestHandler):
    def exact(self,n):
        data=self.rfile.read(n)
        if len(data)!=n:raise EOFError
        return data
    def resp(self):
        prefix=self.exact(1);line=self.rfile.readline().rstrip(b'\r\n')
        if prefix==b'*':return [self.resp() for _ in range(int(line))]
        if prefix==b'$':out=self.exact(int(line));assert self.exact(2)==b'\r\n';return out.decode()
        raise AssertionError((prefix,line))
    def bolt_reply(self,tag,payload):
        output=io.BytesIO();Packer(output).pack(Structure(bytes([tag]),payload));data=output.getvalue()
        self.wfile.write(struct.pack('>H',len(data))+data+b'\0\0');self.wfile.flush()
    def handle(self):
        self.request.settimeout(8)
        s=self.server
        try:
            if s.kind=='redis':
                while True:
                    args=self.resp()
                    if args[0]=='AUTH':s.auth.append(args[1:])
                    if args[0]=='GRAPH.QUERY':
                        s.calls.append(args);s.query_count+=1
                        if s.query_count==s.fail_at:self.wfile.write(b'-ERR staged fixture failure\r\n')
                        else:self.wfile.write(b'*1\r\n*0\r\n')
                    else:self.wfile.write(b'+OK\r\n')
                    self.wfile.flush()
            else:
                hello=self.exact(20);assert hello[:4]==b'\x60\x60\xb0\x17';assert b'\0\0\4\4' in hello[4:]
                self.wfile.write(bytes([0,0,4,4]));self.wfile.flush()
                while True:
                    data=b''
                    while True:
                        n=struct.unpack('>H',self.exact(2))[0]
                        if n==0:
                            if data:break
                            continue
                        data+=self.exact(n)
                    msg=Unpacker(UnpackableBuffer(data)).unpack();tag=msg.tag[0];fields=msg.fields
                    if tag==1:
                        s.auth.append(fields[0]);self.bolt_reply(0x70,{'server':'Neo4j/5.26.0','connection_id':'bolt-fixture'})
                    elif tag==0x10:
                        s.calls.append({'query':fields[0],'params':fields[1]});s.query_count+=1
                        if s.query_count==s.fail_at:self.bolt_reply(0x7F,{'code':'Neo.ClientError.Statement.SyntaxError','message':'staged fixture failure'})
                        else:self.bolt_reply(0x70,{'fields':[],'t_first':1,'qid':0})
                    elif tag in (0x2F,0x3F):self.bolt_reply(0x70,{'type':'w','t_last':1,'db':'neo4j','stats':{'nodes-created':1}})
                    elif tag==0x66:
                        s.routes+=1;address=f'127.0.0.1:{s.server_address[1]}'
                        self.bolt_reply(0x70,{'rt':{'ttl':300,'db':'neo4j','servers':[{'role':role,'addresses':[address]} for role in ['ROUTE','READ','WRITE']]}})
                    elif tag==0x02:break
                    elif tag==0x0F:self.bolt_reply(0x70,{})
                    else:raise AssertionError(('message',tag,fields))
        except (EOFError,ConnectionResetError,BrokenPipeError):pass
        except Exception as e:s.errors.append(repr(e))

try:
    rng=random.Random(68)
    for case in range(150):
        G=rng.choice([nx.Graph,nx.DiGraph,nx.MultiGraph])()
        for i in range(rng.randrange(1,15)):
            G.add_node(f'n{i}',label=rng.choice(['Order','a"; DELETE x','日本語','x\nline']),file_type=rng.choice(['code','a-b','1snake','', 'ßfoo','ﬃtype']),_origin='ast',metadata={'retained':False},enabled=True,score=0.2,nothing=None)
        for _ in range(len(G)*2):
            u,v=rng.choice(list(G)),rng.choice(list(G))
            # Stored direction markers: absent, a lone hidden source, or a reversed arc.
            G.add_edge(u,v,relation=rng.choice(['calls','a-b','','] DELETE x','ß','含有']),confidence='EXTRACTED',weight=0.5,**rng.choice([{'_src':'hidden'},{},{'_src':v,'_tgt':u}]))
        comm={0:list(G)[::2],2:list(G)[1::2]}
        expected,_,_=reference_plan(G,comm)
        actual=result('query_plan',G=G,communities=comm)
        if expected!=actual:native.fail(expected=expected,actual=actual)
        expected,_,_=reference_plan(G,comm,'falkor',uri='localhost:6379')
        falkor=result('falkordb_query_plan',G=G,communities=comm)
        if expected!=falkor:native.fail(expected=expected,actual=falkor)
        labels=result('distinct_node_labels',G=G)
        if labels!=reference._distinct_node_labels(G):native.fail(expected=reference._distinct_node_labels(G),actual=labels)
        for item in falkor[:1]+falkor[len(labels):len(labels)+2]:
            expected=FalkorGraph.__new__(FalkorGraph)._build_params_header(item['params'])
            assert result('params_header',params=item['params'])==expected
        plans+=1
    G=nx.DiGraph();G.add_node('a',label='Alpha',file_type='code',truth=True,quote='"\\ x');G.add_node('b',label='日本語',file_type='document');G.add_edge('a','b',relation='calls',weight=0.75)
    comm={0:['a'],1:['b']};queries=result('query_plan',G=G,communities=comm)
    falkor_queries=result('falkordb_query_plan',G=G,communities=comm);n_index=len(result('distinct_node_labels',G=G))
    assert falkor_queries==reference_plan(G,comm,'falkor',uri='localhost:6379')[0],falkor_queries
    for uri,user,password in [('localhost:6379',None,None),('falkordb://host:1234','neo4j',None),('redis://x:p@localhost', 'ignored','ignored'),('redis://[::1]:1234',None,None),('redis://localhost:0','account','secret'),('redis://account@localhost',None,None)]:
        _,connection,_=reference_plan(G,comm,'falkor',uri=uri,user=user,password=password)
        connection.pop('graph_name')
        assert result('falkor_connection',uri=uri,user=user,password=password)==connection,(uri,connection)
    # Index statements come first and their failures are tolerated (#3804);
    # a failure at or below n_index is swallowed, a later one aborts the push.
    for backend,scheme,auth,fail_at in [('redis','redis',False,None),('redis','falkordb',True,None),('redis','redis',True,1),('redis','redis',True,n_index+2),('bolt','bolt',True,None),('bolt','neo4j',True,None),('bolt','bolt',True,1),('bolt','neo4j',True,n_index),('bolt','bolt',True,n_index+2)]:
        plan=falkor_queries if backend=='redis' else queries
        tolerated=fail_at is not None and fail_at<=n_index
        with Server(backend,fail_at) as server:
            port=server.server_address[1];uri=f'{scheme}://127.0.0.1:{port}'
            kwargs={'G':G,'communities':comm,'uri':uri,'user':'fixture' if auth else None,'password':'local-fixture-only' if auth else None}
            if backend=='redis':kwargs['graph_name']='native_qualification'
            reply=invoke('push_to_falkordb' if backend=='redis' else 'push_to_neo4j',**kwargs)
            if fail_at and not tolerated:
                assert not reply['ok'] and 'staged fixture failure' in reply['error'],reply
                expected=plan[:fail_at]
            else:
                assert reply=={'ok':True,'result':{'nodes':2,'edges':1}},reply
                expected=plan
            if backend=='redis':
                wanted=[['GRAPH.QUERY','native_qualification',FalkorGraph.__new__(FalkorGraph)._build_params_header(q['params'])+q['query'],'--compact'] for q in expected]
                assert server.calls==wanted,(server.calls,wanted)
                assert server.auth==([['fixture','local-fixture-only']] if auth else []),server.auth
            else:
                assert server.calls==expected,(server.calls,expected)
                assert all(a['principal']=='fixture' and a['credentials']=='local-fixture-only' for a in server.auth)
                if scheme=='neo4j':assert server.routes>0
            assert not server.errors,server.errors
            wire_cases.append({'backend':backend,'scheme':scheme,'auth':auth,'fail_at':fail_at,'tolerated':tolerated,'queries':len(server.calls),'routes':server.routes})
    native.receipt('graphify/exporters/graphdb.py',['src/exporters/graphdb.rs','src/exporters/common.rs'],query_plan_comparisons=plans,falkordb_query_plan_comparisons=plans,distinct_label_comparisons=plans,wire_cases=wire_cases,adaptations=['Neo4j transport uses pinned neo4rs 0.9.0-rc.10 with experimental routing support; FalkorDB uses redis 0.32.7 with the SDK-compatible parameter header.','Loopback protocol fixtures verify real driver negotiation, authentication, routing, query/parameter bytes and failure propagation; no live Neo4j/FalkorDB installation was available and database-engine semantics or production TLS were not qualified.','Rust ownership closes clients on success and failure; FalkorDB network operations have a 30-second timeout.'])
finally:native.close()
