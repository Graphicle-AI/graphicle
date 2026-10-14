#!/usr/bin/env python3
"""Use unchanged Graphify tests and Robot's parser as an independent oracle."""
import inspect,json,os,random,sys,tempfile,types
from pathlib import Path
from parity import Native,SOURCE
import pytest
import graphify.extractors.robot as reference
import graphify.extract as facade
from robot.variables import is_assign
from robot.conf.languages import Language,Languages
oracle=types.ModuleType('reference_robot');oracle.__package__='graphify.extractors'
exec(compile((SOURCE/'graphify/extractors/robot.py').read_text(),str(SOURCE/'graphify/extractors/robot.py'),'exec'),oracle.__dict__)
native=Native('robot');saved={};cases=0;helpers=0;languages_checked=[]
def call(function,**kwargs):
    native.use_working_directory(os.getcwd())
    return native.result('robot',function=function,**{k:str(v) if isinstance(v,Path) else v for k,v in kwargs.items()})
def wrap(name):
    sig=inspect.signature(getattr(reference,name))
    def invoke(*args,**kwargs):
        b=sig.bind(*args,**kwargs);b.apply_defaults();r=call(name,**b.arguments)
        return Path(r) if name=='_resolve_robot_import' and r is not None else r
    return invoke
class Reports:
    passed=0;skipped=0;deselected=0
    def pytest_collection_modifyitems(self,config,items):
        selected=[];other=[]
        for i in items:
            (selected if i.name.startswith('test_robot_') and i.name!='test_robot_extractor_degrades_gracefully_without_robotframework' else other).append(i)
        items[:]=selected;self.deselected=len(other);config.hook.pytest_deselected(items=other)
    def pytest_runtest_logreport(self,report):
        if report.when=='call' and report.passed:self.passed+=1
        if report.skipped:self.skipped+=1
reports=Reports()
def compare_source(path,source):
    global cases
    path.write_text(source,encoding='utf-8');expected=oracle.extract_robot(path);actual=call('extract_robot',path=path);cases+=1
    if expected!=actual:native.fail(path=str(path),source=source,expected=expected,actual=actual)
