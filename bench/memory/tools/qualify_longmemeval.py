#!/usr/bin/env python3
"""Frozen public LongMemEval-S screen using the actual Rust service and original histories.

No reference answers, support labels, or evaluation questions enter ingestion.
This measures source retrieval + answering, not trained neural memory or full upstream parity.
Author the generated blinded judgments before calling score; no model judge is hidden here.
"""
import argparse,ast,collections,secrets,concurrent.futures,hashlib,json,math,random,shutil,subprocess,time
from pathlib import Path
ROOT=Path(__file__).resolve().parents[3]/'crates'/'graphicle'  # the runtime crate: its datasets and the official rubric
DATA_SHA='d6f21ea9d60a0d56f34a05b609c79c88a451d2ae03597821ea3d5a9678c3a442'
JUDGE_SHA='ecce9c4c79dc89d99534ac17b383a5cbb5b9f0c69ee98adaf0684742e3d95251'
SEED='graphicle-0.4-public-screen-v1'
ARMS=['basic','reflect']
def sha(p):return hashlib.sha256(Path(p).read_bytes()).hexdigest()
def write(p,x):
    p.parent.mkdir(parents=True,exist_ok=True);t=p.with_suffix(p.suffix+'.tmp');t.write_text(json.dumps(x,indent=2,ensure_ascii=False)+'\n');t.replace(p)
def fmt_function(path):
    if sha(path)!=JUDGE_SHA:raise ValueError('Official rubric changed')
    tree=ast.parse(path.read_text());ns={'__builtins__':{'NotImplementedError':NotImplementedError}}
    fn=[x for x in tree.body if isinstance(x,ast.FunctionDef) and x.name=='get_anscheck_prompt']
    exec(compile(ast.Module(body=fn,type_ignores=[]),str(path),'exec'),ns);return ns['get_anscheck_prompt']
def selected(rows):
    key=lambda r:hashlib.sha256((SEED+':'+r['question_id']).encode()).hexdigest()
    out=[]
    for typ in sorted({r['question_type'] for r in rows}):out+=sorted([r for r in rows if r['question_type']==typ and not r['question_id'].endswith('_abs')],key=key)[:6]
    out+=sorted([r for r in rows if r['question_id'].endswith('_abs')],key=key)[:6]
    return sorted(out,key=key)
def history(row):
    # Explicit whitelist: has_answer and all question-level labels are excluded.
    docs={};spans={};mapping={}
    for i,(date,sid,turns) in enumerate(zip(row['haystack_dates'],row['haystack_session_ids'],row['haystack_sessions'],strict=True)):
        name=f'session-{i:04d}.txt';text=f'Conversation date: {date}\n\n';support=[]
        for t in turns:
            role=t['role'];content=t['content'];start=len(text)
            text+=f'{role}: {content}\n\n'
            if t.get('has_answer') is True:support.append({'start':start+len(role)+2,'end':start+len(role)+2+len(content)})
        docs[name]=text;spans['file:'+name]=support;mapping['file:'+name]=sid
    return docs,spans,mapping
