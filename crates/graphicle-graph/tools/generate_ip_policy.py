#!/usr/bin/env python3
"""Pin the reference Python IP classification as native interval data."""
import hashlib
import ipaddress
import json
import platform
from pathlib import Path
from parity import ROOT, SOURCE
from graphify.security import _ip_is_blocked

def intervals(version):
    cls = ipaddress.IPv4Address if version == 4 else ipaddress.IPv6Address
    network_cls = ipaddress.IPv4Network if version == 4 else ipaddress.IPv6Network
    bits = 32 if version == 4 else 128
    boundaries = {0, 1 << bits}
    def add(value):
        if isinstance(value, network_cls):
            boundaries.update([int(value.network_address), int(value.broadcast_address) + 1])
        elif isinstance(value, (list, tuple)):
            for item in value: add(item)
    constants = cls(0)._constants
    for name in dir(constants): add(getattr(constants,name))
    if version == 4: add(ipaddress.ip_network("100.64.0.0/10"))
    points = sorted(boundaries)
    result = []
    for start, stop in zip(points,points[1:]):
        if _ip_is_blocked(cls(start)):
            if result and result[-1][1] + 1 == start: result[-1][1] = stop - 1
            else: result.append([start,stop-1])
    return result

text = "// Generated reference-policy data. Python " + platform.python_version() + "; not a runtime Python dependency.\n"
for version in [4,6]:
    values = intervals(version)
    text += f"pub const BLOCKED_V{version}: &[(u128,u128)] = &[\n"
    text += "".join(f"    (0x{lo:x},0x{hi:x}),\n" for lo,hi in values) + "];\n"
(ROOT / "src/ip_policy_data.rs").write_text(text)
(ROOT / "migration/IP_POLICY.json").write_text(json.dumps({"reference_python":platform.python_version(),
    "ipaddress_source_sha256":hashlib.sha256(Path(ipaddress.__file__).read_bytes()).hexdigest(),
    "graphify_security_sha256":hashlib.sha256((SOURCE / "graphify/security.py").read_bytes()).hexdigest(),
    "nat64":"64:ff9b::/96 is unwrapped to its IPv4 payload before interval lookup.",
    "interval_counts":{str(v):len(intervals(v)) for v in [4,6]}},indent=2) + "\n")
