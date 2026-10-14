"""Run unchanged Git hook filesystem contracts through the native installer."""
import functools, inspect, json, os, re, subprocess, sys, tempfile, time
from pathlib import Path
from parity import Native, ROOT, SOURCE, BINARY
from isolated_upstream import run
import naming
import graphify.hooks as ref
native=Native('hooks');counts={}

for name,action in [('install','install'),('uninstall','uninstall'),('status','status'),('_hooks_dir','hooks_dir'),('_load_graphifyrc','load_rc')]:
    original=getattr(ref,name);sig=inspect.signature(original)
    @functools.wraps(original)
    def wrapper(*args,_name=name,_action=action,_sig=sig,**kwargs):
        a=_sig.bind(*args,**kwargs);a.apply_defaults();path=next(iter(a.arguments.values()))
        native.use_working_directory(Path.cwd());result=native.call('hooks',action=_action,path=str(path));counts[_name]=counts.get(_name,0)+1
        if not result['ok']:raise (ValueError if _name=='_load_graphifyrc' else RuntimeError)(result['error'])
        return Path(result['result']) if _name=='_hooks_dir' else result['result']
    setattr(ref,name,wrapper)

# 0.9.85 (#4278): names git C-quotes in --name-only output. The native hook
# lists changed files with -z (verbatim names); the decoder serves the
# GRAPHICLE_CHANGED fallback.
def _unquote_git_path(name):
    native.use_working_directory(Path.cwd());result=native.call('hooks',action='unquote_git_path',name=name)
    counts['_unquote_git_path']=counts.get('_unquote_git_path',0)+1
    if not result['ok']:raise ValueError(result['error'])
    return result['result']
ref._unquote_git_path=_unquote_git_path

NAMES='''test_install_creates_hook test_install_is_executable test_install_idempotent test_install_appends_to_existing_hook test_uninstall_removes_hook test_uninstall_no_hook test_status_installed test_status_not_installed test_no_git_repo_raises test_install_creates_post_checkout_hook test_install_post_checkout_is_executable test_uninstall_removes_post_checkout_hook test_status_shows_both_hooks test_installed_hooks_contain_no_nohup test_posix_custom_hookspath_still_works test_default_hooks_dir_unaffected test_hooks_dir_no_warning_on_duplicate_config_keys test_hooks_dir_duplicate_config_keys_honor_custom_hookspath test_install_merge_driver_idempotent test_install_preserves_existing_gitattributes test_uninstall_removes_merge_driver_keeps_other_attrs test_graphifyrc_parsing test_no_config_preserves_existing_hook test_config_baked_into_generated_hook test_baked_viz_limit_yields_to_an_explicit_per_run_override test_status_survives_a_malformed_graphifyrc test_changing_config_updates_existing_hook test_user_hook_content_survives_update test_status_reports_configuration test_both_hooks_configured test_status_compares_only_the_graphify_block test_unquote_git_path'''.split()

def workflows():
    # Native-only workflow at the engine's true defaults: the CLI, the hook
    # scripts and this JSON Lines process all use the default output folder.
    native=Native('hooks-workflows',layout='native')
    try:_workflows(native)
    finally:native.close()
    return native.calls

