"""Installer helper parity and isolated native installation round trips."""
import functools, inspect, json, os, platform, sys, tempfile, tomllib
from pathlib import Path
from parity import Native, ROOT, SOURCE
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
    if '--test' in sys.argv:paths=[sys.argv[sys.argv.index('--test')+1]]
    with tempfile.TemporaryDirectory(prefix='install-oracle-home-') as tmp:
        previous=os.environ.get('HOME');os.environ['HOME']=tmp
        try:code,tests=run(paths)
        finally:
            if previous is None:os.environ.pop('HOME',None)
            else:os.environ['HOME']=previous
    assert code==0
    roundtrips();symlinked_instructions();native.close()
    native.receipt('graphify/install.py',[str(p.relative_to(ROOT)) for p in sorted((ROOT/'src').glob('install*.rs'))]+[str(p.relative_to(ROOT)) for p in sorted((ROOT/'src/assets/install').glob('*'))],checked=counts,upstream_tests=tests,interface_differences=['Embedded native skill and workflow reference replace Python command recipes; platform paths, scopes, configuration preservation, version stamps, and backups retain their contracts.','Skill writers take a skill-directory lock under the engine\'s own ~/.cache/graphicle/skill-locks. The installed skill is skills/graphicle with the stamp .graphicle_version holding the engine package version; the 0.9.72 CLI refresh of stale skills (#1805) keeps upstream\'s limits for those engine copies, and its executable tests are cli/tests/skill_refresh.rs.','Uninstall edits a symlinked instructions file through the link and keeps the link (#3805); symlinked_instructions() compares that file-system contract with the reference uninstall functions. Upstream\'s test_install_claude_md_names_the_symlink_target_it_wrote and test_uninstall_claude_md_names_the_symlink_target_it_edited assert per-file console lines (`_shown_path`); the native installer reports one summary line per platform and names no instruction file, so those lines have no native counterpart.','tests/test_install_hook_identity.py and tests/test_opencode_plugin_refresh.py drive the reference installer flows (settings hook entries, the OpenCode plugin file and the CLI refresh); only the substituted helpers are native there. The native hook-entry identity is the engine\'s own word match on hook-guard/hook-check under any program name (the reference also matches its legacy inline guards), and the native flows for both contracts are tests/compat_0_9_88_outputs.rs (hook identity, OpenCode plugins) and cli/tests/skill_refresh.rs.'])
