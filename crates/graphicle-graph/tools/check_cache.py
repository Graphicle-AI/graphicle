#!/usr/bin/env python3
"""Original cache contracts against native state plus independent source checks.

Python supplies callback results only where the original API accepts a callable.
The library's file hashing, cache decision, disk IO and state transitions are Rust.
"""
import ast,contextlib,copy,functools,importlib.util,inspect,json,os,random,tempfile,time,warnings
from pathlib import Path
from unittest.mock import patch
from parity import Native,SOURCE,upstream_tests
from naming import native_markers,to_upstream
from upstream_fixes import oracle_source
import graphify.cache as reference
spec=importlib.util.spec_from_file_location('_cache_oracle',SOURCE/'graphify/cache.py');oracle=importlib.util.module_from_spec(spec)
exec(compile(oracle_source('graphify/cache.py'),'<pinned cache.py with portable-path fix>','exec'),oracle.__dict__)
native=Native('cache');counts={};metrics={'source_reads':0,'computed_word_counts':0};comparisons=0
fields={'output':'_GRAPHIFY_OUT','extractor_version':'_EXTRACTOR_VERSION','schema':'_AST_CACHE_SCHEMA','cleaned_ast_dirs':'_cleaned_ast_dirs','legacy_semantic_hits':'_legacy_semantic_hits','corrupt_cache_entries':'_corrupt_cache_entries','stat_index':'_stat_index','stat_index_root':'_stat_index_root','stat_index_anchor':'_stat_index_anchor','stat_index_dirty':'_stat_index_dirty'}
def clean(v):
    if isinstance(v,Path):return str(v)
    if isinstance(v,bytes):return list(v)
    if isinstance(v,(list,tuple,set)):return [clean(x) for x in v]
    if isinstance(v,dict):return {str(k):clean(x) for k,x in v.items()}
    return v
def state():
    s={key:clean(getattr(reference,attr)) for key,attr in fields.items()};s['prompt_fp_cache']={json.dumps(list(k),ensure_ascii=False,separators=(',',':')):v for k,v in reference._prompt_fp_cache.items()};s.update(metrics);s['now_ns']=time.time_ns()
    # The AST directory's grammar fingerprint: the reference's, from its installed distributions.
    if hasattr(reference,'_grammar_fingerprint'):s['grammar_fingerprint']=reference._grammar_fingerprint()
    return s
def invoke(name,args):
    counts[name]=counts.get(name,0)+1;native.use_working_directory(os.getcwd())
    # Cache payloads carry the portable root marker under each side's name.
    result=to_upstream(native.result('cache',function=name,arguments=native_markers(clean(args)),state=native_markers(state())))
    for key,attr in fields.items():
        value=result['state'][key]
        if key in ['stat_index_root','stat_index_anchor']:value=Path(value) if value is not None else None
        if key=='cleaned_ast_dirs':value=set(value)
        setattr(reference,attr,value)
    reference._prompt_fp_cache={tuple(json.loads(k)):v for k,v in result['state']['prompt_fp_cache'].items()}
    metrics.update({k:result['state'][k] for k in metrics})
    for message in result['warnings']:warnings.warn(message,RuntimeWarning,stacklevel=3)
    if 'error' in result:
        if result['error'].startswith('file_hash requires'):raise IsADirectoryError(result['error'])
        raise ValueError(result['error'])
    return result['value']
def prompt(v):return {'kind':'file','value':str(v)} if isinstance(v,Path) else v
mutators=['_relativize_source_files_in','_absolutize_source_files_in','_relativize_ids_in','_absolutize_ids_in']
def wrapper(name):
    signature=inspect.signature(getattr(oracle,name))
    @functools.wraps(getattr(reference,name))
    def call(*args,**kwargs):
        bound=signature.bind(*args,**kwargs);bound.apply_defaults();request=dict(bound.arguments)
        if 'prompt' in request:request['prompt']=prompt(request['prompt'])
        if 'st' in request:request['st']={'size':request['st'].st_size,'mtime_ns':request['st'].st_mtime_ns}
        if name=='cached_word_count':
            compute=request.pop('compute');result=invoke('word_count_lookup',request)
            if result['status']=='hit':return result['value']
            value=compute(Path(request['path']))
            return invoke('complete_word_count',{'ticket':result['ticket'],'value':value})
        result=invoke(name,request)
        if name in ['cache_dir','_stat_index_file','_normalize_path']:return Path(result)
        if name=='_body_content':return bytes(result)
        if name=='cached_files':return set(result)
        if name=='_portability_anchors':return tuple(result)
        if name=='_stat_entry_for':return reference._stat_index[request['abs_key']]
        if name in mutators:
            bound.arguments['payload'].clear();bound.arguments['payload'].update(result);return None
        if name=='scope_semantic_result':
            bound.arguments['result'].clear();bound.arguments['result'].update(result['result']);return set(result['dropped_source_files']),result['dropped_item_count']
        return result
    return call
