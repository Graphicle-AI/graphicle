#!/usr/bin/env python3
"""Original query-log tests against native filesystem writes and settings."""
import datetime
import json
import os
import random
import tempfile
from pathlib import Path
from unittest.mock import patch
from parity import Native, upstream_tests
from naming import to_upstream
import graphify.querylog as reference

native=Native("querylog")
old_nodes=reference.nodes_from_result
old_path=reference._log_path
old_responses=reference._log_responses
old_log=reference.log_query
keys=["GRAPHIFY_QUERY_LOG","GRAPHIFY_QUERY_LOG_ENABLE","GRAPHIFY_QUERY_LOG_DISABLE","GRAPHIFY_QUERY_LOG_RESPONSES"]
timestamp="2026-09-23T01:02:03.123456+00:00"
counts={"node_counts":0,"settings":0,"writes":0,"records":0}
def env():return {k:os.environ[k] for k in keys if k in os.environ}
def nodes(text):
    counts["node_counts"]+=1
    expected=old_nodes(text)
    actual=native.result("querylog.nodes",text=text)
    if actual!=expected:native.fail(text=text,expected=expected,actual=actual)
    return actual
def settings():
    counts["settings"]+=1
    expected={"path":str(p) if (p:=old_path()) else None,"responses":old_responses()}
    # The engine's default log path is its own (~/.cache/graphicle/queries.log).
    actual=to_upstream(native.result("querylog.settings",env=env(),home=str(Path.home())))
    if actual!=expected:native.fail(env=env(),expected=expected,actual=actual)
    return actual
def log_path():
    path=settings()["path"]
    return Path(path) if path else None
def log_responses():return settings()["responses"]
def log_query(**kwargs):
    counts["writes"]+=1
    standard={key:kwargs.pop(key) for key in ["kind","question","corpus","result","nodes_returned","duration_ms"] if key in kwargs}
    standard["extra"]=kwargs
    native.result("querylog.write",parameters=standard,env=env(),home=str(Path.home()),timestamp=timestamp)

try:
    reference.nodes_from_result=nodes;reference._log_path=log_path;reference._log_responses=log_responses;reference.log_query=log_query
    tests=upstream_tests("tests/test_querylog.py")
    for digits in ["0","1","232","٠١٢","१२३","𝟙𝟚𝟛","一二","²³"]:
        for whitespace in [" ","\t","\x1f","\u2003","\n"]:
            for word in ["node","nodes","NODES"]:nodes(f"Result: {digits}{whitespace}{word}{whitespace}found")
    rng=random.Random(20260923)
    class FrozenDatetime:
        @classmethod
        def now(cls,zone):return datetime.datetime.fromisoformat(timestamp)
    with tempfile.TemporaryDirectory(prefix="graphify-querylog-parity-") as directory:
        root=Path(directory)
        for index in range(300):
            parameters={"kind":"query","question":"日本語\n<hello>","corpus":"/repo/graph.json","result":rng.choice([None,"3 nodes found\nα💡","","words"]),
                "duration_ms":rng.choice([1.2345,1.2355,0.0005,-1.2345,rng.uniform(-10000,10000)]),"nodes_returned":rng.choice([None,0,4]),"extra":{"mode":"bfs","depth":2,"null":None}}
            if index%5==0:parameters["extra"].update({"ts":"extra overrides standard","result_chars":999,"response":"extra"})
            path=root/f"{index}.jsonl"
            environment={"GRAPHIFY_QUERY_LOG":str(path),"GRAPHIFY_QUERY_LOG_RESPONSES":rng.choice(["","1","false","yes","TRUE"])}
            with patch.dict(os.environ,{k:"" for k in keys}|environment),patch.object(reference,"datetime",FrozenDatetime):
                old_log(**{k:v for k,v in parameters.items() if k!="extra"},**parameters["extra"])
            expected=json.loads(path.read_text())
            actual=native.result("querylog.record",parameters=parameters,timestamp=timestamp,responses=environment["GRAPHIFY_QUERY_LOG_RESPONSES"].lower() in ["1","true","yes"])
            counts["records"]+=1
            if actual!=expected:native.fail(parameters=parameters,expected=expected,actual=actual)
        for overrides in [{},{"GRAPHIFY_QUERY_LOG_ENABLE":"YES"},{"GRAPHIFY_QUERY_LOG_ENABLE":"true "},{"GRAPHIFY_QUERY_LOG":"~/test.log"},{"GRAPHIFY_QUERY_LOG":" ~root/test.log "},{"GRAPHIFY_QUERY_LOG":"\x1f/tmp/test\x1f"},{"GRAPHIFY_QUERY_LOG_DISABLE":"yes","GRAPHIFY_QUERY_LOG":"/tmp/ignored"}]:
            with patch.dict(os.environ,{k:"" for k in keys}|overrides):settings()
finally:native.close()

native.receipt("graphify/querylog.py",["src/querylog.rs"],checked=counts,upstream_tests=tests,
    comparisons="Actual append-only writes, parent creation, off-by-default behavior, disable precedence, response opt-in, Unicode counts, UTC record schema, exact rounding and silent filesystem failure.",
    native_adaptations=["Native JSONL uses compact JSON spacing; decoded records and line boundaries match.","Parsed node counts use checked u64; out-of-range counts return None."])
