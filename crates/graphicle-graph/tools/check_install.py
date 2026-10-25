"""Installer helper parity and isolated native installation round trips."""
import functools, inspect, json, os, platform, sys, tempfile, tomllib
from pathlib import Path
from parity import BINARY, Native, ROOT, SOURCE
from isolated_upstream import run
from naming import NATIVE_BIN, to_native, to_upstream
import graphify.install as ref

native = Native('install'); counts = {}
# The skill stamp records the engine's package version.
VERSION = tomllib.loads((ROOT/'Cargo.toml').read_text())['package']['version']
EXECUTABLE = f'/native bin/{NATIVE_BIN}'
actions = {
    '_canonical_platform': ('canonical', {'platform_name':'platform'}),
    '_replace_or_append_section': ('replace_section', {'content':'content','marker':'marker','new_section':'new_section'}),
    '_remove_marker_section': ('remove_section', {'content':'content','marker':'marker','boundary_prefix':'boundary'}),
    '_strip_json_comments': ('strip_json_comments', {'raw':'raw'}),
    '_load_json_like': ('load_json_like', {'config_file':'path'}),
    '_platform_skill_destination': ('destination', {'platform_name':'platform'}),
    '_project_scope_root': ('scope_root', {'path':'path','project_dir':'project_dir'}),
}
def serial(v):
    if isinstance(v, Path): return str(v)
    if isinstance(v, dict): return {str(k):serial(x) for k,x in v.items()}
    if isinstance(v,(list,tuple)): return [serial(x) for x in v]
    return v
for name,(action,fields) in actions.items():
    original = getattr(ref,name); signature=inspect.signature(original)
    @functools.wraps(original)
    def wrapper(*args,_name=name,_action=action,_fields=fields,_original=original,_signature=signature,**kwargs):
        bound=_signature.bind(*args,**kwargs);bound.apply_defaults(); a=bound.arguments
        request={'action':_action,**{dst:serial(a[src]) for src,dst in _fields.items()}}
        if _name=='_platform_skill_destination':
            request['options']={'home':str(Path.home()),'project':a['project'],'project_dir':str(a.get('project_dir') or '.'),'windows':platform.system()=='Windows','claude_config_dir':os.environ.get('CLAUDE_CONFIG_DIR'),'local_appdata':os.environ.get('LOCALAPPDATA')}
        expected=_original(*args,**kwargs);native.use_working_directory(Path.cwd());actual=native.result('install',**request)
        counts[_name]=counts.get(_name,0)+1
        # Skill folders and file names compare after the name translation; the
        # unchanged installer flows and tests continue with the reference names.
        if to_upstream(actual)!=serial(expected):native.fail(name=_name,request=request,expected=serial(expected),actual=actual)
        return Path(to_upstream(actual)) if isinstance(expected,Path) else to_upstream(actual)
    setattr(ref,name,wrapper)

# 0.9.86 (#4329): --purge never deletes the project, home or a root. The
# reference's guard is compared with the native one wherever the tests or
# uninstall_all consult it (Path.home() is the tests' sandbox home).
original_purge_reason=ref._unsafe_purge_reason
@functools.wraps(original_purge_reason)
def purge_reason(out,project_dir):
    expected=original_purge_reason(out,project_dir);native.use_working_directory(Path.cwd())
    actual=native.result('install',action='unsafe_purge_reason',out=str(out),project_dir=str(project_dir),options={'home':str(Path.home())})
    counts['_unsafe_purge_reason']=counts.get('_unsafe_purge_reason',0)+1
    if actual!=expected:native.fail(name='_unsafe_purge_reason',out=str(out),project_dir=str(project_dir),home=str(Path.home()),expected=expected,actual=actual)
    return actual
ref._unsafe_purge_reason=purge_reason

# 0.9.95 (#4513): references/ is staged under a random name, so a symlink
# pre-created at the old fixed staging name redirects nothing. Its two upstream
# cases run the native staging (the skill folder keeps the reference's name; the
# native install writes no name of its own here).
def install_refs(skill_dst,refs_src):
    native.use_working_directory(Path.cwd());native.result('install',action='install_refs',path=str(skill_dst),source=str(refs_src))
    counts['_install_skill_references']=counts.get('_install_skill_references',0)+1
ref._install_skill_references=install_refs

