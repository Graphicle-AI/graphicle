#!/usr/bin/env python3
"""Retain CPython's HTML5 character-reference table and license for native use."""
import html.entities
import json
import shutil
import sysconfig
from pathlib import Path
root=Path(__file__).resolve().parents[1]
(root/"src/html_entities.json").write_text(json.dumps(html.entities.html5,sort_keys=True,ensure_ascii=True,separators=(",",":"))+"\n")
shutil.copyfile(Path(sysconfig.get_path("stdlib"))/"LICENSE.txt",root/"licenses/PYTHON-LICENSE.txt")
print(json.dumps({"html5_entities":len(html.entities.html5)}))
