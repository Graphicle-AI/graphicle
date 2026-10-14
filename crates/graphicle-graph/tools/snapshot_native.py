"""Pin a built executable while other tasks build in the shared Cargo target."""
import hashlib
import os
import shutil
import tempfile
from pathlib import Path
root=Path(__file__).resolve().parents[1]
source=Path(os.environ.get('GRAPHICLE_GRAPH_BUILD',root/'target/debug/graphicle-graph'))
with source.open('rb') as f: content=f.read()
digest=hashlib.sha256(content).hexdigest()
destination=Path(tempfile.gettempdir())/'graphicle-graph-builds'/digest/'graphicle-graph'
if not destination.exists():
    destination.parent.mkdir(parents=True,exist_ok=True)
    destination.write_bytes(content)
    destination.chmod(0o755)
print(destination)
