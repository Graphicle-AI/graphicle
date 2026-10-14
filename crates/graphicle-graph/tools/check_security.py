#!/usr/bin/env python3
"""Check native security decisions; network transport has separate Rust tests."""
import ast
import ipaddress
import json
import os
import random
from pathlib import Path
from parity import Native, ROOT, SOURCE, upstream_tests
import graphify.security as reference

native = Native("security")
rng = random.Random(20260923)


def result(operation,**fields):
    response=native.call(operation,**fields)
    if response.get("ok"): return response["result"]
    message=response["error"]
    if message.startswith("FileNotFoundError: "): raise FileNotFoundError(message.split(": ",1)[1])
    if message.startswith("OSError: "): raise OSError(message.split(": ",1)[1])
    raise ValueError(message.removeprefix("ValueError: "))


try:
    output_names=["graphify-out","a/../out/","a/b/..",".","..","/","//","///","/a/../b","/a/../../b",""]
    for output in output_names:
        expected=os.path.basename(os.path.normpath(output))
        actual=result("security.out_name",value=output)
        if actual!=expected:native.fail(output_path=output,expected=expected,actual=actual)
    addresses=[]
    for cls in [ipaddress.IPv4Address,ipaddress.IPv6Address]:
        bits=32 if cls is ipaddress.IPv4Address else 128
        addresses += [cls(rng.getrandbits(bits)) for _ in range(10000)]
        addresses += [cls((rng.getrandbits(28)<<100) | (0x2<<124)) for _ in range(1000)] if bits==128 else []
        def append(value):
            if isinstance(value,(ipaddress.IPv4Network,ipaddress.IPv6Network)) and value.version==(4 if bits==32 else 6):
                for point in [int(value.network_address)-1,int(value.network_address),int(value.broadcast_address),int(value.broadcast_address)+1]:
                    if 0<=point<1<<bits: addresses.append(cls(point))
            elif isinstance(value,(tuple,list)):
                for item in value: append(item)
        for name in dir(cls(0)._constants): append(getattr(cls(0)._constants,name))
    addresses += [ipaddress.IPv6Address(int(ipaddress.IPv6Address("64:ff9b::")) | rng.getrandbits(32)) for _ in range(2000)]
    addresses += [ipaddress.ip_address(ip) for ip in ["127.0.0.1","8.8.8.8","100.64.0.0","100.127.255.255","100.128.0.0","::1","64:ff9b::808:808","64:ff9b::7f00:1"]]
    for offset in range(0,len(addresses),1000):
        batch=addresses[offset:offset+1000]
        expected=[reference._ip_is_blocked(ip) for ip in batch]
        actual=result("security.ip",ips=[str(ip) for ip in batch])
        if expected!=actual:
            native.fail(addresses=[str(ip) for ip in batch],expected=expected,actual=actual)
    samples=[None,True,False,42,3.14,"<&'\"日本語\x00\x1f",[1,"<&",None],{"\x00":"drop","日本語":"é<&'\""}]
    alphabet=list("abc<&>'\"日本語é\x00\x1f\x7f\n\t")
    samples += ["".join(rng.choices(alphabet,k=rng.randrange(1000))) for _ in range(500)]
    for value in samples:
        for suffix,oracle in [("label",reference.sanitize_label),("metadata_string",reference._sanitize_metadata_string),("metadata_value",reference._sanitize_metadata_value)]:
            actual=result("security."+suffix,value=value)
            expected=oracle(value)
            if expected!=actual: native.fail(operation=suffix,value=value,expected=expected,actual=actual)
    for i in range(100):
        value={"nest":{"list":rng.choices(samples,k=70)},"<key>":rng.choice(samples)}
        actual=result("security.metadata",value=value)
        expected=reference.sanitize_metadata(value)
        if expected!=actual: native.fail(operation="metadata",value=value,expected=expected,actual=actual)

    saved_raw=os.environ.get("GRAPHIFY_MAX_GRAPH_BYTES")
    cap_inputs=["", "  ", "_1", "+1", "1__0", "1_000", "0", "-1", "\x1c+٢_０ MB\x1f", "１２MB", "𝟚GB", "1.5GB", "640MB", "16_000"]
    for raw in cap_inputs:
        os.environ["GRAPHIFY_MAX_GRAPH_BYTES"]=raw
        expected=reference._max_graph_file_bytes()
        actual=result("security.graph_cap",raw=raw)
        if expected!=actual: native.fail(raw=raw,expected=expected,actual=actual)
    if saved_raw is None: os.environ.pop("GRAPHIFY_MAX_GRAPH_BYTES",None)
    else: os.environ["GRAPHIFY_MAX_GRAPH_BYTES"]=saved_raw
    reference.sanitize_label=lambda value:result("security.label",value=value)
    reference._sanitize_metadata_string=lambda value:result("security.metadata_string",value=value if isinstance(value,(str,int,float,bool,list,dict,tuple,type(None))) else str(value))
    reference._sanitize_metadata_value=lambda value:result("security.metadata_value",value=value)
    reference.sanitize_metadata=lambda value:result("security.metadata",value=value)
    reference._max_graph_file_bytes=lambda:result("security.graph_cap",raw=os.environ.get("GRAPHIFY_MAX_GRAPH_BYTES",""),default=reference._MAX_GRAPH_FILE_BYTES)
    reference.check_graph_file_size_cap=lambda path:result("security.check_graph_size",path=str(path),cap=reference._max_graph_file_bytes())
    reference.validate_graph_path=lambda path,base=None:Path(result("security.graph_path",path=str(path),base=str(base) if base is not None else None,output=reference.GRAPHIFY_OUT,output_name=reference.GRAPHIFY_OUT_NAME))
    reference.validate_url=lambda url:result("security.validate_url",url=url)
    tree=ast.parse((SOURCE / "tests/test_security.py").read_text())
    names=[node.name for node in tree.body if isinstance(node,ast.FunctionDef) and node.name.startswith("test_") and not node.name.startswith("test_safe_fetch") and node.name!="test_graph_size_cap_unreadable_directory_silently_returns"]
    tests=upstream_tests(*["tests/test_security.py::"+name for name in names])
finally:
    native.close()

native.receipt("graphify/security.py",["src/security.rs","src/ip_policy_data.rs","src/decimal_data.rs"],
               ip_classifications=len(addresses),sanitization_cases=len(samples)*3+100,upstream_tests=tests,
               output_directory_name_cases=len(output_names),
               transport_tests="Native Rust tests cover pinned address use, mixed public/private DNS, redirect scheme rejection, bounded read overflow and non-2xx status.",
               adaptations=["Rust HTTP/TLS replaces urllib handler classes. Environment HTTP proxy forwarding is disabled to keep the connection pinned.","Rust URL parsing rejects malformed hostless HTTP URLs earlier.","Rust graph byte limits use a 64-bit positive integer; JSON types replace Python-specific objects.","Python mocked-opener and mocked-Path.stat tests are not counted as native transport/filesystem tests."])
