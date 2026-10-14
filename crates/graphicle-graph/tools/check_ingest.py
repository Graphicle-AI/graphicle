#!/usr/bin/env python3
"""Pinned ingestion formats, native IO and concurrent persistence checks."""
import contextlib, datetime, io, json, random, sys, tempfile, threading, types, uuid
from pathlib import Path
from unittest.mock import patch
from parity import Native, ROOT, SOURCE, upstream_tests
from naming import to_upstream
import graphify.ingest as upstream
import yaml

reference=types.ModuleType("graphify_ingest_reference")
exec(compile((SOURCE/"graphify/ingest.py").read_text(),str(SOURCE/"graphify/ingest.py"),"exec"),reference.__dict__)
instant=datetime.datetime(2026,9,23,12,34,56,123456,tzinfo=datetime.timezone.utc)
class FixedDate(datetime.datetime):
    @classmethod
    def now(cls,tz=None): return instant
reference.datetime=FixedDate
native=Native("ingest")
lock=threading.Lock()
counts={"yaml":0,"filenames":0,"types":0,"html_basic":0,"captures":0,"queries":0}
def call(function,**kwargs):
    with lock: return native.result("ingest",function=function,**kwargs)
def same(function,expected,**kwargs):
    actual=to_upstream(call(function,**kwargs)["value"])
    if actual!=expected:native.fail(function=function,kwargs=kwargs,expected=expected,actual=actual)
    return actual
def bridge(question,answer,memory_dir,query_type="query",source_nodes=None,outcome=None,correction=None):
    return Path(call("save_query_result",question=question,answer=answer,memory_dir=str(memory_dir),options=dict(query_type=query_type,source_nodes=source_nodes or [],outcome=outcome,correction=correction))["value"])
