#!/usr/bin/env python3
"""Fetch missing notices at Cargo's recorded VCS revision; never infer a license."""
import concurrent.futures
import hashlib
import json
from pathlib import Path
import urllib.error
import urllib.request

ROOT=Path(__file__).resolve().parents[1]


def main():
    graph=json.loads((ROOT/"DEPENDENCIES.json").read_text())
    packages={(p['name'],p['version']):p for p in graph['packages']}
    def resolve(missing):
        package=packages[missing['name'],missing['version']]
        directory=Path(missing['manifest']);vcs=directory/'.cargo_vcs_info.json'
        if not vcs.exists():return {'package':missing['name'],'missing':'No VCS receipt; locate the matching upstream release'}
        receipt=json.loads(vcs.read_text());revision=receipt['git']['sha1'];repository=package['repository'].removesuffix('.git').removeprefix('https://github.com/')
        dest=ROOT/'licenses/upstream-crates'/f"{missing['name']}-{missing['version']}";dest.mkdir(parents=True,exist_ok=True)
        found=[]
        for filename in ['LICENSE','LICENSE-MIT','LICENSE-APACHE','LICENSE.md','LICENSE.txt','COPYING','COPYRIGHT']:
            url=f'https://raw.githubusercontent.com/{repository}/{revision}/{filename}'
            try:
                with urllib.request.urlopen(url,timeout=30) as response: data=response.read(1024*1024)
            except urllib.error.HTTPError as error:
                if error.code==404:continue
                raise
            (dest/filename).write_bytes(data);found.append({'url':url,'sha256':hashlib.sha256(data).hexdigest(),'file':str((dest/filename).relative_to(ROOT))})
        result={'package':missing['name'],'version':missing['version'],'revision':revision,'licenses':found}
        if not found:result['missing']='No root license at the recorded VCS revision'
        return result
    results=[]
    with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
        for r in pool.map(resolve,graph['missing_package_license_texts']):results.append(r)
    (ROOT/'licenses/CRATE_SOURCE_NOTICES.json').write_text(json.dumps(results,indent=2)+'\n')
    print(json.dumps([r for r in results if r.get('missing')],indent=2))


if __name__=='__main__':main()
