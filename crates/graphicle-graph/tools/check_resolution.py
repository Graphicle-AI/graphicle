"""Differential contracts for native file, symbol, import and type resolution."""
import copy
import dataclasses
import functools
import inspect
import json
import sys
from pathlib import Path
from parity import Native, ROOT, SOURCE
from isolated_upstream import run
import graphify.extractors.resolution as ref
import graphify.extract as extraction
import graphify.extractors.models as models
from graphify.rcfile import get_language_overrides

native = Native('resolution')
original = {n:v for n,v in vars(ref).items() if callable(v)}
counts = {}; failures = []; in_reference = False

def serial(v):
    if dataclasses.is_dataclass(v): return serial(dataclasses.asdict(v))
    if isinstance(v, Path): return str(v)
    if isinstance(v, dict): return {str(k): serial(x) for k,x in v.items()}
    if isinstance(v,(tuple,list,set,frozenset)): return [serial(x) for x in v]
    return v

def canonical(v):
    v=serial(v)
    if isinstance(v,dict):
        for k in ('nodes','edges','added'):
            if isinstance(v.get(k),list): v[k].sort(key=lambda x:json.dumps(x,sort_keys=True))
    return v

def compare(name,op,expected,request):
    native.use_working_directory(Path.cwd())
    # The reference's active `.graphifyrc` language overrides (#2961) apply to the native call too.
    native.result('rcfile',function='set_language_overrides',mapping=get_language_overrides())
    actual=native.result(op,**serial(request)); counts[name]=counts.get(name,0)+1
    if canonical(expected)!=canonical(actual):
        failures.append({'name':name,'request':serial(request),'expected':serial(expected),'actual':actual})
        (ROOT/'migration/receipts/resolution-differences.json').write_text(json.dumps(failures,indent=2)+'\n')
        raise AssertionError('Native resolution differs: '+name)
    return actual

def expected_call(name,*args,**kw):
    global in_reference
    old=in_reference; in_reference=True
    try: return original[name](*args,**kw)
    finally: in_reference=old

def install(name,impl):
    @functools.wraps(original[name])
    def wrapped(*args,**kw):
        if in_reference: return original[name](*args,**kw)
        return impl(*args,**kw)
    if hasattr(original[name],'cache_clear'): wrapped.cache_clear=original[name].cache_clear
    setattr(ref,name,wrapped)
    if hasattr(extraction,name): setattr(extraction,name,wrapped)

# Simple value-returning resolver interfaces. The Python wrapper restores Path
# and tuple shape; runtime computation always comes from the native process.
def simple(name,action,keys,*,op='resolution.js'):
    def impl(*args,**kwargs):
        bound=inspect.signature(original[name]).bind(*args,**kwargs); bound.apply_defaults()
        req={dest:bound.arguments[src] for src,dest in keys.items() if src in bound.arguments}
        value=expected_call(name,*args,**kwargs)
        actual=compare(name,op,value,{'action':action,**req})
        def shape(a,b):
            if isinstance(b,Path):return Path(a)
            if isinstance(b,tuple):return tuple(shape(x,y) for x,y in zip(a,b))
            if isinstance(b,list):return [shape(x,y) for x,y in zip(a,b)]
            if isinstance(b,dict):return {k:shape(a[k],v) for k,v in b.items()}
            return a
        return shape(actual,value)
    install(name,impl)

for name,action,key in [
 ('_strip_jsonc','strip_jsonc','text'),('_resolve_js_import_path','resolve_js_import_path','candidate'),
 ('_read_json_config','read_json_config','path'),('_find_js_config','find_js_config','start_dir'),
 ('_load_tsconfig_aliases','aliases','start_dir'),('_load_tsconfig_base_url','base_url','start_dir'),
 ('_find_workspace_root','workspace_root','start_dir'),('_workspace_globs','workspace_globs','root'),
 ('_load_workspace_packages','workspace_packages','start_dir'),('_importer_platform','platform','start_dir'),
 ('_find_js_project_anchor','anchor','start_dir'),('_load_package_imports','package_imports','start_dir'),
 ('_vue_mask_non_script','vue_mask','src')]:
    simple(name,action,{key:'text' if action in ('strip_jsonc','vue_mask') else 'path'})
