#!/usr/bin/env python3
"""Compare the native heuristic estimator with unchanged upstream tests."""
import contextlib, inspect, io, json, random, sys, tempfile, types
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
from naming import to_upstream
import networkx as nx
import graphify.benchmark as reference
import graphify.security as security
import graphify.serve as serve
oracle=types.ModuleType('reference_benchmark');oracle.__package__='graphify'
exec(compile((SOURCE/'graphify/benchmark.py').read_text(),str(SOURCE/'graphify/benchmark.py'),'exec'),oracle.__dict__)
native=Native('benchmark');saved={};comparisons={};graphs=0;artifacts=0;encoding_failures=0
# Per-graph query caches the reference keeps in G.graph once a query ran on it
# (0.9.80 #4192, idf, trigram index): derived data the native rebuilds itself.
QUERY_CACHES=('_idf_cache','_trigram_index','_traversal_view','_path_search_graphs')
def plain(v):
    if isinstance(v,nx.Graph):
        data=nx.node_link_data(v,edges='links')
        data['graph']={k:x for k,x in data['graph'].items() if k not in QUERY_CACHES}
        return data
    if isinstance(v,Path):return str(v)
    return v
def call(function,**kw):
    if function in ('_safe','_hr','print_benchmark'):kw.setdefault('encoding',getattr(sys.stdout,'encoding',None) or '')
    if function=='run_benchmark':kw['max_graph_bytes']=security._MAX_GRAPH_FILE_BYTES
    response=native.call('benchmark',function=function,**{k:plain(v) for k,v in kw.items()})
    if not response.get('ok'):
        # The graph loader's error categories (0.9.87 _read_graph): a missing
        # file is FileNotFoundError, every other refusal ValueError.
        error=response.get('error','')
        raise (FileNotFoundError if 'Graph file not found' in error else ValueError)(error)
    result=response['result']
    return to_upstream(result)
def wrap(name):
    sig=inspect.signature(getattr(reference,name))
    def invoke(*args,**kw):
        bound=sig.bind(*args,**kw);bound.apply_defaults()
        value=call(name,**bound.arguments)
        if name=='print_benchmark':print(value,end='');return None
        return value
    return invoke
def compare(name,**kw):
    expected=getattr(oracle,name)(**kw);actual=call(name,**kw);comparisons[name]=comparisons.get(name,0)+1
    if actual!=expected:native.fail(function=name,arguments={k:plain(v) for k,v in kw.items()},expected=expected,actual=actual)
    return actual
try:
    for name in ['_safe','_hr','_estimate_tokens','_query_subgraph_tokens','run_benchmark','print_benchmark']:
        saved[name]=getattr(reference,name);setattr(reference,name,wrap(name))
    tests=upstream_tests('tests/test_benchmark.py','tests/test_benchmark_raw_graph.py','tests/test_benchmark_real_query.py')
    for name,fn in saved.items():setattr(reference,name,fn)
    rng=random.Random(74)
    queries=['how does authentication work','wie funktioniert die API?','comment fonctionne API','cómo funciona authentication',
             '如何建立知识图谱','前端和后端之间如何通信','机器学习的核心算法','函数调用关系','北京大学和清华大学','南京市长江大桥',
             'OpenAI与数据库API连接','a_b c! e-mail','it AI go','how does it work','東京 🧠','e\u0301 é Ω',
             'what connects API and database','what are the core abstractions',
             '中文abc-def','版本v1.2.3中文','后端foo_bar.py','为什么函数a.b(x)失败']
    for dictionary in (False,True):
        with patch.object(serve,'_jieba',serve._jieba if dictionary else None):
            for question in queries+[' '.join(rng.choices(queries,k=3)) for _ in range(100)]:
                expected=serve._query_terms(question)
                actual=call('_query_terms',question=question,segmentation='jieba' if dictionary else 'bigrams')
                comparisons['query_terms_dictionary' if dictionary else 'query_terms_bigrams']=comparisons.get('query_terms_dictionary' if dictionary else 'query_terms_bigrams',0)+1
                if expected!=actual:native.fail(question=question,dictionary=dictionary,expected=expected,actual=actual)
    with tempfile.TemporaryDirectory() as temporary:
        root=Path(temporary)
        for _ in range(180):
            G=rng.choice([nx.Graph,nx.DiGraph,nx.MultiGraph,nx.MultiDiGraph])()
            for i in range(rng.randrange(1,20)):
                G.add_node(f'n{i}',label=rng.choice(['authentication','api_handler','database','main_entry','errors','前端','后端',None,'Ω']),source_file=f'{i}.rs',source_location=rng.choice(['L1','L4',None]))
            for _ in range(len(G)*2):G.add_edge(rng.choice(list(G)),rng.choice(list(G)),relation=rng.choice(['calls','imports','contains','验证']))
            question=rng.choice(queries)
            for depth in [-1,0,1,3,6]:compare('_query_subgraph_tokens',G=G,question=question,depth=depth);graphs+=1
            # The reference's query above left its caches in G.graph (the
            # trigram index holds array('i') postings); graph.json has none.
            data=nx.node_link_data(G,edges=rng.choice(['edges','links']))
            data['graph']={k:x for k,x in data['graph'].items() if k not in QUERY_CACHES}
            path=root/'graph.json';path.write_text(json.dumps(data),encoding='utf-8')
            words=rng.choice([None,-51,0,100,10000,10000001])
            result=compare('run_benchmark',graph_path=str(path),corpus_words=words,questions=[question,'how does authentication work'])
            for encoding in ['utf-8','cp1252']:
                stream=io.TextIOWrapper(io.BytesIO(),encoding=encoding,errors='strict')
                try:
                    with contextlib.redirect_stdout(stream):oracle.print_benchmark(result)
                except UnicodeEncodeError:
                    actual=call('print_benchmark',result=result,encoding=encoding)
                    try:actual.encode(encoding)
                    except UnicodeEncodeError:encoding_failures+=1;continue
                    native.fail(reason='Native formatted text changed unencodable question content',result=result)
                stream.flush();expected=stream.buffer.getvalue().decode(encoding)
                actual=call('print_benchmark',result=result,encoding=encoding)
                if actual!=expected:native.fail(result=result,expected=expected,actual=actual)
                artifacts+=1
        for text in ['','😀','é'*300,'αβ γδ','日本語'*150,'a'*401]:compare('_estimate_tokens',text=text)
    native.receipt('graphify/benchmark.py',['src/benchmark.rs','src/query_terms.rs','src/assets/query/stopwords.json','src/assets/query/jieba-py-0.46.12.hmm'],
        upstream_tests=tests,comparisons=comparisons,graph_queries=graphs,formatted_reports=artifacts,matching_unencodable_question_failures=encoding_failures,
        adaptations=['Dictionary-backed Chinese segmentation uses optional native MIT jieba-rs with pinned jieba-py 0.46.12 HMM parameters; the upstream bigram fallback is also available and independently checked.',
                     'Rust output is Unicode by default. The formatter accepts an explicit encoding for legacy console compatibility.',
                     'This preserves the upstream character/word-based approximation. It does not measure actual billed tokens, model accuracy, full-corpus answer quality, or superiority.',
                     'Since Graphify 0.9.87 (#4347) a question costs the estimated tokens of the answer query_graph returns (the native serve query text, BFS, depth 3, 2000-token budget), on the graph loaded as the MCP server loads it; its parity follows the serve port.'])
finally:
    for name,fn in saved.items():setattr(reference,name,fn)
    native.close()