original=upstream.save_query_result
try:
    upstream.save_query_result=bridge
    tests=upstream_tests("tests/test_ingest.py")
    upstream.save_query_result=original
    # First run here on 14 October 2026: URL classification. Each call returns
    # the native type after comparing it with the reference's.
    original_detect=upstream._detect_url_type
    upstream._detect_url_type=lambda url:same("_detect_url_type",reference._detect_url_type(url),url=url)
    try:url_tests=upstream_tests("tests/test_ingest_url_type.py")
    finally:upstream._detect_url_type=original_detect
    rng=random.Random(75821)
    alphabet=list('title"\\\t\n\r 中文 A_1-')+[chr(i) for i in range(33)]+["\x7f","\u2028","\u2029"]
    for value in [None,True,False,3,{},["x"],""]+["".join(rng.choices(alphabet,k=rng.randrange(80))) for _ in range(100)]:
        same("_yaml_str",reference._yaml_str(value),value=value);counts["yaml"]+=1
    urls=["https://example.org/a/b.pdf?query=1#f","https://example.org/a.JPG","https://x.com/person/status/15","https://twitter.com/x", "https://arxiv.org/abs/2501.12345", "https://github.com/a/b","https://youtu.be/abc","https://example.org/file.tar.gz","ftp://example.org/a","//example.org/a/","https://例子.测试/中文/é-😀","/local/path","http://USER:pass@EXAMPLE.org/a;b","https://example.org","https://youtube.com/file.pdf","https://example.org/a.gif;ignored", "https://example.org/"+"a"*150,"https://example.org/.jpg"]
    for url in urls:
        same("_detect_url_type",reference._detect_url_type(url),url=url);counts["types"]+=1
        for suffix in [".md",".pdf",".JPG",""]:
            same("_safe_filename",reference._safe_filename(url,suffix),url=url,suffix=suffix);counts["filenames"]+=1
    html_cases=["", "<p>hello <b>bold</b> text</p>","<SCRIPT>bad()\n</SCRIPT><p>ok</p>","<style>.x{}</style><p>A&nbsp; B</p>","<h1>Title</h1><p>"+("日本語 "*3000)+"</p>","<a href='/x'>link</a><img src='file' alt='removed'>","<script type='x'>drop</script><STYLE>more drop</STYLE>retained","<div>line\u001c two\n three</div>"]
    with patch.dict(sys.modules,{"markdownify":None}):
        for html in html_cases:
            same("_html_to_markdown",reference._html_to_markdown(html,"https://example.org"),html=html,html_mode="basic");counts["html_basic"]+=1
    html="<h1>Title</h1><p>Hello <strong>reader</strong>.</p><ul><li>One</li><li>Two</li></ul><p><a href='https://example.org/docs'>Documentation</a></p><script>secret_script</script><style>secret_style</style><img src='x' alt='secret_image'>"
    rich=call("_html_to_markdown",html=html)["value"]
    assert "# Title" in rich and "One" in rich and "Two" in rich and "https://example.org/docs" in rich
    assert all(s not in rich for s in ("secret_script","secret_style","secret_image"))
    with tempfile.TemporaryDirectory() as temporary:
        root=Path(temporary)
        for i in range(50):
            question="".join(rng.choices(alphabet,k=rng.randrange(80)))
            answer="Source-backed answer.\n"+str(i)
            options={"query_type":rng.choice(["query","path_query","explain"]),"source_nodes":[f"node {j}" for j in range(rng.randrange(17))],"outcome":rng.choice([None,"useful","dead_end","corrected"]),"correction":rng.choice([None,"","Actually A, see source."])}
            with patch.object(reference.uuid,"uuid4",return_value=types.SimpleNamespace(hex="1234abcd"*4)):
                expected=reference.save_query_result(question,answer,root/f"reference-{i}",**options)
            same("query_filename",expected.name,question=question,now=instant.isoformat(),uuid_prefix="1234abcd")
            same("query_content",expected.read_bytes().decode(),question=question,answer=answer,now=instant.isoformat(),options=options);counts["queries"]+=1
        for options in [{"outcome":""},{"outcome":"great"},{"outcome":"CORRECTED"}]:
            result=native.call("ingest",function="save_query_result",question="Q",answer="A",memory_dir=str(root/"invalid"),options=options)
            assert not result["ok"] and not (root/"invalid").exists()
        # Both the field omitted by the original and the normal escaped fields
        # must remain one YAML scalar, not injected sibling keys.
        hostile='query"\nadmin: true\nx: "'
        path=Path(call("save_query_result",question='q"\nkey: true',answer="A",memory_dir=str(root/"escaped"),options={"query_type":hostile})["value"])
        fm=yaml.safe_load(path.read_text().split("---",2)[1]);assert fm["type"]==hostile and "admin" not in fm
        paths=call("save_concurrent",question="same question",answer="kept",memory_dir=str(root/"concurrent"),count=64)["value"]
        assert len(set(paths))==64 and len(list((root/"concurrent").glob("*.md")))==64
        assert all("kept" in Path(p).read_text() for p in paths)
        # Fetch functions use the same native protected transport interface;
        # fixed response bodies keep source-format comparisons deterministic.
        for i in range(24):
            kind=["webpage","tweet","arxiv"][i%3]
            url={"webpage":f"https://example.org/page{i}","tweet":f"https://x.com/user/status/{i}","arxiv":f"https://arxiv.org/abs/2609.{12345+i}"}[kind]
            options={"author":rng.choice([None,"","author"]), "contributor":rng.choice([None,"","team"])}
            html=f'<title>A  title\n{i}</title><h1 class="title mathjax">Title: <b>Paper</b></h1><div class="authors">A and B</div><blockquote class="abstract mathjax">Abstract: body.</blockquote><p>Actual source</p><script>drop</script>'
            tweet=json.dumps({"html":f"<blockquote>tweet {i} &amp; source</blockquote>","author_name":"some author"})
            api={"webpage":url,"tweet":"https://publish.twitter.com/oembed?url="+reference.urllib.parse.quote(url.replace("x.com","twitter.com"))+"&omit_script=true","arxiv":f"https://export.arxiv.org/abs/2609.{12345+i}"}[kind]
            body=tweet if kind=="tweet" else html
            def fetch(u):
                assert u==api,(u,api)
                return body
            with patch.object(reference,"safe_fetch_text",side_effect=fetch),patch.dict(sys.modules,{"markdownify":None}):
                expected=getattr(reference,"_fetch_"+kind)(url,**options)
            same("_fetch_"+kind,list(expected),url=url,responses={api:{"body":body}},now=instant.isoformat(),html_mode="basic",**options)
            result=call("ingest",url=url,target_dir=str(root/"captures"),responses={api:{"body":body}},now=instant.isoformat(),html_mode="basic",**options)
            assert Path(result["value"]).read_text()==expected[0]
            counts["captures"]+=1
        for kind,url in [("tweet","https://x.com/user/status/no"),("arxiv","https://arxiv.org/abs/1234.56789")]:
            with patch.object(reference,"safe_fetch_text",side_effect=OSError("unavailable")):
                expected=getattr(reference,"_fetch_"+kind)(url,None,None)
            same("_fetch_"+kind,list(expected),url=url,responses={},now=instant.isoformat())
        # Distinct captures, binary routes, YouTube wiring and failure behavior.
        url="https://example.org/same"
        for i in range(3):
            p=call("ingest",url=url,target_dir=str(root/"repeat"),responses={url:{"body":"<title>x</title>x"}},html_mode="basic")["value"]
            assert Path(p).name==("example_org_same"+(f"_{i}" if i else "")+".md")
        for suffix in [".pdf",".PNG",".jpg"]:
            url="https://example.org/file"+suffix
            p=call("ingest",url=url,target_dir=str(root/"binary"),responses={url:{"bytes":[0,1,2,255]}})["value"]
            assert Path(p).read_bytes()==bytes([0,1,2,255]) and Path(p).suffix==suffix
        p=call("ingest",url="https://youtu.be/item",target_dir=str(root/"youtube"),responses={},audio={"filename":"download.opus","body":"audio"})["value"]
        assert Path(p).read_text()=="audio"
        for url in ["file:///etc/passwd","http://127.0.0.1/private","http://[::1]/private","https://metadata.google.internal/"]:
            result=native.call("ingest",function="ingest",url=url,target_dir=str(root/"blocked"),responses={})
            assert not result["ok"] and "ingest:" in result["error"],result
            assert not list((root/"blocked").iterdir())
        for error in ["HTTPError: HTTP Error 404: Not Found","URLError: unavailable","OSError: incomplete"]:
            url="https://example.org/bad"
            result=native.call("ingest",function="ingest",url=url,target_dir=str(root/"failed"),responses={url:{"error":error}})
            assert not result["ok"] and "ingest: failed to fetch" in result["error"]
        # One public page exercises the actual protected HTTP implementation.
        real=call("ingest",url="https://example.com",target_dir=str(root/"public-page"),execute=True)
        body=Path(real["value"]).read_text()
        assert "Example Domain" in body and 'source_url: "https://example.com"' in body
        real_capture={"url":"https://example.com","saved_bytes":len(body.encode()),"source_url_retained":True}
    native.receipt("graphify/ingest.py",["src/ingest.rs","src/ingest_api.rs"],original_tests=tests,url_type_tests=url_tests,comparisons=counts,concurrent_native_saves=64,live_public_capture=real_capture,
        adaptations=["Native HTML conversion uses html2text 0.15.5 instead of optional Python markdownify. Rich layout/Markdown whitespace can differ; the original basic-strip mode is retained and compared exactly.",
        "Query type is escaped like the other YAML fields. Exclusive file creation prevents concurrent overwrites; exhausted capture names now return an error instead of overwriting the last file.",
        "CLI argument registration belongs to the separately tracked native application entry point. No Python Graphify code or paid model API is invoked at runtime."])
finally:
    upstream.save_query_result=original
    native.close()
