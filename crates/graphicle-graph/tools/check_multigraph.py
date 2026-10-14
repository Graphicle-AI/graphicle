#!/usr/bin/env python3
"""Probe actual native graph behavior and compare node-link data with NetworkX."""
import random
from parity import Native
import networkx as nx
from networkx.readwrite import json_graph
from graphify.multigraph_compat import probe_multigraph_capabilities

native=Native("multigraph_compat")
counts={"scenarios":0,"roundtrips":0}
rng=random.Random(20260923)
try:
    expected=probe_multigraph_capabilities()
    actual=native.result("graph.capabilities")
    assert actual["runtime"]=="Rust" and actual["ok"]
    assert actual["checks"]==[{"name":check.name,"ok":check.ok,"detail":check.detail} for check in expected.checks]
    failure=native.result("graph.capability_message",result={"runtime":"Rust","backend":"test backend","checks":[{"name":"round_trip","ok":False,"detail":"boom"}]})
    assert failure["ok"] is False and len(failure["failed"])==1
    assert "round_trip: boom" in failure["message"] and "Default simple graph mode remains available" in failure["message"]
    for iteration in range(750):
        directed=rng.choice([False,True]);multi=rng.choice([False,True])
        graph=(nx.MultiDiGraph if directed and multi else nx.DiGraph if directed else nx.MultiGraph if multi else nx.Graph)()
        actions=[]
        for i in range(rng.randrange(1,35)):
            choice=rng.randrange(10)
            source,target=rng.choices(["a","b","c","d","e","f",1,2],k=2)
            if choice<2:
                attributes={"label":str(source),"value":i}
                graph.add_node(source,**attributes)
                actions.append({"kind":"node","id":source,"attributes":attributes})
            elif choice<8:
                attributes={"relation":rng.choice(["calls","imports","contains"]),"iteration":i}
                action={"kind":"edge","source":source,"target":target,"attributes":attributes}
                if multi and rng.random()<.7:action["key"]=rng.choice([None,0,1,"same","other"])
                if multi:graph.add_edge(source,target,key=action.get("key"),**attributes)
                else:graph.add_edge(source,target,**attributes)
                actions.append(action)
            elif choice==8:
                edge_key=rng.choice([None,0,"same"])
                action={"kind":"remove","source":source,"target":target}
                if edge_key is not None and multi:action["key"]=edge_key
                try:
                    if multi:graph.remove_edge(source,target,key=edge_key)
                    else:graph.remove_edge(source,target)
                except nx.NetworkXError:pass
                actions.append(action)
            else:
                graph=graph.to_undirected();actions.append({"kind":"undirected"})
        expected=json_graph.node_link_data(graph,edges="links")
        actual=native.result("graph.check_scenario",directed=directed,multigraph=multi,actions=actions)
        counts["scenarios"]+=1
        if actual["graph"]!=expected or actual["errors"]:native.fail(actions=actions,directed=directed,multigraph=multi,expected=expected,actual=actual)
        loaded=native.result("graph.check_scenario",data=expected)
        reference=json_graph.node_link_data(json_graph.node_link_graph(expected,edges="links"),edges="links")
        counts["roundtrips"]+=1
        if loaded["graph"]!=reference:native.fail(data=expected,expected=reference,actual=loaded)
finally:native.close()
native.receipt("graphify/multigraph_compat.py",["src/multigraph_compat.rs","src/graph.rs"],checked=counts,
    upstream_reference={"networkx":nx.__version__,"capability_checks":6,"passed":expected is not None},
    comparisons="Six actual backend probes plus ordered node-link round-trips, simple/directed/multigraph modes, automatic and explicit keys, overwrite, removal, reverse/self edges and undirected conversion.",
    native_adaptations=["Capability metadata and diagnostic messages identify Rust and the native backend; Python/NetworkX versions are not claimed for native execution.","Undirected view is an immutable borrowed Rust view; mutability follows Rust borrowing rather than Python object aliasing."])
