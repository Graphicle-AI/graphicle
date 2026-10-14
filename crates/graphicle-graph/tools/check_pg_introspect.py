#!/usr/bin/env python3
"""Catalog parity and a loopback PostgreSQL-wire fixture for the real libpq path."""
import ast,importlib.util,json,random,socket,struct,sys,threading,types
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
from upstream_fixes import oracle_source
import graphify.pg_introspect as reference
spec=importlib.util.spec_from_file_location('_pg_oracle',SOURCE/'graphify/pg_introspect.py');oracle=importlib.util.module_from_spec(spec);spec.loader.exec_module(oracle)
# The generated DDL goes through the SQL extractor; use the pinned extractor with the
# reviewed post-pin fixes applied in memory (PR #3020 adapted, issue #2712) as the oracle.
_sql=types.ModuleType('graphify_oracle_sql');_sql.__package__='graphify.extractors'
exec(compile(oracle_source('graphify/extractors/sql.py'),'<pinned sql.py with reviewed fixes>','exec'),_sql.__dict__)
oracle.extract_sql=_sql.extract_sql
native=Native('pg_introspect');catalogs=0;ddls=0;wire_runs=0;counts={}
queries=native.result('postgres.queries')
def compare(catalog,info,expected):
    global catalogs
    actual=native.result('postgres.catalog',catalog=catalog,info=info);catalogs+=1
    if actual!=expected:native.fail(catalog=catalog,info=info,expected=expected,actual=actual)
    return actual
def wrapped(dsn=None):
    pg=sys.modules['psycopg']
    try:conn=pg.connect(dsn or '')
    except pg.OperationalError as e:
        actual=native.result('postgres.connection_error',message=str(e));wanted='could not connect to PostgreSQL: '+str(e).split('\n')[0]
        if actual!=wanted:native.fail(expected=wanted,actual=actual)
        raise ConnectionError(actual)
    conn.execute(queries['transaction']);catalog={}
    with conn.cursor() as cur:
        for name,q in zip(['tables','views','routines','fks'],queries['queries']):cur.execute(q);catalog[name]=cur.fetchall()
    conn.close();info=pg.conninfo.conninfo_to_dict(dsn or '')
    return compare(catalog,info,oracle.introspect_postgres(dsn))

def frame(kind,body):return kind+struct.pack('!I',len(body)+4)+body
def exact(sock,n):
    data=b''
    while len(data)<n:
        chunk=sock.recv(n-len(data))
        if not chunk:raise EOFError
        data+=chunk
    return data
def array_text(values):return '{'+','.join('NULL' if v is None else '"'+v.replace('\\','\\\\').replace('"','\\"')+'"' for v in values)+'}'
class PgFixture:
    def __init__(self,catalog,fail_query=None):
        self.catalog=catalog;self.queries=[];self.failure=None;self.fail_query=fail_query
        self.socket=socket.socket();self.socket.bind(('127.0.0.1',0));self.socket.listen(1);self.port=self.socket.getsockname()[1]
        self.thread=threading.Thread(target=self.run,daemon=True);self.thread.start()
    def run(self):
        try:
            conn,_=self.socket.accept();conn.settimeout(10)
            with conn:
                n=struct.unpack('!I',exact(conn,4))[0];startup=exact(conn,n-4)
                assert struct.unpack('!I',startup[:4])[0]==196608
                conn.sendall(frame(b'R',struct.pack('!I',0))+frame(b'S',b'server_version\x0018.3\x00')+frame(b'S',b'client_encoding\x00UTF8\x00')+frame(b'Z',b'I'))
                while True:
                    kind=exact(conn,1);n=struct.unpack('!I',exact(conn,4))[0];body=exact(conn,n-4)
                    if kind==b'X':break
                    assert kind==b'Q',(kind,body)
                    query=body[:-1].decode();self.queries.append(query)
                    if self.fail_query and self.fail_query in query:
                        conn.sendall(frame(b'E',b'SERROR\x00VERROR\x00C42501\x00Mcatalog denied\x00\x00')+frame(b'Z',b'E'));continue
                    selected=None
                    for key,needle in [('tables','information_schema.tables'),('views','information_schema.views'),('routines','information_schema.routines'),('fks','pg_catalog.pg_constraint')]:
                        if needle in query:selected=key
                    if selected is None:
                        conn.sendall(frame(b'C',b'BEGIN\x00' if query=='BEGIN' else b'SET\x00')+frame(b'Z',b'T'));continue
                    rows=self.catalog[selected];cols={'tables':3,'views':3,'routines':5,'fks':7}[selected]
                    fields=struct.pack('!H',cols)
                    for i in range(cols):fields+=('c'+str(i)).encode()+b'\0'+struct.pack('!IhIhih',0,0,1003 if selected=='fks' and i in [3,6] else 25,-1,-1,0)
                    data=frame(b'T',fields)
                    for row in rows:
                        record=struct.pack('!H',cols)
                        for value in row:
                            if value is None:record+=struct.pack('!i',-1)
                            else:
                                encoded=(array_text(value) if isinstance(value,list) else value).encode();record+=struct.pack('!I',len(encoded))+encoded
                        data+=frame(b'D',record)
                    conn.sendall(data+frame(b'C',f'SELECT {len(rows)}'.encode()+b'\0')+frame(b'Z',b'T'))
        except EOFError:pass
        except BaseException as e:self.failure=e
        finally:self.socket.close()
    def close(self):
        self.thread.join(12)
        assert not self.thread.is_alive(),'libpq did not close its connection'
        if self.failure:raise self.failure
