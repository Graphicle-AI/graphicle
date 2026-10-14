#!/usr/bin/env python3
"""Pinned transcription contracts, real downloader subprocess and CPU audio inference."""
import contextlib, hashlib, io, json, os, random, sys, tempfile, types
from pathlib import Path
from unittest.mock import patch
from parity import Native, ROOT, SOURCE, upstream_tests
import graphify.transcribe as upstream

reference = types.ModuleType("graphify_transcribe_reference")
exec(compile((SOURCE/"graphify/transcribe.py").read_text(), str(SOURCE/"graphify/transcribe.py"), "exec"), reference.__dict__)
native = Native("transcribe")
counts = {"prompts":0, "url_classification":0, "transcript_cases":0, "download_cases":0}
def call(method, **kwargs):
    result=native.result("transcribe",function=method,**kwargs)
    return result
def same(method, expected, **kwargs):
    actual=call(method,**kwargs)["value"]
    if actual!=expected:native.fail(method=method,kwargs=kwargs,expected=expected,actual=actual)
    return actual
def text(log): return "".join(line+"\n" for line in log)
def current_env(): return {k:v for k,v in os.environ.items() if k.startswith("GRAPHIFY_WHISPER")}
def bridge_prompt(nodes): return call("build_whisper_prompt",nodes=nodes,env=current_env())["value"]
def bridge_transcribe(path,output_dir=None,initial_prompt=None,force=False):
    return Path(call("transcribe",path=str(path),output_dir=str(output_dir) if output_dir else None,initial_prompt=initial_prompt,force=force)["value"])
def bridge_all(paths,output_dir=None,initial_prompt=None):
    return call("transcribe_all",paths=paths,output_dir=str(output_dir) if output_dir else None,initial_prompt=initial_prompt)["value"]