names=[n for n,v in vars(reference).items() if inspect.isfunction(v) and v.__module__==reference.__name__ and n not in ['_rewrite_strings','_rewrite_id_keyed_table_keys','_semantic_source_matcher','_grammar_fingerprint']]
fns={n:wrapper(n) for n in names}
def compare(name,args):
    global comparisons
    expected_args=copy.deepcopy(args);actual_args=copy.deepcopy(args)
    expected=getattr(oracle,name)(**expected_args);actual=fns[name](**actual_args);comparisons+=1
    if name=='_portability_anchors':expected=(sorted(expected[0]),expected[1],sorted(expected[2]),expected[3]);actual=(sorted(actual[0]),actual[1],sorted(actual[2]),actual[3])
    if clean(actual)!=clean(expected) or (name in mutators and actual_args!=expected_args):native.fail(function=name,args=clean(args),expected=clean(expected),actual=clean(actual),expected_args=clean(expected_args),actual_args=clean(actual_args))
try:
    selected=[];excluded=[]
    exclusions={'test_cache.py':{'test_save_cached_survives_a_winerror_17_replace'},'test_stat_index_portability.py':{'test_legacy_absolute_index_migrates_gracefully'},'test_partial_cache.py':{'test_partial_files_carries_empty_parse_truncation','test_stamped_manifest_excludes_partial_files','test_stamped_manifest_excludes_failed_ast_sources','test_mark_partial_and_partial_source_files','test_partial_source_files_empty_when_unmarked','test_strip_partial_markers_removes_internal_key'},'test_semantic_cache_out_root.py':{'test_extract_corpus_parallel_accepts_cache_root_kwarg'},
        # Not applicable: it changes the output folder between two calls, and this
        # check rebuilds the engine's cache state for each call from the
        # reference's globals, which do not hold the stat index's file path.
        'test_stat_index_multi_root.py':{'test_each_root_gets_its_own_stat_index_file_not_the_first_roots'}}
    for file in ['test_cache.py','test_word_count_cache.py','test_stat_index_portability.py','test_stat_index_husk.py','test_semantic_cache_out_root.py','test_partial_cache.py','test_semantic_cache_basename_recovery.py','test_cache_stale_import_target.py','test_grammar_cache_key.py',
                 # First run here on 14 October 2026.
                 'test_stat_index_multi_root.py']:
        tree=ast.parse((SOURCE/'tests'/file).read_text())
        for n in tree.body:
            if isinstance(n,ast.FunctionDef) and n.name.startswith('test_'):
                target='tests/'+file+'::'+n.name
                if n.name in exclusions.get(file,set()):excluded.append(target)
                else:selected.append(target)
    with contextlib.ExitStack() as stack:
        for n,fn in fns.items():stack.enter_context(patch.object(reference,n,fn))
        tests=upstream_tests(*selected)
    rng=random.Random(2026092357)
    with tempfile.TemporaryDirectory() as temp:
        root=Path(temp)/'repo';(root/'src').mkdir(parents=True);file=root/'src/f.py';file.write_text('source\n');(root/'alias').symlink_to(file);outside=Path(temp)/'other.py';outside.write_text('other\n')
        reference._stat_index_root=None;reference._stat_index_anchor=None;reference._stat_index={};reference._stat_index_dirty=False
        for source,target in [('drop',[]),([], 'drop'),('keep','drop'),('drop','keep')]:
            a={'nodes':[{'id':'keep','source_file':str(file)},{'id':'drop','source_file':str(outside)}],'edges':[{'source':source,'target':target,'source_file':str(file)}]};b=copy.deepcopy(a)
            want=oracle.scope_semantic_result(a,root,[file]);got=fns['scope_semantic_result'](b,root,[file]);comparisons+=1
            if a!=b or want!=got:native.fail(expected=a,actual=b,expected_drop=clean(want),actual_drop=clean(got))
        # 0.9.91 (#2276): an edge attributed to an out-of-scope file stays when both
        # endpoint ids are kept nodes; one to a dropped, unknown or unhashable id
        # does not. Hyperedges keep the plain scope rule.
        for source,target in [('keep','keep2'),('keep2','keep'),('keep','drop'),('drop','keep'),('keep','missing'),('keep',[]),('keep',None)]:
            a={'nodes':[{'id':'keep','source_file':str(file)},{'id':'keep2','source_file':'src/f.py'},{'id':'drop','source_file':str(outside)}],
               'edges':[{'source':source,'target':target,'source_file':str(outside)},{'source':'keep','target':'keep2','source_file':'../other.py'}],
               'hyperedges':[{'id':'h','nodes':['keep','keep2'],'source_file':str(outside)}]};b=copy.deepcopy(a)
            want=oracle.scope_semantic_result(a,root,[file]);got=fns['scope_semantic_result'](b,root,[file]);comparisons+=1
            if a!=b or want!=got:native.fail(expected=a,actual=b,expected_drop=clean(want),actual_drop=clean(got))
        for i in range(400):
            text=rng.choice(['','---\nx: y\n---\nbody','----\nnot metadata\n---','--- \r\nx: y\r\n---\t\r\nbody','\ufffd','a\x1f','---\n---\n'])
            compare('_body_content',{'content':text.encode()+bytes(rng.choices([32,65,255],k=rng.randrange(4)))})
            compare('prompt_fingerprint',{'prompt':rng.choice(['Prompt\r\nLine  \n','Prompt\nLine','a\r b','  é 中 \n'])})
            src=rng.choice([str(file),str(root/'alias'),'src/f.py','./src/f.py',str(outside),'../other.py',str(file).replace('/','\\')])
            compare('_normalize_source_file_value',{'src':src,'root_resolved':root.resolve()});compare('_stat_key_to_relative',{'key':src,'anchor':root});compare('_stat_key_to_absolute',{'key':src,'anchor':root})
            compare('_id_anchor',{'path_str':str(file),'rel_str':'src/f.py'});compare('_portability_anchors',{'path':Path(src),'root':root})
            nid=oracle._id_anchor(str(file),'src/f.py')+'_src_f'
            payload={'nodes':[{'id':nid,'source_file':src,'definition_file':src}],'edges':[{'source':nid,'target':nid+'_x','target_file':str(file),'source_file':str(file)}],'raw_calls':[{'caller_nid':nid,'source_file':src}],'objc_field_types':{'tables':{nid:{'path':str(file)}}},'extra':[nid,str(root),str(root)+'/a\\b']}
            compare('_relativize_source_files_in',{'payload':payload,'root':root});compare('_absolutize_source_files_in',{'payload':payload,'root':root});compare('_relativize_ids_in',{'payload':payload,'path':file,'root':root});oracle._relativize_ids_in(payload,file,root);compare('_absolutize_ids_in',{'payload':payload,'path':file,'root':root})
            compare('_semantic_entry_matches_path',{'result':payload,'path':file,'root':root});compare('_group_has_partial_marker',{'group':{'nodes':[{'_partial':i%3==0}], 'edges':[]}})
        # Real native read counters verify the fast path, including healing an old index.
        reference._stat_index_root=None;reference._stat_index_anchor=None;reference._stat_index={};reference._stat_index_dirty=False
        old=time.time_ns()-60_000_000_000;os.utime(file,ns=(old,old));before=metrics['source_reads'];a=fns['file_hash'](file,root);b=fns['file_hash'](file,root);assert a==b and metrics['source_reads']==before+1
        key=str(file.resolve());reference._stat_index[key].pop('indexed_at_ns');before=metrics['source_reads'];fns['file_hash'](file,root);fns['file_hash'](file,root);assert metrics['source_reads']==before+1
        changed='source2';file.write_text(changed);before=metrics['source_reads'];h=fns['file_hash'](file,root);st=file.stat();file.write_text('changed');os.utime(file,ns=(st.st_atime_ns,st.st_mtime_ns));h2=fns['file_hash'](file,root);assert h!=h2 and metrics['source_reads']==before+2
        fns['_flush_stat_index']()
        # Disk/source-key equality against the original cache implementation.
        for i in range(40):
            payload={'nodes':[{'id':'n'+str(i),'source_file':str(file)}],'edges':[]};oracle._stat_index_root=None;oracle._stat_index={};oracle.save_cached(file,payload,root,kind='semantic',prompt='p'+str(i%3));h=oracle.file_hash(file,root);folder=oracle.cache_dir(root,'semantic',oracle.prompt_fingerprint('p'+str(i%3)));expected=(folder/(h+'.json')).read_bytes();fns['save_cached'](file,payload,root,kind='semantic',prompt='p'+str(i%3));actual=(folder/(h+'.json')).read_bytes();comparisons+=1
            if expected!=actual:native.fail(expected=expected.decode(),actual=actual.decode())
        # Canonical root + walked directory alias: source paths and IDs must
        # relocate together, including when a different checkout hits the cache.
        original=Path(temp)/'original';relocated=Path(temp)/'relocated'
        for directory in (original,relocated):
            (directory/'src').mkdir(parents=True)
            (directory/'src/a.py').write_text('def alpha(): return 1\n')
        walked=Path(temp)/'walked';walked.symlink_to(original,target_is_directory=True)
        aliased=walked/'src/a.py';canonical=original.resolve();shared=Path(temp)/'shared-cache'
        nid=oracle._id_anchor(str(aliased),'src/a.py')+'_src_a_alpha'
        payload={'nodes':[{'id':nid,'source_file':str(aliased)}],'edges':[]}
        compare('_portability_anchors',{'path':aliased,'root':canonical})
        compare('_relativize_ids_in',{'payload':payload,'path':aliased,'root':canonical})
        # Both sides write the one entry of a shared cache root; the native's
        # replaces the reference's, so the reference reads the native-written
        # entry (cache-format compatibility). The entry carries the portable
        # root marker under the engine's name: the reference reads it with the
        # marker translated back, the native reads its own entry unchanged.
        oracle.save_cached(aliased,payload,canonical,cache_root=shared)
        fns['save_cached'](aliased,payload,canonical,cache_root=shared)
        [entry]=[p for p in (shared/oracle._GRAPHIFY_OUT/'cache/ast').rglob('*.json')]
        written=entry.read_bytes();translated=to_upstream(written.decode('utf-8')).encode('utf-8')
        assert written!=translated,'the native entry carries no root marker'
        destination=relocated.resolve()/'src/a.py'
        entry.write_bytes(translated)
        expected=oracle.load_cached(destination,relocated.resolve(),cache_root=shared)
        entry.write_bytes(written)
        actual=fns['load_cached'](destination,relocated.resolve(),cache_root=shared)
        comparisons+=1
        if actual!=expected or actual['nodes'][0]['source_file']!=str(destination):
            native.fail(label='walked_directory_relocated',expected=expected,actual=actual)
        # The standalone process must flush its own global cache on exit.
        exit_root=Path(temp)/'exit';exit_root.mkdir();f=exit_root/'x.txt';f.write_text('native process lifetime')
        child=Native('cache-exit',cwd=exit_root)
        try:
            r=child.result('cache',function='file_hash',arguments={'path':str(f),'root':str(exit_root)})
            assert 'error' not in r and not (exit_root/'graphify-out/cache/stat-index.json').exists()
        finally:child.close()
        assert 'x.txt' in json.loads((exit_root/'graphify-out/cache/stat-index.json').read_text())
    native.receipt('graphify/cache.py',['src/cache.rs','src/cache_paths.rs','src/cache_api.rs'],upstream_tests=tests,operations=counts,independent_comparisons=comparisons,source_reads=metrics['source_reads'],callback_computations=metrics['computed_word_counts'],excluded_original_tests=excluded,adaptations=['AST cache directory grammar fingerprint (#4236): the reference fingerprints its installed tree-sitter distributions, the native the grammar crates it was built with (cache::compiled_grammars, an interface difference). The comparison passes the reference fingerprint in the native state, so both name one versioned directory; the fingerprint function itself stays the reference\'s.','Reviewed portable-cache fix: infer the walked directory alias beside a canonical root, restore marked source paths before plain relative paths, and compare the independent Python adaptation on a cache reused under a second checkout. Native AST schema 7 invalidates affected old records.','Name translation in the walked-directory relocation case: both implementations write the one entry of a shared cache root, the native last. The entry stores the portable root marker under the engine\'s name ($graphicle-root$), so the reference loads the native-written entry with the marker translated back to $graphify-root$ (tools/naming.py to_upstream), and the native loads its own untranslated entry; the two loads must agree.','Native AST cache versions include the native package version to avoid serving incompatible Python parser output. Semantic content/prompt keys retain the original layout.','Rust owns cache state; CacheSession flushes on drop, and the standalone process flushes its shared cache at shutdown. The comparison boundary imports/exports state to preserve original Python fixtures that inspect module globals.','Callback tests use the native lookup/ticket/completion API; the pre-read timestamp survives the callback. Python extraction fixtures supply original extractor payloads only; complete native extraction migration is tracked separately.','Python-specific os.replace mocking is covered by the previously qualified native path replacement tests. The Python read counter fixture is replaced by assertions against actual native source-read counters. Excluded LLM/manifest tests belong to later modules.'])
finally:
    reference._stat_index_dirty=False;oracle._stat_index_dirty=False;native.close()