try:
    tree=ast.parse((SOURCE/'tests/test_pg_introspect.py').read_text())
    names=[n.name for n in tree.body if isinstance(n,ast.FunctionDef) and n.name.startswith('test_') and n.name not in ['test_pg_introspect_import_error','test_pg_introspect_grammar_error_surfaces']]
    with patch.object(reference,'introspect_postgres',wrapped):tests=upstream_tests(*['tests/test_pg_introspect.py::'+n for n in names])
    from psycopg.conninfo import conninfo_to_dict
    for dsn in ['',"host=localhost dbname=example user=demo password='fake value'",'postgresql://demo:dummy@localhost/example?sslmode=require',"host=one,two port=5432,5433 dbname='with space'",'postgresql://[::1]:5432/test',"host='/tmp/socket dir' dbname=x",'postgresql:///sample']:
        expected=conninfo_to_dict(dsn);actual=native.result('postgres.conninfo',dsn=dsn)
        if actual!=expected:native.fail(dsn='qualification input',expected=expected,actual=actual)
    rng=random.Random(2026092355);names=['public','order','user-data','CamelCase','double"quote','é中','space name'];mock_spec=importlib.util.spec_from_file_location('_pg_fixtures',SOURCE/'tests/test_pg_introspect.py');fixtures=importlib.util.module_from_spec(mock_spec);mock_spec.loader.exec_module(fixtures)
    for i in range(120):
        schema=rng.choice(names);a=rng.choice(names);b=rng.choice(names)
        catalog={'tables':[[schema,a,'BASE TABLE'],[schema,b,'BASE TABLE']],'views':[[schema,'View',rng.choice([None,'','SELECT 1','SELECT * FROM "public"."order"'])]],'routines':[[schema,'Fn','FUNCTION',rng.choice([None,'SELECT 42;','BEGIN SELECT 1; END;','c_symbol']),rng.choice(['C','SQL',None,'PLPGSQL'])]],'fks':[['FK',schema,a,['id','other"id'],schema,b,['id','other"id']]]}
        pg=fixtures._make_mock_psycopg(catalog['tables'],catalog['views'],catalog['routines'],catalog['fks'],host='test-host',dbname='fixture')
        saved={};original_sql=oracle.extract_sql
        def capture(path,content=None):saved['ddl']=content;return original_sql(path,content=content)
        with patch.dict(sys.modules,{'psycopg':pg}),patch.object(oracle,'extract_sql',capture):expected=oracle.introspect_postgres('postgresql://test-host/fixture')
        actual_ddl=native.result('postgres.ddl',catalog=catalog);ddls+=1
        if actual_ddl!=saved['ddl']:native.fail(expected=saved['ddl'],actual=actual_ddl)
        compare(catalog,{'host':'test-host','dbname':'fixture'},expected)
        if i<5:
            server=PgFixture(catalog);dsn=f'host=127.0.0.1 port={server.port} user=qualification dbname=fixture sslmode=disable connect_timeout=2'
            try:actual=native.result('postgres.introspect',dsn=dsn)
            finally:server.close()
            wanted=native.result('postgres.catalog',catalog=catalog,info={'host':'127.0.0.1','dbname':'fixture'})
            if actual!=wanted:native.fail(expected=wanted,actual=actual)
            assert server.queries==["set client_encoding to 'UTF8'",'BEGIN',queries['transaction'],*queries['queries']],server.queries
            wire_runs+=1
    server=PgFixture(catalog,fail_query='information_schema.views')
    try:failure=native.call('postgres.introspect',dsn=f'host=127.0.0.1 port={server.port} user=qualification dbname=fixture sslmode=disable connect_timeout=2')
    finally:server.close()
    assert not failure['ok'] and 'catalog denied' in failure['error'];assert all('routines' not in q for q in server.queries)
    for values in [[],['A','B'],['','NULL','comma,name','quote"name','back\\slash','é中'],[None,'NULL']]:
        actual=native.result('postgres.array',text=array_text(values))
        if actual!=values:native.fail(expected=values,actual=actual)
    native.receipt('graphify/pg_introspect.py',['src/pg_introspect.rs',*['src/assets/postgres/'+n+'.sql' for n in ['transaction','tables','views','routines','foreign_keys']]],upstream_tests=tests,catalog_comparisons=catalogs,exact_ddl_comparisons=ddls,libpq_wire_successes=wire_runs,libpq_wire_error_closes=1,cargo_features=['postgres'],
        adaptations=['The optional native postgres feature replaces the optional psycopg package. Static SQL grammar and missing Python-package tests do not apply.', 'Wire tests use a loopback PostgreSQL protocol fixture with the real libpq client, not a production database. TLS/authentication combinations remain libpq responsibilities and were not newly qualified here.'])
finally:native.close()