def _workflows(native):
    with tempfile.TemporaryDirectory(prefix='native-hooks-') as tmp:
        root=Path(tmp);repo=root/'repository with spaces';repo.mkdir();env={**naming.scrubbed(os.environ),'HOME':str(root/'home'),'GRAPHICLE_REBUILD_LOG':str(root/'rebuild.log'),'GRAPHICLE_REBUILD_TIMEOUT':'30','GRAPHICLE_MAX_WORKERS':'1'}
        def git(*args):return subprocess.run(['git','-C',str(repo),*args],env=env,text=True,capture_output=True,check=True)
        git('init');git('config','user.name','Native test');git('config','user.email','native-test@example.invalid')
        f=repo/'source.py';f.write_text('def first():\n    return 1\n');git('add','source.py');git('commit','-m','initial fixture')
        p=subprocess.run([str(BINARY),'extract','.', '--code-only','--no-cluster'],cwd=repo,env=env,text=True,capture_output=True);assert p.returncode==0,p.stderr
        native.result('hooks',action='install',path=str(repo))
        for event in ['post-commit','post-checkout']:
            script=repo/'.git/hooks'/event
            assert str(BINARY) in script.read_text()
            assert 'python' not in script.read_text().lower()
            subprocess.run(['/bin/sh','-n',str(script)],check=True)
        before=(repo/naming.NATIVE_OUT/'graph.json').read_bytes()
        f.write_text('def first():\n    return second()\ndef second():\n    return 2\n');git('add','source.py');git('commit','-m','add function')
        graph=repo/naming.NATIVE_OUT/'graph.json';deadline=time.monotonic()+30
        while time.monotonic()<deadline:
            if any(n.get('label')=='second()' for n in json.loads(graph.read_text())['nodes']):break
            time.sleep(.15)
        else:raise AssertionError('Native detached hook did not rebuild: '+(root/'rebuild.log').read_text())
        assert before!=graph.read_bytes();counts['detached_native_rebuild']=1
        # Skip guard must work even when the native executable cannot be located.
        for event in ['post-commit','post-checkout']:
            p=subprocess.run(['/bin/sh',str(repo/'.git/hooks'/event),'old','new','1'],cwd=repo,env={**env,'GRAPHICLE_SKIP_HOOK':'1','PATH':''},capture_output=True,text=True)
            assert p.returncode==0,p.stderr
        # The scope marker is bounded to the repository and tolerates a BOM.
        (repo/'sub').mkdir();out=repo/naming.NATIVE_OUT
        for value,allowed in [('\ufeffsub\n',True),('../outside',False),('',True)]:
            (out/'.graphicle_root').write_text(value)
            selected,warning=native.result('hooks',action='saved_root',path=str(repo),out=str(out))
            assert (warning is None)==allowed,(value,selected,warning)
        counts['native_scope_and_skip_guards']=5
        # Status flags a block written by an older release, naming the refresh
        # command, but ignores the pinned executable (#3771). Upstream's tests edit
        # Python launcher text; the native block drops a guard line and re-pins
        # `_GCL_BIN=` instead.
        hook=repo/'.git/hooks/post-commit';text=hook.read_text();older=text.replace('[ -f "$_GCL_GITDIR/CHERRY_PICK_HEAD" ] && exit 0\n','')
        assert older!=text;hook.write_text(older)
        res=native.result('hooks',action='status',path=str(repo));line=lambda name:next(l for l in res.splitlines() if l.startswith(name+':'))
        assert 'out of date' in line('post-commit') and f'{naming.NATIVE_BIN} hook install' in line('post-commit'),res
        assert 'out of date' not in line('post-checkout'),res
        assert 'updated existing post-commit hook' in native.result('hooks',action='install',path=str(repo))
        assert 'out of date' not in native.result('hooks',action='status',path=str(repo))
        text=hook.read_text();repinned=re.sub(r'(?m)^_GCL_BIN=.*$',"_GCL_BIN='/opt/other/bin/graphicle-graph'",text)
        assert repinned!=text;hook.write_text(repinned)
        assert 'out of date' not in native.result('hooks',action='status',path=str(repo))
        native.result('hooks',action='install',path=str(repo));assert str(BINARY) in hook.read_text()
        counts['native_status_out_of_date']=2
        # Hooks configured through Husky must live beside its generated shims.
        git('config','core.hooksPath','.husky/_');(repo/'.husky/_').mkdir(parents=True)
        native.result('hooks',action='install',path=str(repo));assert (repo/'.husky/post-commit').exists();assert not (repo/'.husky/_/post-commit').exists()
        native.result('hooks',action='uninstall',path=str(repo));assert not (repo/'.husky/post-commit').exists();counts['husky_roundtrip']=1
        # A timed-out worker must terminate only its own child tree, including
        # external tools that opened a separate process group.
        if os.name=='posix':
            f.write_text(f.read_text()+'\ndef timeout_sentinel():\n    return True\n')
            fake=root/'bin';fake.mkdir();child_pid=root/'child.pid';tool=fake/'git'
            tool.write_text('#!/bin/sh\nsleep 30 &\nprintf "%s\\n" "$!" > "$GRAPHIFY_TEST_CHILD_PID"\nwait\n');tool.chmod(0o755)
            unrelated=subprocess.Popen(['/bin/sleep','30'])
            try:
                started=time.monotonic()
                r=subprocess.run([str(BINARY),'hook','run','post-checkout'],cwd=repo,env={**env,'PATH':str(fake)+':/usr/bin:/bin','GRAPHIFY_TEST_CHILD_PID':str(child_pid),'GRAPHICLE_REBUILD_TIMEOUT':'1'},text=True,capture_output=True,timeout=8)
                assert r.returncode!=0 and 'exceeded' in r.stderr,(r.returncode,r.stdout,r.stderr)
                assert time.monotonic()-started<6 and unrelated.poll() is None
                assert child_pid.exists()
                pid=int(child_pid.read_text())
                for _ in range(50):
                    check=subprocess.run(['/bin/ps','-p',str(pid),'-o','stat='],text=True,capture_output=True)
                    if not check.stdout.strip() or check.stdout.strip().startswith('Z'):break
                    time.sleep(.05)
                else:raise AssertionError('Hook timeout left its child alive')
                counts['native_worker_timeout_tree_cleanup']=1
            finally:unrelated.terminate();unrelated.wait(timeout=3)

if __name__=='__main__':
    paths=['tests/test_hooks.py::'+name for name in NAMES]
    if '--test' in sys.argv:paths=[sys.argv[sys.argv.index('--test')+1]]
    code,tests=run(paths,ignore=['test_installed_hooks_contain_no_nohup'],check='check_hooks');assert code==0
    native.calls+=workflows();native.close()
    native.receipt('graphify/hooks.py',['src/hooks.rs','src/hooks_runtime.rs'],checked=counts,upstream_tests=tests,interface_differences=['Hooks pin the native executable and launch native workers; interpreter discovery and embedded Python snippets are intentionally replaced.','test_status_flags_a_hook_block_from_an_older_graphify and test_status_ignores_the_pinned_interpreter edit Python launcher text (the creation flags, `_PINNED=`) that native hooks do not contain; the native workflow checks the same status contracts by dropping a guard line from the block and re-pinning `_GCL_BIN=`.','The native workflow (detached rebuild, skip guard, scope marker, status, Husky, timeout) runs at the engine defaults: output folder .graphicle, marker .graphicle_root, GRAPHICLE_* settings.',
    'test_post_commit_changed_list_matches_real_non_ascii_files and test_rebuild_bodies_treat_blank_graphify_out_as_default execute the Python hook script and rebuild bodies (0.9.85/0.9.86); the native hook lists changed files with `git diff --name-only -z` (names verbatim, never quoted) and the worker reads the output folder from the engine\'s paths module. tests/compat_0_9_88_outputs.rs checks the changed-file list against a real commit of non-ASCII and quote-bearing names.'])