# #3286/#4513: the version stamp is written to a private temporary file renamed
# over the stamp itself, so a symlink there is replaced, never followed. Every
# reference flow, and test_atomic_version_stamp.py, writes it natively (under
# the reference's stamp name).
def write_version_stamp(skill_dst,version):
    native.use_working_directory(Path.cwd());native.result('install',action='write_version',path=str(skill_dst),version=version,name='.graphify_version')
    counts['_write_version_stamp']=counts.get('_write_version_stamp',0)+1
ref._write_version_stamp=write_version_stamp
STAMP_NOT_APPLICABLE={'tests/test_atomic_version_stamp.py::test_write_version_stamp_preserves_existing_on_replace_failure':
    "Patches the reference's os.replace to fail; the engine's rename is native and cannot be patched from Python. "
    "The paths.rs unit test replace_file_replaces_a_link_and_keeps_the_old_file_on_failure fails the rename natively instead."}

def roundtrips():
    platforms=native.result('install',action='platforms')
    with tempfile.TemporaryDirectory(prefix='native-install-') as tmp:
        root=Path(tmp)
        for windows in [False,True]:
            for project in [False,True]:
                for name in platforms:
                    base=root/str(windows)/str(project)/name; home=base/'home'; p=base/'project';p.mkdir(parents=True);home.mkdir()
                    options={'home':str(home),'project_dir':str(p),'project':project,'windows':windows,'claude_config_dir':None,'local_appdata':None,'executable':EXECUTABLE}
                    sentinel='## Handwritten\nKeep this text.\n'
                    for file in ['CLAUDE.md','AGENTS.md','GEMINI.md','CODEBUDDY.md']:(p/file).write_text(sentinel)
                    native.result('install',action='install',platform=name,options=options)
                    native.result('install',action='install',platform=name,options=options)
                    actual_options={**options,'project':True} if name=='kiro' else options
                    if name!='cursor':
                        dst=Path(native.result('install',action='destination',platform=name,options=actual_options))
                        assert dst.is_file(),(name,options,dst)
                        content=dst.read_text(); assert f'{NATIVE_BIN} extract' in content
                        assert (dst.parent/'references/workflows.md').is_file()
                        assert (dst.parent/'.graphicle_version').read_text()==VERSION
                        assert not dst.with_suffix('.md.bak').exists(),(name,'unchanged reinstall created backup')
                        dst.write_text(content+'\nCUSTOM LOCAL EDIT\n')
                        native.result('install',action='copy_skill',platform=name,options=actual_options)
                        assert 'CUSTOM LOCAL EDIT' in dst.with_suffix('.md.bak').read_text()
                        assert 'CUSTOM LOCAL EDIT' not in dst.read_text()
                    native.result('install',action='uninstall',platform=name,options=options)
                    native.result('install',action='uninstall',platform=name,options=options)
                    if name!='cursor':assert not dst.exists()
                    for file in ['CLAUDE.md','AGENTS.md','GEMINI.md','CODEBUDDY.md']:assert 'Keep this text.' in (p/file).read_text(),(name,file)
                    counts['native_roundtrip']=counts.get('native_roundtrip',0)+1
        # Corrupt settings must not be destroyed, and unrelated valid hooks survive.
        for name,rel in [('claude','.claude/settings.json'),('codex','.codex/hooks.json'),('gemini','.gemini/settings.json'),('codebuddy','.codebuddy/settings.json')]:
            p=root/'settings'/name;f=p/rel;f.parent.mkdir(parents=True); f.write_text('{broken')
            options={'home':str(root/'unused'),'project_dir':str(p),'project':True,'executable':EXECUTABLE}
            failed=native.call('install',action='hook',platform=name,options=options)
            assert not failed['ok'] and f.read_text()=='{broken',failed
            event='BeforeTool' if name=='gemini' else 'PreToolUse'
            custom={'matcher':'Custom','hooks':[{'type':'command','command':'echo keep'}]}
            settings={'other':{'enabled':True},'hooks':{event:[custom]}}
            f.write_text(json.dumps(settings));before=f.read_bytes()
            native.result('install',action='hook',platform=name,options=options)
            assert f.with_name(f.name+'.graphicle-bak').read_bytes()==before
            native.result('install',action='hook',platform=name,options=options,remove=True)
            assert json.loads(f.read_text())==settings
            counts['settings_preserved']=counts.get('settings_preserved',0)+1