def coverage(context, docs, spans, mapping, required):
    """Score exact source intervals; repeated unanchored quotes cannot locate a turn."""
    intervals = collections.defaultdict(list)
    touched = set()
    invalid = ambiguous = 0
    encoded = {}
    for ev in context.get('evidence', []):
        sid = ev.get('id', '')
        name = sid.removeprefix('file:')
        text, quote = docs.get(name, ''), ev.get('text', '')
        if not isinstance(quote, str) or not quote or name not in docs:
            continue
        reference = ev if 'byte_start' in ev or 'byte_end' in ev else ev.get('source', {})
        if 'byte_start' in reference or 'byte_end' in reference:
            if name not in encoded:
                encoded[name] = text.encode()
            raw = encoded[name]
            a, b = reference.get('byte_start'), reference.get('byte_end')
            if (type(a) is not int or type(b) is not int or not 0 <= a < b <= len(raw)
                    or raw[a:b] != quote.encode()):
                invalid += 1
                continue
            try:
                start = len(raw[:a].decode())
            except UnicodeDecodeError:
                invalid += 1
                continue
        else:
            start = text.find(quote)
            if start < 0:
                invalid += 1
                continue
            if text.find(quote, start + 1) >= 0:
                ambiguous += 1
                if sid in mapping:
                    touched.add(mapping[sid])
                continue
        intervals[sid].append((start, start + len(quote)))
        if sid in mapping:
            touched.add(mapping[sid])
    merged = {}
    for sid, ranges in intervals.items():
        ranges.sort()
        out = []
        for a, b in ranges:
            if out and a <= out[-1][1]:
                out[-1] = (out[-1][0], max(b, out[-1][1]))
            else:
                out.append((a, b))
        merged[sid] = out
    labelled = [(sid, turn) for sid, turns in spans.items() for turn in turns]
    complete = lambda sid, a, b: any(start <= a and end >= b for start, end in merged.get(sid, []))
    required = set(required)
    return {'annotated_sessions': len(required), 'annotated_sessions_touched': len(required & touched),
            'all_annotated_sessions_touched': bool(required) and required <= touched,
            'annotated_answer_turns': len(labelled),
            'complete_annotated_answer_turns': sum(complete(sid, v['start'], v['end']) for sid, v in labelled),
            'all_annotated_answer_turns_present': bool(labelled) and all(complete(sid, v['start'], v['end']) for sid, v in labelled),
            'session_touch_is_not_complete_evidence': True,
            'evidence_characters': sum(b - a for ranges in merged.values() for a, b in ranges),
            'invalid_evidence_intervals': invalid, 'ambiguous_unanchored_quotes': ambiguous}