try:
    for name in ('extract_robot','_kw_id','_resolve_robot_import'):
        saved[name]=getattr(reference,name);setattr(reference,name,wrap(name))
    saved_facade=facade.extract_robot;facade.extract_robot=reference.extract_robot
    status=pytest.main(['-q','-p','no:cacheprovider',str(SOURCE/'tests/test_languages.py')],plugins=[reports]);assert status==0,status
    for name,f in saved.items():setattr(reference,name,f)
    rng=random.Random(71)
    for base in ('Suite/suite.robot','suite.robot','/repo/sub/suite.robot','./folder/../suite.robot'):
        for raw in ('common.resource','../vars.py','${CURDIR}/../common.resource','${Cur_Dir}${/}foo.robot','${EXEC DIR}/resource/common.robot','${ROOT}/missing','${curdir}/%{HOME}/x','',r'Library\\libs.py','/foo/../bar','${CURDIR}../x'):
            expected=oracle._resolve_robot_import(raw,Path(base));actual=call('_resolve_robot_import',raw=raw,source_path=base);helpers+=1
            if (str(expected) if expected is not None else None)!=actual:native.fail(function='resolve',source=base,raw=raw,expected=str(expected),actual=actual)
    alphabet='aBb _é中.$/{x}[]=\\\t'
    for _ in range(800):
        name=''.join(rng.choice(alphabet) for _ in range(rng.randrange(60)))
        expected=oracle._kw_id(name);actual=call('_kw_id',name=name);helpers+=1
        if expected!=actual:native.fail(function='kw_id',name=name,expected=expected,actual=actual)
        assignment=rng.choice(['${a}','@{args}','&{kw}','${a${b}}','${}','${a\\}}','not a variable','${a}[one][${index}]','${name: int}'])+rng.choice(['','=',' =','  =','[a]','[a[b]]','[bad','[\\]]','a',' = '])
        expected=is_assign(assignment,allow_assign_mark=True,allow_nested=True,allow_items=True);actual=call('is_assign',text=assignment);helpers+=1
        if expected!=actual:native.fail(function='is_assign',text=assignment,expected=expected,actual=actual)
    with tempfile.TemporaryDirectory() as temp:
        root=Path(temp)
        snippets=[
            '    Open Session\n    Given Open Session\n    Library.Log    argument\n',
            '    ${value} =    Open Session\n    ${dict}[name]    Set Value\n',
            '    FOR    ${i}    IN RANGE    10\n        Open Session\n        IF    $i\n            Close Session\n        ELSE\n            Continue Loop\n        END\n    END\n',
            '    TRY\n        Connect\n    EXCEPT    Problem\n        Recover\n    FINALLY\n        Close Session\n    END\n',
            '    IF    1    Open Session    ELSE IF    2    Close Session    ELSE    Log    none\n',
            '    ${result}=    IF    1    Open Session    ELSE    Close Session\n',
            '    [Setup]    Prepare\n    [Teardown]    Cleanup\n    [Documentation]    Ignored Words\n    Open Session\n',
            '    [Template]    Open Session\n    alice\n    bob\n',
            '    [Template]    NONE\n    Open Session\n    [Template]    Ignored Duplicate\n',
            '    Open Session\n    ...    continued argument\n    # ignored\n    Close Session    # another comment\n',
            '    VAR    ${x}    1\n    WHILE    $x\n        BREAK\n    END\n    GROUP    Example\n        Open Session\n    END\n',
            '    FOR    ${i}    IN    1\n        [Setup]    Not A Setting Inside Loop\n        Open Session\n    END\n',
        ]
        for i in range(240):
            sep=rng.choice(['    ','  ','\t','\u00a0\u00a0'])
            settings='*** Settings ***\nLibrary    Collections\nLibrary    SeleniumLibrary\nResource    ${CURDIR}/common.resource\nVariables    vars.py    arg\nSuite Setup    Prepare\n'
            if i%5==0:settings+='Test Template    Open Session\n'
            if i%11==0:settings+='Test Template    Ignored\nSuite Setup    Another setup\nResource    bad    too many\n'
            body=rng.choice(snippets)+rng.choice(snippets)
            content=settings+'*** Test Cases ***\nCase One\n'+body+'*** Keywords ***\nOpen Session\n    [Arguments]    ${user}\n    Log    opening\nClose Session\n    [Setup]    Ready\n    Log    closing\n'
            content=content.replace('    ',sep)
            if i%7==0:content=content.replace('*** Test Cases ***','*** Tasks ***')
            if i%13==0:content='\ufeff'+content.replace('\n','\r\n')
            compare_source(root/f'suite{i}.robot',content)
        for i,body in enumerate(snippets):
            compare_source(root/f'keywords{i}.resource','*** Keywords ***\nOpen Session\n'+body+'*** Test Cases ***\nMust Be Ignored\n    Wrong\n')
        pipes=['| *** Settings *** |','| Resource | common.resource |','| Suite Setup | Prepare |','| *** Test Cases *** |','| Case | Open Session |','| | Close Session |','| | IF | 1 | Close Session | ELSE | Open Session |','| *** Keywords *** |','| Open Session |','| | Log | hi |']
        compare_source(root/'pipes.robot','\n'.join(pipes))
        for cls in Language.__subclasses__():
            lang=cls();cfg=Languages(lang,add_english=False)
            header=lambda v:next(k for k,x in cfg.headers.items() if x==v)
            setting=lambda v:next(k for k,x in cfg.settings.items() if x==v)
            content=f'Language: {lang.code}\n*** {header("Settings")} ***\n{setting("Library")}    SeleniumLibrary\n*** {header("Test Cases")} ***\nCase\n    [{setting("Setup")}]    Open Session\n    Close Session\n*** {header("Keywords")} ***\nOpen Session\n    Log    hi\n'
            compare_source(root/f'{lang.code}.robot',content);languages_checked.append(lang.code)
        missing=root/'missing.robot';a=oracle.extract_robot(missing);b=call('extract_robot',path=missing)
        assert a['nodes']==b['nodes'] and a['edges']==b['edges'] and a.get('error') and b.get('error')
    native.receipt('graphify/extractors/robot.py',['src/extractors/robot.rs','src/assets/robot/languages.json'],original_tests={'file':'tests/test_languages.py','passed':reports.passed,'skipped':reports.skipped,'excluded':'Python optional-dependency import-hook subprocess; native parser is built in.'},full_extraction_comparisons=cases,helper_comparisons=helpers,built_in_languages_checked=languages_checked,reference_robot_version=__import__('robot').__version__,adaptations=['Robot Framework source tokenization and graph-relevant classification execute in Rust instead of robot.api. Its source parser and built-in language tables are Apache-2.0; notices retained.','The optional Python-import failure is inapplicable. OS read errors retain the file node and error field with native error text. Custom Python language modules are not executed; unknown language names remain unrecognized, as with an unavailable reference module.','The Graphify visitor remains English-only for BDD keyword prefix stripping, matching the pinned implementation. No Robot test execution engine is bundled or claimed.'])
finally:
    for name,f in saved.items():setattr(reference,name,f)
    if 'saved_facade' in globals():facade.extract_robot=saved_facade
    native.close()
