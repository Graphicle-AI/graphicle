#!/usr/bin/env python3
"""Report release prerequisites without reading credential values or publishing."""
import argparse, json, os, platform, shutil, subprocess, time
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--output',type=Path,required=True);a=p.parse_args()
def capture(*cmd):
    r=subprocess.run(cmd,capture_output=True,text=True,timeout=30)
    return {'exit_code':r.returncode,'output':r.stdout.strip(),'error':r.stderr.strip()[:1000]}
r={'checked_unix':time.time(),'platform':platform.platform(),'published':False,'checks':{}}
for name in ('cargo','docker','codesign','xcrun','openssl'):
    r['checks'][name]={'installed':bool(shutil.which(name))}
if platform.system()=='Darwin':
    identities=capture('security','find-identity','-v','-p','codesigning')
    r['checks']['apple_developer_id']={'available':'Developer ID Application:' in identities['output'],'inspection':identities}
r['checks']['release_signing_key']={'configured':bool(os.environ.get('GRAPHICLE_RELEASE_PRIVATE_KEY'))}
r['checks']['notary_credentials']={'configured':all(bool(os.environ.get(k)) for k in ('APPLE_ID','APPLE_TEAM_ID','APPLE_APP_PASSWORD'))}
r['checks']['public_host']={'configured':bool(os.environ.get('GRAPHICLE_PUBLIC_ORIGIN'))}
r['checks']['provider_accounts']={'requires_authorized_real_account_checks':True,'fixtures_do_not_qualify_live_access':True}
a.output.parent.mkdir(parents=True,exist_ok=True);a.output.write_text(json.dumps(r,indent=2)+'\n');print(json.dumps(r,indent=2))