selected=[
    "test_video_extensions_set","test_build_whisper_prompt_no_nodes",
    "test_build_whisper_prompt_env_override","test_build_whisper_prompt_returns_topic_string",
    "test_build_whisper_prompt_nodes_without_labels","test_transcribe_uses_cache",
    "test_transcribe_all_empty","test_transcribe_all_uses_cache",
]
originals={name:getattr(upstream,name) for name in ("VIDEO_EXTENSIONS","build_whisper_prompt","transcribe","transcribe_all")}
try:
    upstream.VIDEO_EXTENSIONS=set(call("constants")["value"]["VIDEO_EXTENSIONS"])
    upstream.build_whisper_prompt=bridge_prompt
    upstream.transcribe=bridge_transcribe
    upstream.transcribe_all=bridge_all
    tests=upstream_tests(*["tests/test_transcribe.py::"+name for name in selected])
    for name,value in originals.items():setattr(upstream,name,value)
    rng=random.Random(731)
    for i in range(100):
        nodes=[rng.choice([{},{"label":""},{"label":"Python"},{"label":"神经网络"},{"label":" A\n B "},{"label":None}]) for _ in range(rng.randrange(18))]
        for override in ("","Custom domain hint."):
            with patch.dict(os.environ,{"GRAPHIFY_WHISPER_PROMPT":override}):
                same("build_whisper_prompt",reference.build_whisper_prompt(nodes),nodes=nodes,env={"GRAPHIFY_WHISPER_PROMPT":override})
                counts["prompts"]+=1
    for path in ["http://x","https://x","www.x","HTTP://x","ftp://x","/www.x","www.",".mp4","a https://x","file:///x","", "https://日本語.test/音声"]:
        same("is_url",reference.is_url(path),path=path);counts["url_classification"]+=1
    with tempfile.TemporaryDirectory() as temporary:
        root=Path(temporary)
        for i in range(60):
            audio=root/f"Lecture {i} 日本語.mp4";audio.write_bytes(b"fixture")
            out=root/f"out-{i}";out.mkdir();target=out/(audio.stem+".txt")
            cache=i%3==0;force=i%2==0
            if cache:target.write_text("Cached\ntext")
            segments=[rng.choice([" hello ", "", "\u001c spaced\u001f", "\nword\n", "中文。", "\t", "line one\nline two", "é"]) for _ in range(rng.randrange(10))]
            info=types.SimpleNamespace(language="en" if i%2 else "zh")
            calls=[]
            class Model:
                def __init__(self,*args,**kw): pass
                def transcribe(self,path,**kw):
                    calls.append({"path":path,"model":"tiny","initial_prompt":kw["initial_prompt"],"beam_size":kw["beam_size"]})
                    return [types.SimpleNamespace(text=s) for s in segments],info
            prompt=None if i%3==0 else "Domain-specific words."
            output=io.StringIO()
            with patch.object(reference,"_get_whisper",return_value=Model),patch.dict(os.environ,{"GRAPHIFY_WHISPER_MODEL":"tiny"}),contextlib.redirect_stdout(output):
                expected=reference.transcribe(audio,out,prompt,force)
            expected_bytes=target.read_bytes()
            if cache:target.write_text("Cached\ntext")
            else:target.unlink()
            actual=call("transcribe",path=str(audio),output_dir=str(out),initial_prompt=prompt,force=force,env={"GRAPHIFY_WHISPER_MODEL":"tiny"},responses=[{"segments":segments,"language":info.language}])
            if actual["value"]!=str(expected) or target.read_bytes()!=expected_bytes or text(actual["stdout"])!=output.getvalue() or actual["speech_calls"]!=calls:
                native.fail(case=i,expected={"path":str(expected),"text":expected_bytes.decode(),"stdout":output.getvalue(),"calls":calls},actual=actual)
            counts["transcript_cases"]+=1
        paths=[str(root/"first.wav"),str(root/"bad.wav"),str(root/"last.wav")]
        result=call("transcribe_all",paths=paths,output_dir=str(root/"multi"),responses=[{"path":paths[0],"segments":["First."]},{"path":paths[1],"error":"RuntimeError: boom"},{"path":paths[2],"segments":["Last."]}])
        assert len(result["value"])==2 and len(result["speech_calls"])==3
        assert any("bad.wav: boom" in s for s in result["stdout"])
        assert not (root/"multi"/"bad.txt").exists()
        # The official downloader remains a separate executable. This fixture
        # verifies the native process boundary, not a Python callback in Rust.
        executable=root/"fake yt-dlp";control=root/"download-control.json";invocation=root/"argv.json"
        executable.write_text("#!"+sys.executable+"\n"+"""
import json,os,sys
from pathlib import Path
c=json.loads(Path(os.environ['GRAPHIFY_DOWNLOAD_CONTROL']).read_text())
Path(os.environ['GRAPHIFY_DOWNLOAD_LOG']).write_text(json.dumps(sys.argv[1:]))
if c.get('failure'):
    sys.stderr.write('download rejected');sys.exit(1)
t=sys.argv[sys.argv.index('--output')+1]
p=Path(t.replace('%(ext)s',c.get('actual_ext','m4a')))
p.write_bytes(b'audio fixture')
print(json.dumps({'ext':c.get('metadata_ext','m4a')}))
""")
        executable.chmod(0o755)
        os.environ["GRAPHIFY_DOWNLOAD_CONTROL"]=str(control)
        os.environ["GRAPHIFY_DOWNLOAD_LOG"]=str(invocation)
        # Reopen only to inherit fixture command settings. No caches are tested
        # across this process change.
        native.cwd=None;native.use_working_directory(ROOT)
        env={"GRAPHIFY_YT_DLP":str(executable)}
        for i,(actual_ext,meta_ext) in enumerate([("m4a","m4a"),("opus","opus"),("webm","m4a")]):
            url=f"https://93.184.216.34/watch?v=fixture-{i}"
            out=root/f"audio {i}";control.write_text(json.dumps({"actual_ext":actual_ext,"metadata_ext":meta_ext}))
            result=call("download_audio",url=url,output_dir=str(out),env=env,execute=True)
            expected=out/("yt_"+hashlib.sha1(url.encode()).hexdigest()[:12]+"."+actual_ext)
            assert result["value"]==str(expected) and expected.read_bytes()==b"audio fixture"
            argv=json.loads(invocation.read_text())
            assert argv[-2:]==["--",url] and argv[argv.index("--format")+1]=="bestaudio[ext=m4a]/bestaudio/best"
            assert "--no-playlist" in argv and "--no-simulate" in argv
            result=call("download_audio",url=url,output_dir=str(out),env=env,execute=True)
            assert result["value"]==str(expected) and result["stdout"]==["  cached audio: "+expected.name]
            counts["download_cases"]+=2
        for bad in ["file:///etc/passwd","http://127.0.0.1/clip","http://[::1]/clip"]:
            result=native.call("transcribe",function="download_audio",url=bad,output_dir=str(root/"blocked"),execute=True,env=env)
            assert not result["ok"] and "Blocked" in result["error"]
            assert not (root/"blocked").exists();counts["download_cases"]+=1
        control.write_text(json.dumps({"failure":True}))
        result=native.call("transcribe",function="download_audio",url="https://93.184.216.34/fail",output_dir=str(root/"failed"),execute=True,env=env)
        assert not result["ok"] and "download rejected" in result["error"];counts["download_cases"]+=1
        # Model download and real CPU inference are part of this receipt. The
        # model cache is <XDG_CACHE_HOME or ~/.cache>/graphicle/whisper; a set
        # XDG_CACHE_HOME is forwarded so a run can reuse an already cached model.
        fixture=ROOT/"migration/fixtures/transcribe/jfk.wav"
        env={k:os.environ[k] for k in ("PATH","HOME","XDG_CACHE_HOME") if k in os.environ}
        env["GRAPHIFY_WHISPER_MODEL"]="tiny.en"
        env["GRAPHIFY_WHISPER_THREADS"]="4"
        result=call("transcribe",path=str(fixture),output_dir=str(root/"real"),env=env,execute=True)
        spoken=Path(result["value"]).read_text()
        normalized=" ".join("".join(c.lower() if c.isalnum() else " " for c in spoken).split())
        assert "ask not what your country can do for you" in normalized,spoken
        assert "ask what you can do for your country" in normalized,spoken
        cached=call("transcribe",path=str(fixture),output_dir=str(root/"real"),env={"GRAPHIFY_WHISPER_MODEL":"missing"},execute=False)
        assert cached["value"]==result["value"] and not cached["stdout"]
        bad=root/"bad.wav";bad.write_text("invalid media")
        rejected=native.call("transcribe",path=str(bad),output_dir=str(root/"rejected"),env=env,execute=True)
        assert not rejected["ok"] and "ffmpeg" in rejected["error"]
        assert not (root/"rejected/bad.txt").exists()
        real={"engine":"whisper-rs 0.16.0 / whisper.cpp","device":"CPU","model":"tiny.en","model_catalogue":json.loads((ROOT/"src/assets/transcribe/models.json").read_text()),"sample":json.loads((ROOT/"migration/fixtures/transcribe/jfk.json").read_text()),"transcript":spoken,"invalid_media_rejected":True,"cached_transcript_skips_engine":True}
    native.receipt("graphify/transcribe.py",["src/transcribe.rs","src/transcribe_native.rs","src/transcribe_api.rs","src/assets/transcribe/models.json"],
        original_tests=tests,comparisons=counts,real_inference=real,
        deferred_original_tests=["test_transcribe_force_reruns","test_transcribe_missing_faster_whisper","test_transcribe_all_skips_failed"],
        adaptations=["Python faster-whisper/CTranslate2 inference is replaced by native whisper.cpp via Rust bindings, with beam size 5, automatic language detection and the same prompt. Inference text/quantization/performance equivalence across these engines is not claimed.",
        "FFmpeg remains a separately installed native media decoder. URL audio uses the separately installed official yt-dlp executable; this is not a rewrite of that external downloader.",
        "Failed downloads with no resulting audio are errors. Native media/model errors and explicit audio-size/deadline limits use native wording.",
        "Original Python monkeypatch-specific inference tests are replaced by independent recorded-segment contracts, actual subprocess and actual CPU inference checks; no paid model API is used."])
finally:
    for name,value in originals.items():setattr(upstream,name,value)
    native.close()
