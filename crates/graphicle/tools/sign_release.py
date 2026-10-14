#!/usr/bin/env python3
"""Sign an exact release payload with a publisher-owned Ed25519 key; never publish.

Artifact list JSON: [{platform,edition,name,url,local_path}]; edition is core or pro,
and each platform has at most one installer per edition (`graphicle update` takes
only its own edition's). Installs made before the editions were split expect
exactly one installer per platform and read an installer without an edition as
the commercial edition's, so sign one feed per edition while such installs exist. The private key
must be PEM.
Obtain the corresponding base64 public key separately; never trust one in a feed.
"""
import argparse,base64,hashlib,json,subprocess,tempfile,time
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--artifacts',type=Path,required=True);p.add_argument('--version',required=True);p.add_argument('--sequence',type=int,required=True);p.add_argument('--key',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--openssl',default='openssl');p.add_argument('--valid-days',type=int,default=30);a=p.parse_args()
if not 1<=a.valid_days<=90 or a.sequence<1: p.error('Use 1–90 valid days and a positive monotonic sequence')
artifacts=[]
for x in json.loads(a.artifacts.read_text()):
    if x.get('edition') not in ('core','pro'): p.error(f"{x.get('name')}: edition must be core or pro")
    if any((y['platform'],y['edition'])==(x['platform'],x['edition']) for y in artifacts): p.error(f"Two {x['edition']} installers for {x['platform']}")
    raw=Path(x['local_path']).read_bytes();artifacts.append({k:x[k] for k in ('platform','edition','name','url')}|{'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest()})
now=int(time.time());payload=json.dumps({'schema':1,'channel':'stable','version':a.version,'sequence':a.sequence,'published_at':now,'expires_at':now+a.valid_days*86400,'artifacts':artifacts},separators=(',',':')).encode()
with tempfile.TemporaryDirectory(prefix='graphicle-release-') as temp:
    root=Path(temp);(root/'payload').write_bytes(payload)
    subprocess.run([a.openssl,'pkeyutl','-sign','-rawin','-inkey',str(a.key.resolve()),'-in',str(root/'payload'),'-out',str(root/'signature')],check=True)
    sig=(root/'signature').read_bytes()
    if len(sig)!=64:raise SystemExit('Expected an Ed25519 signature, not another key type')
    public=subprocess.check_output([a.openssl,'pkey','-in',str(a.key.resolve()),'-pubout','-outform','DER'])
    if len(public)!=44 or public[:12]!=bytes.fromhex('302a300506032b6570032100'):raise SystemExit('Signing key is not Ed25519')
a.output.parent.mkdir(parents=True,exist_ok=True);a.output.write_text(json.dumps({'payload':base64.b64encode(payload).decode(),'signature':base64.b64encode(sig).decode()},indent=2)+'\n')
print(json.dumps({'manifest':str(a.output),'published':False,'public_key_base64':base64.b64encode(public[-32:]).decode(),'public_key_sha256':hashlib.sha256(public[-32:]).hexdigest()}))