def symlinked_instructions():
    """#3805: uninstall edits a symlinked instructions file through the link and
    keeps the link; an emptied regular file is still deleted. Compared with the
    reference uninstall functions on identical trees."""
    import contextlib, io
    cases=[('.claude/CLAUDE.md','_SKILL_REGISTRATION_MARKER','claude',ref._remove_claude_skill_registration,'uninstall'),
           ('CLAUDE.md','_CLAUDE_MD_MARKER','claude',ref.claude_uninstall,'configure'),
           ('GEMINI.md','_GEMINI_MD_MARKER','gemini',ref.gemini_uninstall,'configure'),
           ('.github/copilot-instructions.md','_VSCODE_INSTRUCTIONS_MARKER','vscode',ref.vscode_uninstall,'configure'),
           ('AGENTS.md','_AGENTS_MD_MARKER','agents',ref._agents_uninstall,'configure'),
           ('CODEBUDDY.md','_CODEBUDDY_MD_MARKER','codebuddy',ref.codebuddy_uninstall,'configure')]
    with tempfile.TemporaryDirectory(prefix='native-install-symlink-') as tmp:
        root=Path(tmp);previous=os.environ.get('HOME');os.environ['HOME']=str(root/'home');(root/'home').mkdir()
        try:
            for index,(rel,marker_name,platform_name,uninstall,action) in enumerate(cases):
                marker=getattr(ref,marker_name)
                for linked in [True,False]:
                    results={}
                    for side in ['python','native']:
                        base=root/side/str(index)/str(linked);project=base/'project';path=project/rel;path.parent.mkdir(parents=True)
                        target=base/'dotfiles/rules.md'
                        # Each implementation's section carries its own marker.
                        section=f'{marker if side=="python" else to_native(marker)}\n\ngraphify rules\n'
                        if linked:target.parent.mkdir(parents=True);target.write_text(section);path.symlink_to(target)
                        else:path.write_text(section)
                        if side=='python':
                            with contextlib.redirect_stdout(io.StringIO()):uninstall(project)
                        else:
                            home=base/'home';home.mkdir()
                            options={'home':str(home),'project_dir':str(project),'project':True,'windows':False,'claude_config_dir':None,'local_appdata':None,'executable':EXECUTABLE}
                            if action=='uninstall':native.result('install',action='uninstall',platform=platform_name,options=options)
                            else:native.result('install',action='configure',platform=platform_name,options=options,remove=True)
                        results[side]={'is_symlink':path.is_symlink(),'exists':path.exists(),'resolves_to_target':linked and path.resolve()==target.resolve(),
                                       'target':target.read_text() if target.exists() else None}
                    if results['python']!=results['native']:native.fail(case=rel,linked=linked,expected=results['python'],actual=results['native'])
                    if linked:assert results['native']['is_symlink'] and results['native']['resolves_to_target'],(rel,results)
                    else:assert not results['native']['exists'],(rel,results)
                    counts['symlinked_uninstall' if linked else 'emptied_file_uninstall']=counts.get('symlinked_uninstall' if linked else 'emptied_file_uninstall',0)+1
        finally:
            if previous is None:os.environ.pop('HOME',None)
            else:os.environ['HOME']=previous

