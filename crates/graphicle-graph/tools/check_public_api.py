"""Exercise all package exports through the public native integration test."""
import subprocess
from parity import Native, ROOT

native = Native('public-api')
assert native.result('info')['implementation'] == 'rust'
command = ['cargo', 'test', '--offline', '--locked', '--features', 'postgres', '--test', 'public_api']
subprocess.run(command, cwd=ROOT, check=True)
native.close()
native.receipt('graphify/__init__.py', ['src/lib.rs','src/model.rs','src/error.rs','tests/public_api.rs'],
               public_exports=17, native_integration_tests=1, command=command,
               adaptations=['Public Rust functions use typed results, borrowed graph references, and named options. The native GraphDocument model validates identifiers, spans, metadata and endpoints; the legacy NetworkX adapter retains flexible identifier and metadata compatibility.'])
