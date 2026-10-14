#!/usr/bin/env python3
"""Exercise original slicing cases through native, character-indexed readers."""
import dataclasses, inspect, json, os, random, shutil, sys, tempfile, types
from pathlib import Path
from parity import Native,SOURCE
import graphify.file_slice as fs
import graphify.detect as det
import pytest

native=Native('file_slice'); originals={};comparisons=0
oracle=types.ModuleType('reference_file_slice');oracle.__package__='graphify'
sys.modules[oracle.__name__]=oracle
exec(compile((SOURCE/'graphify/file_slice.py').read_text(),str(SOURCE/'graphify/file_slice.py'),'exec'),oracle.__dict__)
def plain(value):
    if dataclasses.is_dataclass(value):return plain(dataclasses.asdict(value))
    if isinstance(value,Path):return str(value)
    if isinstance(value,(tuple,list)):return [plain(v) for v in value]
    if isinstance(value,dict):return {k:plain(v) for k,v in value.items()}
    return value
def call(name,**kwargs):
    native.use_working_directory(os.getcwd());return native.result('file_slice',function=name,**plain(kwargs))
def unit(value):return fs.FileSlice(**{**value,'path':Path(value['path'])}) if isinstance(value,dict) else Path(value)
def wrap(name):
    original=getattr(fs,name);originals[name]=original;signature=inspect.signature(original)
    def invoke(*args,**kwargs):
        bound=signature.bind(*args,**kwargs);bound.apply_defaults();params=dict(bound.arguments)
        if 'fs' in params:params['slice']=params.pop('fs')
        value=call(name,**params)
        if name=='expand_oversized_files':return [unit(v) for v in value]
        if name=='bisect_slice':return tuple(unit(v) for v in value) if value is not None else None
        if name=='unit_path':return Path(value)
        if name=='slice_boundaries':return [tuple(v) for v in value]
        return value
    return invoke
excluded={'test_read_files_keys_every_slice_to_parent_path','test_estimate_tokens_for_slice_scales_with_range','test_partition_keeps_slices_as_text','test_pack_chunks_handles_slices','test_the_tail_reaches_the_prompt'}
class Reports:
    passed=0;skipped=0;deselected=[]
    def pytest_collection_modifyitems(self,config,items):
        omit=[i for i in items if i.originalname in excluded];items[:]=[i for i in items if i.originalname not in excluded];self.deselected=[i.nodeid for i in omit];config.hook.pytest_deselected(items=omit)
    def pytest_runtest_logreport(self,report):
        if report.when=='call' and report.passed:self.passed+=1
        if report.skipped:self.skipped+=1
reports=Reports();old_pdf=det.extract_pdf_text
try:
    for name in ('unit_path','is_splittable_text','_best_cut','slice_boundaries','expand_oversized_files','unit_source_text','read_slice_text','bisect_slice'):setattr(fs,name,wrap(name))
    # PDF fixture expectations must address the same converted string that
    # native slice offsets index. PDF parser differences are recorded separately
    # in detect.json rather than hidden by comparing binary-container offsets.
    det.extract_pdf_text=lambda p:native.result('detect',function='extract_pdf_text',path=str(p))['value']
    files=['tests/test_file_slice.py','tests/test_pdf_slicing.py']
    status=pytest.main(['-q','-p','no:cacheprovider',*[str(SOURCE/f) for f in files]],plugins=[reports]);assert status==0,status
    for name,value in originals.items():setattr(fs,name,value)
    det.extract_pdf_text=old_pdf
    rng=random.Random(62)
    for i in range(80):
        text=''.join(rng.choice(['word ','日本語','é','😀','\n','\n\n','\n# H\n']) for _ in range(rng.randrange(1,70)))
        for maximum in [1,2,17,100,1000]:
            expected=plain(oracle.slice_boundaries(text,maximum));actual=call('slice_boundaries',text=text,max_chars=maximum);comparisons+=1
            if actual!=expected:native.fail(text=text,max_chars=maximum,expected=expected,actual=actual)
            assert ''.join(text[a:b] for a,b in actual)==text
    with tempfile.TemporaryDirectory() as temporary:
        root=Path(temporary);p=root/'unicode.md';p.write_text('日本語\n😀é\nline 3\n')
        for start,end in [(0,4),(-5,-1),(-500,500),(500,600),(8,3)]:
            s=fs.FileSlice(p,start,end,0,1);expected=fs.read_slice_text(s);actual=call('read_slice_text',slice=s);comparisons+=1
            assert actual==expected,(actual,expected)
        from reportlab.pdfgen.canvas import Canvas
        source=root/'source.pdf';canvas=Canvas(str(source));canvas.drawString(20,720,'Original PDF text');canvas.save()
        call('reset');first=call('unit_source_text',path=source);second=call('unit_source_text',path=source)
        assert first==second and call('stats')=={'entries':1,'pdf_extractions':1}
        canvas=Canvas(str(source));canvas.drawString(20,720,'Changed and longer PDF evidence');canvas.save()
        rewritten=call('unit_source_text',path=source);assert 'Changed and longer PDF evidence' in rewritten and rewritten!=first
        assert call('stats')['pdf_extractions']==2
        call('reset')
        for index in range(65):
            p=root/f'copy-{index}.pdf';shutil.copyfile(source,p);assert call('unit_source_text',path=p)==rewritten
        assert call('stats')=={'entries':1,'pdf_extractions':65}
        # Invalid nonpositive limits return an explicit error instead of entering
        # the upstream non-advancing loop on nonempty text.
        invalid=native.call('file_slice',function='slice_boundaries',text='nonempty',max_chars=0);assert not invalid['ok']
    native.receipt('graphify/file_slice.py',['src/file_slice.rs'],original_tests={'files':files,'passed':reports.passed,'skipped':reports.skipped,'deselected':reports.deselected},unicode_boundary_and_slice_comparisons=comparisons,pdf_cache={'repeat_reads':1,'rewrite_reparsed':True,'capacity':64,'whole_cache_eviction_at':65},
        adaptations=['Slices use Unicode character offsets, independent of UTF-8 byte length. Native frozen records expose read-only accessors.',
        'PDF tests use the native discovery converter consistently for offset construction and reading. Its raw whitespace differences from pypdf are preserved in the discovery receipt.',
        'Nonpositive limits on nonempty text return an error rather than reproducing an infinite loop. Original llm.py prompt/packing tests remain assigned to that unported module.',
        'Converted text uses shared immutable Rust strings with a bounded 64-entry cache keyed by path, size and nanosecond mtime.'])
finally:
    for name,value in originals.items():setattr(fs,name,value)
    det.extract_pdf_text=old_pdf;native.close()
