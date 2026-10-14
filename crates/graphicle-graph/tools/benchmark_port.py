#!/usr/bin/env python3
"""Reproducible release/Python comparisons, including identical JSONL transport.

Use --record NAME --binary PATH before/after an optimization. Both persistent
workers are warmed, receive the same requests, and must return equal results.
This measures the exposed port, not just a hand-picked inner loop.
"""
import argparse, contextlib, dataclasses, datetime, hashlib, json, os, platform, statistics, subprocess, sys, tempfile, time
from pathlib import Path
from naming import native_env

ROOT=Path(__file__).resolve().parents[1]
SOURCE=Path(json.loads((ROOT/'migration/FILES.json').read_text())['source_root'])

def canonical(value):
    if isinstance(value,dict):
        value={k:canonical(v) for k,v in value.items()}
        for key in ['nodes','edges','links','hyperedges','extracted_sources','failed_sources']:
            if isinstance(value.get(key),list):value[key].sort(key=lambda v:json.dumps(v,sort_keys=True))
        return value
    if isinstance(value,list):return [canonical(v) for v in value]
    return value

def worker():
    sys.path.insert(0,str(SOURCE))
    from graphify.affected import affected_nodes
    from graphify.extract import extract
    from networkx.readwrite import json_graph
    for line in sys.stdin:
        request=json.loads(line)
        if request['operation']=='affected.hits':
            graph=json_graph.node_link_graph(request['graph'],edges='links')
            result=[dataclasses.asdict(v) for v in affected_nodes(graph,request['seed'],depth=request['depth'])]
        else:
            with contextlib.redirect_stdout(sys.stderr):
                result=extract([Path(p) for p in request['paths']],root=Path(request['root']),parallel=request.get('parallel',False))
        print(json.dumps({'ok':True,'result':result}),flush=True)

def graph(size):
    return {'directed':True,'multigraph':False,'graph':{},
            'nodes':[{'id':f'n{i}','label':f'Handler{i}()','source_file':f'module_{i//50}.py','file_type':'code'} for i in range(size)],
            'links':[{'source':f'n{i}','target':f'n{i+j}','relation':'calls','confidence':'EXTRACTED'} for i in range(size) for j in [1,7,31] if i+j<size]}

def request(process,payload):
    start=time.perf_counter_ns();process.stdin.write(payload);process.stdin.flush()
    line=process.stdout.readline();elapsed=(time.perf_counter_ns()-start)/1e6
    if not line:raise RuntimeError(f'Worker exited: {process.poll()}')
    value=json.loads(line)
    if not value.get('ok'):raise RuntimeError(value)
    return elapsed,value['result']

def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--worker',action='store_true');parser.add_argument('--record');parser.add_argument('--binary',type=Path);parser.add_argument('--samples',type=int,default=7)
    parser.add_argument('--manifest',type=Path)
    parser.add_argument('--baseline-sources',type=Path)
    args=parser.parse_args()
    if args.worker:return worker()
    if not args.record or not args.binary:parser.error('--record and --binary are required')
    binary=args.binary.resolve();results=[];qualification=None
    if args.manifest:
        from qualification import validate
        qualification=validate(args.manifest,binary)
        assert qualification['profile']=='release','Release profile required for performance receipts'
    with tempfile.TemporaryDirectory(prefix='graphify-perf-') as directory:
        root=Path(directory).resolve();corpus=root/'corpus';corpus.mkdir()
        for i in range(100):
            (corpus/f'unit_{i}.py').write_text(f'def normalize_{i}(value):\n    return value.strip()\n\ndef handle_{i}(value):\n    return normalize_{i}(value)\n')
        # The same settings for the reference worker (GRAPHIFY_*) and the engine (GRAPHICLE_*).
        env=native_env({**os.environ,'GRAPHIFY_NO_INCREMENTAL_CACHE':'1','GRAPHIFY_MAX_WORKERS':'1'},'native')
        cases=[(f'graph_load_and_reverse_traversal_{size}',{'operation':'affected.hits','graph':graph(size),'seed':f'n{size-1}','depth':4}) for size in [2000,20000]]
        cases.append(('python_source_extraction_100_files',{'operation':'extract','paths':[str(p) for p in sorted(corpus.glob('*.py'))],'root':str(corpus),'parallel':False,'use_cache':False}))
        processes=[subprocess.Popen(command,cwd=ROOT,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True) for command in [[str(binary),'--jsonl'],[sys.executable,str(Path(__file__).resolve()),'--worker']]]
        try:
            for name,value in cases:
                payload=json.dumps(value,separators=(',',':'))+'\n';timings=[[],[]];expected=None
                # Alternate order to reduce bias from warming and thermal changes.
                for iteration in range(args.samples+1):
                    for which in ([0,1] if iteration%2==0 else [1,0]):
                        elapsed,result=request(processes[which],payload)
                        result=canonical(result)
                        if expected is None:expected=result
                        if result!=expected:
                            (ROOT/'migration/benchmarks').mkdir(parents=True,exist_ok=True)
                            (ROOT/'migration/benchmarks/mismatch.json').write_text(json.dumps({'workload':name,'worker':which,'expected':expected,'actual':result},indent=2))
                            raise AssertionError(f'Result mismatch: {name}, worker {which}; see migration/benchmarks/mismatch.json')
                        if iteration:timings[which].append(elapsed)
                medians=[statistics.median(x) for x in timings]
                row={'workload':name,'request_bytes':len(payload.encode()),'samples':args.samples,'native_ms':timings[0],'python_ms':timings[1],'native_median_ms':medians[0],'python_median_ms':medians[1],'python_over_native':medians[1]/medians[0],'result_sha256':hashlib.sha256(json.dumps(expected,sort_keys=True).encode()).hexdigest()}
                results.append(row);print(json.dumps({k:v for k,v in row.items() if not isinstance(v,list)}),flush=True)
        finally:
            for process in processes:
                process.stdin.close()
                try:process.wait(timeout=10)
                except subprocess.TimeoutExpired:process.kill();process.wait()
    receipt={'name':args.record,'binary':str(binary),'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'profile':'release','system':platform.platform(),'machine':platform.machine(),'python':platform.python_version(),'cpu_count':os.cpu_count(),'transport':'persistent JSONL, identical request bytes, one warmup per workload','results':results,'passed':True}
    receipt.update(measured_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                   benchmark_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                   upstream_commit=json.loads((ROOT/'migration/FILES.json').read_text())['upstream_commit'],
                   rustc=subprocess.check_output(['rustc','--version'],text=True).strip())
    if qualification:
        from qualification import validate
        validate(args.manifest,binary)
        receipt.update(qualification_manifest=str(args.manifest.resolve().relative_to(ROOT)),
                       source_set_sha256=qualification['source_set_sha256'])
    if args.baseline_sources:
        receipt.update(baseline_source_snapshot=str(args.baseline_sources),
                       baseline_source_snapshot_sha256=hashlib.sha256(args.baseline_sources.read_bytes()).hexdigest())
    target=ROOT/'migration/benchmarks'/f'{args.record}.json';target.parent.mkdir(parents=True,exist_ok=True);target.write_text(json.dumps(receipt,indent=2)+'\n')

if __name__=='__main__':main()
