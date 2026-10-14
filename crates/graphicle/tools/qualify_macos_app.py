#!/usr/bin/env python3
"""Check the built Mac app on disposable data without opening browser tabs."""
from pathlib import Path
import json,subprocess,time,os,signal,tempfile
from urllib.request import urlopen
root=Path(__file__).resolve().parents[1]
(root/'test-artifacts').mkdir(exist_ok=True)
binary=root/'dist/Graphicle.app/Contents/MacOS/Graphicle'
with tempfile.TemporaryDirectory(prefix='graphicle-app-lifecycle-') as temporary:
    p=subprocess.Popen([str(binary)],env=dict(os.environ,GRAPHICLE_DATA_DIR=temporary+'/store',GRAPHICLE_DIAGNOSTICS='1',GRAPHICLE_NO_BROWSER='1'),stdout=subprocess.DEVNULL,stderr=subprocess.PIPE,text=True)
    child=None;url=None;started=time.monotonic()
    try:
        for _ in range(100):
            if p.poll() is not None: raise RuntimeError(p.stderr.read())
            found=subprocess.run(['pgrep','-P',str(p.pid),'-x','graphicle'],capture_output=True,text=True).stdout.split()
            if found:
                child=int(found[0]);lines=subprocess.run(['lsof','-nP','-a','-p',str(child),'-iTCP','-sTCP:LISTEN','-Fn'],capture_output=True,text=True).stdout.splitlines()
                host=next((line[1:] for line in lines if line.startswith('n127.0.0.1:')),None)
                if host:
                    try:
                        url='http://'+host
                        with urlopen(url+'/health',timeout=1) as response: health=json.load(response)
                        if health['status']=='ok': break
                    except OSError: pass
            time.sleep(.1)
        if not url: raise RuntimeError('App did not start a healthy loopback service')
        print(json.dumps({'app_pid':p.pid,'worker_pid':child,'ready_after_seconds':round(time.monotonic()-started,3)}),flush=True)
        stop=time.monotonic();p.send_signal(signal.SIGTERM)
        try: p.wait(timeout=12)
        except subprocess.TimeoutExpired:
            p.kill();p.wait(timeout=5);print(p.stderr.read(),flush=True);raise
        alive=subprocess.run(['ps','-p',str(child),'-o','stat='],capture_output=True,text=True).stdout.strip()
        print(p.stderr.read(),flush=True)
        if p.returncode!=0 or alive: raise RuntimeError(f'App exit {p.returncode}; child state {alive!r}')
        receipt={'mac_app_started_bundled_rust_process':True,'health_ready':True,'normal_process_termination_calls_application_quit':True,'app_exit_code':p.returncode,'worker_exited':True,'shutdown_seconds':round(time.monotonic()-stop,3),'native_menu_automation':'not exercised; process lifecycle checked directly','data':'fresh disposable directory'}
        (root/'test-artifacts/MAC_APP_LIFECYCLE.json').write_text(json.dumps(receipt,indent=2)+'\n');print(json.dumps(receipt),flush=True)
    finally:
        if p.poll() is None: p.kill();p.wait(timeout=5)
        if child:
            try: os.kill(child,signal.SIGINT)
            except ProcessLookupError: pass