for name,action in [('_resolve_workspace_import','workspace_import'),('_resolve_package_import','package_import')]:
    simple(name,action,{'raw':'text','start_dir':'path'})
simple('_resolve_js_import_target','import_target',{'raw':'text','str_path':'path'})
simple('_resolve_lua_import_target','lua_target',{'raw_module':'text','str_path':'path'})
simple('_resolve_export_targets','export_targets',{'value':'value','platform':'platform'})
simple('_platform_variants','variants',{'candidate':'path','platform':'platform'})
simple('_package_entry_candidates','package_entries',{'package_dir':'path','subpath':'text','platform':'platform'})
simple('_resolve_tsconfig_alias','resolve_alias',{'raw':'text','aliases':'aliases','base_url':'base'})
simple('_match_tsconfig_alias','match_alias',{'raw':'text','pattern':'pattern'})
simple('_match_subpath_import','match_subpath',{'raw':'text','pattern':'pattern'})
for name,action,keys in [
 ('_source_key','source_key',{'source_file':'source','root':'root'}),
 ('_is_type_like_definition','type_like',{'node':'node'}),
 ('_go_import_path_for_file','go_import_path',{'source_file':'source','root':'root'}),
 ('_resolve_python_module_path','python_module',{'module_name':'module','current_path':'path','root':'root','level':'level'}),
 ('_resolve_python_namespace_dir','python_namespace',{'module_name':'module','current_path':'path','root':'root','level':'level'}),
 ('_probe_python_module_candidate','python_probe',{'candidate':'path'}),
 ('_resolve_c_include_path','c_include',{'raw':'raw','str_path':'path'}),
 ('_pascal_project_root','pascal_root',{'from_path':'path'}),
 ('_pascal_resolve_unit','pascal_unit',{'from_path':'path','unit_name':'name'}),
 ('_pascal_resolve_class','pascal_class',{'from_path':'path','class_name':'name'})]:
    simple(name,action,keys,op='resolution')

def mutation(name,action):
    def impl(*args,**kwargs):
        bound=inspect.signature(original[name]).bind(*args,**kwargs);bound.apply_defaults(); d=bound.arguments
        nodes=d.get('all_nodes',d.get('nodes'));edges=d.get('all_edges',d.get('edges'));raw=d.get('raw_calls')
        memo={}
        clones=copy.deepcopy(bound.arguments,memo)
        owners={id(memo[id(item)]): item for values in (nodes,edges,raw) if values is not None for item in values if id(item) in memo}
        expected=expected_call(name,**clones)
        request={'action':action,'nodes':nodes or [],'edges':edges or [],'paths':d.get('paths',[]),'per_file':d.get('per_file',[]),'root':d.get('root',Path.cwd()),'raw_calls':raw or [],'context_nodes':d.get('resolution_context_nodes') or [],'context_edges':d.get('resolution_context_edges') or [],'ambiguous_python_modules':sorted(d.get('ambiguous_python_modules') or [])}
        if 'facts' in d: request['facts']=d['facts']
        output={'nodes':clones.get('all_nodes',clones.get('nodes')) or [],'edges':clones.get('all_edges',clones.get('edges')) or []}
        if action in ('python_imports','java_imports','java_types','php_types','go_types'):output['added']=expected or []
        if action=='disambiguate':output['raw_calls']=clones['raw_calls']
        actual=compare(name,'resolution',output,request)
        def sync(dest, key, originals):
            if dest is None:return
            by_value={json.dumps(serial(x),sort_keys=True):x for x in originals}
            result=[]
            for value in actual[key]:
                clone=by_value.get(json.dumps(value,sort_keys=True))
                prior=owners.get(id(clone))
                if prior is not None:prior.clear();prior.update(value);result.append(prior)
                else:result.append(value)
            dest[:]=result
        sync(nodes,'nodes',clones.get('all_nodes',clones.get('nodes')) or [])
        sync(edges,'edges',clones.get('all_edges',clones.get('edges')) or [])
        if raw is not None:sync(raw,'raw_calls',clones['raw_calls'])
        return actual.get('added') if action in ('python_imports','java_imports') else None
    install(name,impl)