def pi_guard():
    """0.9.89 `pi guard install|uninstall|path` against the reference
    dispatcher, in the three contracts of tests/test_pi_guard_install.py:
    `path` names a complete package (the reference's files with the program
    and extension names translated, every manifest extension a file inside
    it); install and uninstall hand that folder to `pi install|remove` and exit
    with Pi's status; without `pi` on PATH both exit 1 with the same error and
    nothing is written."""
    import contextlib, io, subprocess
    from types import SimpleNamespace
    from unittest import mock
    package=Path(ref.__file__).resolve().parent/'pi'
    def upstream_text(text):
        # The native files are upstream's after the name translation; their
        # two header lines name the upstream release and file.
        lines=text.split('\n')
        if lines[0].startswith('// Adapted from Graphify'):lines=lines[2:]
        text='\n'.join(lines).replace('graphicle-graph','graphify').replace('graphicle','graphify')
        return text.replace('the package ships','the wheel ships').replace('to the installed graphify hook-guard CLI',"to Graphify's installed hook-guard CLI")
    def reference(action,which):
        seen={};out=io.StringIO();err=io.StringIO()
        def host(argv,*,check):
            seen['argv']=list(argv);seen['package']=(Path(argv[2])/'package.json').is_file()
            return SimpleNamespace(returncode=23)
        with tempfile.TemporaryDirectory(prefix='reference-pi-guard-') as work,contextlib.chdir(work),\
             mock.patch.object(sys,'argv',['graphify','pi','guard',action]),mock.patch('graphify.install.shutil.which',lambda name:which),\
             mock.patch('subprocess.run',host),contextlib.redirect_stdout(out),contextlib.redirect_stderr(err):
            try:ref.dispatch_install_cli('pi');code=0
            except SystemExit as stop:code=stop.code
            written=sorted(p.name for p in Path(work).iterdir())
        return {'code':code,'stderr':err.getvalue(),'verb':(seen.get('argv') or [None,None])[1],'package':seen.get('package'),'written':written},out.getvalue()
    with tempfile.TemporaryDirectory(prefix='native-pi-guard-') as tmp:
        root=Path(tmp);bindir=root/'bin';empty=root/'empty';bindir.mkdir();empty.mkdir();record=root/'argv'
        (bindir/'pi').write_text(f"#!/bin/sh\nprintf '%s\\n' \"$@\" > '{record}'\nexit 23\n");(bindir/'pi').chmod(0o755)
        def run_native(action,host):
            home=root/f'home-{action}-{host}';work=root/f'work-{action}-{host}';home.mkdir();work.mkdir();record.unlink(missing_ok=True)
            done=subprocess.run([str(BINARY),'pi','guard',action],cwd=work,capture_output=True,text=True,timeout=120,
                                env={'HOME':str(home),'PATH':f'{bindir}:/usr/bin:/bin' if host else str(empty),'LANG':'C.UTF-8'})
            argv=record.read_text().splitlines() if record.exists() else [None,None]
            folder=home/'.graphicle-graph'/'pi'
            if host and argv[1] is not None:assert Path(argv[1])==folder,(action,argv)
            return {'code':done.returncode,'stderr':done.stderr,'verb':argv[0] if host else None,'package':(Path(argv[1])/'package.json').is_file() if argv[1] else None,
                    'written':sorted(p.name for p in work.iterdir())},done.stdout,home
        # path: the same package, translated.
        expected,shown=reference('path',None)
        assert Path(shown.strip())==package,(shown,package)
        actual,shown,home=run_native('path',False)
        folder=Path(shown.strip())
        if folder!=home/'.graphicle-graph'/'pi' or actual['code']!=expected['code']:native.fail(case='pi guard path',expected=expected,actual=actual,shown=shown)
        names=sorted(p.name for p in package.iterdir() if p.is_file())
        if sorted(p.name for p in folder.iterdir())!=names:native.fail(case='pi guard path files',expected=names,actual=sorted(p.name for p in folder.iterdir()))
        for name in names:
            if upstream_text((folder/name).read_text())!=(package/name).read_text():native.fail(case='pi guard package text',file=name)
        manifest=json.loads((folder/'package.json').read_text());entries=manifest['pi']['extensions']
        assert entries and all((folder/e).is_file() and (folder/e).resolve().is_relative_to(folder.resolve()) for e in entries),manifest
        counts['pi_guard_path']=1
        # install/uninstall: with and without Pi on PATH.
        for action in ['install','uninstall']:
            for host in [True,False]:
                expected,_=reference(action,'/usr/bin/pi' if host else None)
                actual,_,home=run_native(action,host)
                if actual!=expected:native.fail(case=f'pi guard {action}',host=host,expected=expected,actual=actual)
                if not host:assert not any(home.iterdir()),(action,list(home.iterdir()))
                counts['pi_guard_delegated' if host else 'pi_guard_without_host']=counts.get('pi_guard_delegated' if host else 'pi_guard_without_host',0)+1

