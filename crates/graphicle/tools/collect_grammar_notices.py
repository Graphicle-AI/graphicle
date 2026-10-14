#!/usr/bin/env python3
"""Retain license text at each statically linked grammar's exact pinned revision."""
import concurrent.futures
import hashlib
import json
from pathlib import Path
import tomllib
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def main():
    paths = list((Path.home()/".cargo/registry/src").glob("*/tree-sitter-language-pack-1.20.0/language_definitions.json"))
    definitions = json.loads(paths[0].read_text())
    languages = tomllib.loads((ROOT/".cargo/config.toml").read_text())["env"]["TSLP_LANGUAGES"].split(",")
    projects = {}
    for language in languages:
        definition = definitions[language]
        key = (definition["repo"], definition["rev"])
        projects.setdefault(key, []).append(language)
    destination = ROOT/"licenses/grammars"; destination.mkdir(parents=True, exist_ok=True)
    def download(item):
        (repository, revision), names = item
        project = repository.removeprefix("https://github.com/")
        saved = destination/(project.replace("/", "__")+"__"+revision+".txt")
        if not saved.exists():
            for name in ["LICENSE", "LICENSE.md", "LICENSE.txt", "LICENSE-MIT", "COPYING", "license", "LICENSE-MIT.md", "LICENSE-MIT.txt", "LICENSE.rst", "COPYRIGHT", "License", "LICENSE-APACHE", "LICENSE-APACHE-2.0"]:
                url = f"https://raw.githubusercontent.com/{project}/{revision}/{name}"
                try:
                    with urllib.request.urlopen(url, timeout=40) as response: text = response.read(1024*1024+1)
                    if len(text)>1024*1024: raise RuntimeError("License text exceeds limit")
                    saved.write_bytes(text); break
                except urllib.error.HTTPError as error:
                    if error.code!=404: raise
            else: raise RuntimeError(f"Locate actual license for {project} at {revision}")
        return {"repository":repository,"revision":revision,"languages":names,"license_file":str(saved.relative_to(ROOT)),"license_sha256":hashlib.sha256(saved.read_bytes()).hexdigest()}
    results=[]; missing=[]
    with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
        pending={pool.submit(download,item):item for item in sorted(projects.items())}
        for future in concurrent.futures.as_completed(pending):
            try:results.append(future.result())
            except Exception as error:missing.append({"project":pending[future],"error":str(error)})
    results.sort(key=lambda p:p["repository"])
    receipt = {"grammar_pack":"tree-sitter-language-pack==1.20.0","language_definitions_sha256":hashlib.sha256(paths[0].read_bytes()).hexdigest(),"statically_linked_languages":languages,"projects":results}
    (ROOT/"GRAMMAR_NOTICES.json").write_text(json.dumps(receipt,indent=2)+"\n")
    print(f"Retained {len(results)} pinned license texts for {len(languages)} linked grammars.")
    if missing:
        print(json.dumps(missing,indent=2));raise SystemExit("Resolve missing grammar licenses before distribution.")


if __name__=="__main__": main()