for name,action in [('_disambiguate_colliding_node_ids','disambiguate'),('_merge_decl_def_classes','merge_decl_def'),('_apply_symbol_resolution_facts','apply_facts'),('_augment_symbol_resolution_edges','augment'),('_resolve_cross_file_imports','python_imports'),('_resolve_cross_file_java_imports','java_imports'),('_resolve_java_type_references','java_types'),('_resolve_php_type_references','php_types'),('_resolve_go_type_references','go_types')]:mutation(name,action)

def collect(name,action):
    def impl(*args,**kw):
        bound=inspect.signature(original[name]).bind(*args,**kw);bound.apply_defaults(); d=bound.arguments
        clone=copy.deepcopy(d['facts']); expected_call(name,**{**d,'facts':clone})
        actual=compare(name,'resolution',clone,{'action':action,'paths':d['paths'],'root':d.get('root',Path.cwd()),'facts':d['facts'],'ambiguous_python_modules':sorted(d.get('ambiguous_python_modules') or [])})
        fact_types={'declarations':'_SymbolDeclarationFact','imports':'_SymbolImportFact','aliases':'_SymbolAliasFact','exports':'_SymbolExportFact','star_exports':'_StarExportFact','namespace_exports':'_NamespaceExportFact','uses':'_SymbolUseFact'}
        for field,items in actual.items():
            if field=='module_imports':items=[(Path(a),Path(b),c,e) for a,b,c,e in items]
            else:
                cls=getattr(models,fact_types[field]);items=[cls(**{k:Path(v) if k.endswith('_path') and v is not None else v for k,v in item.items()}) for item in items]
            getattr(d['facts'],field)[:]=items
    install(name,impl)
collect('_collect_js_symbol_resolution_facts','js_facts');collect('_collect_python_symbol_resolution_facts','python_facts')

class NativeCache(dict):
    def clear(self):
        super().clear()
        native.use_working_directory(Path.cwd())
        native.result('resolution.js', action='clear')
for cache_name in ('_TSCONFIG_ALIAS_CACHE','_TSCONFIG_BASEURL_CACHE','_PACKAGE_IMPORTS_CACHE','_WORKSPACE_PACKAGE_CACHE'):
    cache=NativeCache(getattr(ref,cache_name))
    setattr(ref,cache_name,cache)
    if hasattr(extraction,cache_name):setattr(extraction,cache_name,cache)

names=[n for n,v in vars(ref).items() if n in original and v is not original[n]]
paths=[str(p.relative_to(SOURCE)) for p in sorted((SOURCE/'tests').glob('test_*.py')) if any(n in p.read_text() for n in names) or p.name in ('test_extract.py','test_languages.py','test_multilang.py','test_astro_import_ids.py','test_go_qualified_type_refs.py','test_php_type_resolution.py','test_java_type_resolution.py')]
if '--test' in sys.argv:paths=[sys.argv[sys.argv.index('--test')+1]]
# test_absolute_path_lookup_skips_getcwd pins the reference's memo key (no getcwd for an
# absolute path, a speed detail): the native memo keys absolute paths without the working
# directory already (resolution_paths::resolve_cached), and the comparison itself reads it.
code,tests=run(paths,ignore=['test_extract_generic_surfaces_tree_sitter_version_mismatch_hint','test_absolute_path_lookup_skips_getcwd'])
native.close()
(ROOT/'migration/receipts/resolution-differences.json').write_text(json.dumps(failures,indent=2)+'\n')
assert code==0 and not failures, 'Resolution comparison failures'
native.receipt('graphify/extractors/resolution.py',[str(p.relative_to(ROOT)) for p in sorted((ROOT/'src/extractors').glob('resolution*.rs'))]+['src/extractors/pascal_paths.rs'],checked=counts,upstream_tests=tests)