if __name__=='__main__':
    # Reuse only the static selector, without loading the CLI comparison module.
    import ast
    paths=[]
    for p in sorted((SOURCE/'tests').glob('test_*.py')):
        text=p.read_text()
        if not any(name in text for name in actions):continue
        tree=ast.parse(text)
        for node in tree.body:
            if isinstance(node,ast.FunctionDef) and node.name.startswith('test_') and any(name in (ast.get_source_segment(text,node) or '') for name in actions):paths.append('tests/'+p.name+'::'+node.name)
    # Parametrized over module-level uninstall callables, so the selector cannot see them (#3805).
    if (SOURCE/'tests/test_uninstall_symlink.py').exists():paths.append('tests/test_uninstall_symlink.py')
    # 0.9.84-0.9.88 installer contracts, run whole: line endings (#4360) go
    # through the native section helpers, purge safety (#4329) through the
    # native guard above; hook identity (#4359) and the OpenCode plugin refresh
    # run the reference installer flows (see interface_differences).
    paths+=[p for p in ['tests/test_install_line_endings.py','tests/test_uninstall_purge_safety.py','tests/test_install_hook_identity.py','tests/test_opencode_plugin_refresh.py'] if (SOURCE/p).exists()]
    # 0.9.89 `pi guard`: its test drives the reference dispatcher (the wheel's
    # package and pyproject package-data); pi_guard() compares the native command.
    paths+=[p for p in ['tests/test_pi_guard_install.py'] if (SOURCE/p).exists()]
    paths+=['tests/test_install_references.py::'+name for name in ('test_install_skill_references_ignores_precreated_predictable_tmp_symlink_to_file','test_install_skill_references_ignores_precreated_predictable_tmp_symlink_to_dir') if (SOURCE/'tests/test_install_references.py').exists()]
    paths+=[p for p in ['tests/test_atomic_version_stamp.py'] if (SOURCE/p).exists()]
    if '--test' in sys.argv:paths=[sys.argv[sys.argv.index('--test')+1]]
    with tempfile.TemporaryDirectory(prefix='install-oracle-home-') as tmp:
        previous=os.environ.get('HOME');os.environ['HOME']=tmp
        try:code,tests=run(paths,not_applicable_tests={k:v for k,v in STAMP_NOT_APPLICABLE.items() if any(p.split('::')[0]==k.split('::')[0] for p in paths)})
        finally:
            if previous is None:os.environ.pop('HOME',None)
            else:os.environ['HOME']=previous
    assert code==0
    roundtrips();symlinked_instructions();pi_guard();native.close()
    native.receipt('graphify/install.py',[str(p.relative_to(ROOT)) for p in sorted((ROOT/'src').glob('install*.rs'))]+[str(p.relative_to(ROOT)) for p in sorted((ROOT/'src/assets/install').rglob('*')) if p.is_file()],checked=counts,upstream_tests=tests,interface_differences=['`pi guard` (0.9.89): upstream ships the Pi package in the wheel (graphify/pi); a native build writes the same files, with the program and extension names translated, to ~/.graphicle-graph/pi and hands that folder to Pi. tests/test_pi_guard_install.py drives the reference dispatcher; pi_guard() compares the native command with it (package files, delegation to `pi install|remove` with Pi\'s exit status, exit 1 with nothing written when Pi is not on PATH).','Embedded native skill and workflow reference replace Python command recipes; platform paths, scopes, configuration preservation, version stamps, and backups retain their contracts.','Skill writers take a skill-directory lock under the engine\'s own ~/.cache/graphicle/skill-locks. The installed skill is skills/graphicle with the stamp .graphicle_version holding the engine package version; the 0.9.72 CLI refresh of stale skills (#1805) keeps upstream\'s limits for those engine copies, and its executable tests are cli/tests/skill_refresh.rs.','Uninstall edits a symlinked instructions file through the link and keeps the link (#3805); symlinked_instructions() compares that file-system contract with the reference uninstall functions. Upstream\'s test_install_claude_md_names_the_symlink_target_it_wrote and test_uninstall_claude_md_names_the_symlink_target_it_edited assert per-file console lines (`_shown_path`); the native installer reports one summary line per platform and names no instruction file, so those lines have no native counterpart.','tests/test_install_hook_identity.py and tests/test_opencode_plugin_refresh.py drive the reference installer flows (settings hook entries, the OpenCode plugin file and the CLI refresh); only the substituted helpers are native there. The native hook-entry identity is the engine\'s own word match on hook-guard/hook-check under any program name (the reference also matches its legacy inline guards), and the native flows for both contracts are tests/compat_0_9_88_outputs.rs (hook identity, OpenCode plugins) and cli/tests/skill_refresh.rs.'])