def main():
    p=argparse.ArgumentParser();p.add_argument('phase',choices=['freeze','run','packets','score']);p.add_argument('--run-dir',type=Path,required=True);p.add_argument('--dataset',type=Path,default=ROOT/'qualification/0.4/datasets/longmemeval_s_cleaned.json');p.add_argument('--binary',type=Path);p.add_argument('--profile',type=Path);p.add_argument('--concurrency',type=int,default=2);a=p.parse_args()
    root=a.run_dir.resolve();root.mkdir(parents=True,exist_ok=True)
    if sha(a.dataset)!=DATA_SHA:raise ValueError('Dataset differs from pinned cleaned release')
    rows=selected(json.loads(a.dataset.read_text()));protocol_file=root/'PROTOCOL.json'
    rubric=ROOT/'qualification/0.4/official-longmemeval/evaluate_qa.py'
    if a.phase=='freeze':
        if protocol_file.exists():raise ValueError('Already frozen; use a new run directory for a changed candidate')
        if not a.binary or not a.profile:p.error('freeze requires --binary and --profile')
        shutil.copyfile(a.binary,root/'graphicle');(root/'graphicle').chmod(0o755);shutil.copyfile(a.profile,root/'models.json')
        protocol={'binary_sha256':sha(root/'graphicle'),'runner_sha256':sha(__file__),'dataset_sha256':DATA_SHA,'rubric_sha256':sha(rubric),'configuration_sha256':sha(root/'models.json'),'questions':[{'id':r['question_id'],'type':r['question_type'],'abstention':r['question_id'].endswith('_abs')} for r in rows],
         'selection':{'seed':SEED,'method':'6 hash-selected answerable questions per category plus 6 hash-selected abstentions; chosen without inspecting answers','size':len(rows)},'arms':ARMS,'limits':{'max_calls':3,'max_attempts':2,'max_input_tokens':72000,'max_output_tokens':9000,'context_tokens':8000,'evidence_limit':12,'retain':False},
         'source_scope':'All supplied S history sessions; only role/content/date passed to ingestion. No gold, support labels or questions used to build memory.',
         'memory_scope':'Persistent original sources, lexical/code retrieval, bounded source-reading controller and answer review. No pre-approved semantic memories, graph reports or learned neural checkpoint.',
         'baseline':'Same model and histories, date, initial evidence budget; basic uses one ordinary retrieval answer call. Reflect may use up to three total calls. Actual calls/cost differ and are reported.',
         'training':False,'independent_private_test':False,'public_benchmark_previously_used_in_other_research':True,'purpose':'Public benchmark release screen, not all 500 questions and not a leaderboard claim','frozen_unix':time.time()}
        write(protocol_file,protocol);print(json.dumps({'frozen':len(rows),'protocol':str(protocol_file)}));return
    protocol=json.loads(protocol_file.read_text())
    for file,key in [(root/'graphicle','binary_sha256'),(Path(__file__),'runner_sha256'),(root/'models.json','configuration_sha256'),(rubric,'rubric_sha256')]:
        if sha(file)!=protocol[key]:raise ValueError(f'Frozen input changed: {key}')
    if [r['question_id'] for r in rows]!=[r['id'] for r in protocol['questions']]:raise ValueError('Selected population changed')
    def case(row):
        case=root/'cases'/row['question_id'];case.mkdir(parents=True,exist_ok=True);output=case/'RESULT.json'
        if output.exists() and len(json.loads(output.read_text()).get('arms',{}))==len(ARMS):return
        docs,spans,mapping=history(row);source_dir=case/'sources';source_dir.mkdir(exist_ok=True)
        for name,text in docs.items():(source_dir/name).write_text(text)
        command=[str(root/'graphicle'),'--data-dir',str(case/'store'),'--config',str(root/'models.json')]
        def call(*args):
            proc=subprocess.run([*command,*map(str,args)],capture_output=True,text=True,timeout=900)
            if proc.returncode:raise RuntimeError(proc.stderr.strip()[:1000] or f'CLI exited {proc.returncode} without an error message')
            return json.loads(proc.stdout)
        result={'question_id':row['question_id'],'type':row['question_type'],'abstention':row['question_id'].endswith('_abs'),'session_count':len(docs),'source_bytes':sum(len(t.encode()) for t in docs.values()),'source_sha256':{k:hashlib.sha256(t.encode()).hexdigest() for k,t in docs.items()},'arms':{}}
        try:
            start=time.monotonic();result['ingestion']=call('sync',source_dir);result['ingestion_seconds']=time.monotonic()-start
            for arm in ARMS:
                args={**protocol['limits'],'question':row['question'],'question_time':row['question_date']};write(case/'arguments.json',args)
                start=time.monotonic()
                try:
                    job=call('run',arm,'--arguments',case/'arguments.json','--max-calls',args['max_calls'],'--wait')
                    value={'status':'ok','job':job,'latency_seconds':time.monotonic()-start}
                    value['coverage']=coverage(job.get('result',{}).get('context',{}),docs,spans,mapping,row['answer_session_ids'])
                except Exception as e:value={'status':'failed','error':str(e),'latency_seconds':time.monotonic()-start}
                result['arms'][arm]=value;write(output,result)
            result['jobs']=call('jobs');result['complete']=True
        except Exception as e:
            result['ingestion_error']=str(e)
            for arm in ARMS:result['arms'].setdefault(arm,{'status':'failed','error':str(e)})
        write(output,result);print(f"{row['question_id']}: "+', '.join(f'{k}={v["status"]}' for k,v in result['arms'].items()),flush=True)
    if a.phase=='run':
        with concurrent.futures.ThreadPoolExecutor(max_workers=max(1,min(a.concurrency,4))) as pool:list(pool.map(case,rows))
        print('All selected questions recorded, including failures. Prepare blinded grading packets.');return
    if a.phase=='packets':
        if (root/'PACKETS.json').exists():raise ValueError('Packets already frozen')
        fmt=fmt_function(rubric);mapping={};packets=[]
        for row in rows:
            result=json.loads((root/'cases'/row['question_id']/'RESULT.json').read_text());answers=[]
            for arm in ARMS:
                value=result['arms'][arm];identifier=secrets.token_hex(12)
                answer=value.get('job',{}).get('result',{}).get('answer','')
                mapping[identifier]={'question_id':row['question_id'],'arm':arm,'status':value['status'],'type':row['question_type'],'abstention':row['question_id'].endswith('_abs')}
                answers.append({'answer_id':identifier,'status':value['status'],'official_prompt':fmt(row['question_type'],row['question'],row['answer'],answer,row['question_id'].endswith('_abs'))})
            answers.sort(key=lambda v:v['answer_id']);packets.append({'question_id':row['question_id'],'answers':answers})
        write(root/'UNBLIND_AFTER_JUDGING.json',mapping)
        for i in range(0,len(packets),7):write(root/'packets'/f'{i//7+1:02d}.json',packets[i:i+7])
        write(root/'PACKETS.json',{'protocol_sha256':sha(protocol_file),'files':{str(p.relative_to(root)):sha(p) for p in sorted((root/'packets').glob('*.json'))},'results':{str(p.relative_to(root)):sha(p) for p in sorted((root/'cases').glob('*/RESULT.json'))},'mapping_sha256':sha(root/'UNBLIND_AFTER_JUDGING.json')})
        print(json.dumps({'packets':math.ceil(len(packets)/7),'ratings':len(mapping)}));return
    packet_pin=json.loads((root/'PACKETS.json').read_text())
    if sha(protocol_file)!=packet_pin['protocol_sha256'] or sha(root/'UNBLIND_AFTER_JUDGING.json')!=packet_pin['mapping_sha256']:raise ValueError('Review protocol changed')
    for name,value in (packet_pin['files']|packet_pin['results']).items():
        if sha(root/name)!=value:raise ValueError('Frozen review evidence changed')
    mapping=json.loads((root/'UNBLIND_AFTER_JUDGING.json').read_text());judgments=[json.loads(l) for l in (root/'JUDGMENTS.jsonl').read_text().splitlines() if l.strip()]
    if len(judgments)!=len(mapping) or {x['answer_id'] for x in judgments}!=set(mapping):raise ValueError('Every unique answer needs a judgment')
    scored=[]
    for j in judgments:
        if j['verdict'] not in ('yes','no') or not j.get('reason') or type(j.get('ambiguity')) is not bool:raise ValueError('Invalid judgment')
        m=mapping[j['answer_id']]
        if m['status']!='ok' and j['verdict']!='no':raise ValueError('Failed inference receives no credit')
        scored.append({**m,**j})
    summaries={};pairs=collections.defaultdict(dict)
    for arm in ARMS:
        r=[x for x in scored if x['arm']==arm];categories={}
        for typ in sorted({x['type'] for x in r}):
            rr=[x for x in r if x['type']==typ];categories[typ]={'n':len(rr),'correct':sum(x['verdict']=='yes' for x in rr)}
        summaries[arm]={'n':len(r),'correct':sum(x['verdict']=='yes' for x in r),'failed':sum(x['status']!='ok' for x in r),'categories':categories}
        for x in r:pairs[x['question_id']][arm]=int(x['verdict']=='yes')
    deltas=[x['reflect']-x['basic'] for x in pairs.values()];rng=random.Random(20260922);boot=sorted(sum(rng.choices(deltas,k=len(deltas)))/len(deltas)*100 for _ in range(10000))
    write(root/'SCORES.json',{'arms':summaries,'delta_percentage_points':sum(deltas)/len(deltas)*100,'paired_bootstrap_95_percent':[boot[249],boot[9749]],'judge':'Codex assistant applying pinned official rubric with arm labels hidden','official_gpt4o_judge':False,'semantic_grounding_not_measured_by_official_qa_rubric':True,'public_subset':True,'judgments':scored})
    print(json.dumps(summaries,indent=2))
if __name__=='__main__':main()
