import asyncio
import importlib.util
import json
import os
from pathlib import Path

import httpx
import pytest

# The endpoint comes from the environment; the mock transport never reaches it.
os.environ.setdefault("AZURE_OPENAI_BASE_URL", "https://azure.example.invalid/openai/v1")
MODULE = Path(__file__).resolve().parents[1] / "tools/azure_memory_eval.py"
spec = importlib.util.spec_from_file_location("azure_memory_eval", MODULE)
api = importlib.util.module_from_spec(spec)
spec.loader.exec_module(api)


def completion(model="gpt-5-2025-08-07", text="yes", finish="stop"):
    return {"model": model, "choices": [{"finish_reason": finish, "message": {"content": text}}],
            "usage": {"prompt_tokens": 20, "completion_tokens": 3, "total_tokens": 23}}


def test_duplicate_requests_share_receipt_without_leaking_key(tmp_path):
    calls = []

    def respond(request):
        assert request.headers["api-key"] == "sensitive-test-key"
        body = json.loads(request.content)
        assert body["store"] is False
        assert "temperature" not in body
        calls.append(body)
        return httpx.Response(200, json=completion())

    async def check():
        engine = api.AzureBatch(tmp_path, key="sensitive-test-key", transport=httpx.MockTransport(respond))
        request = api.payload("gpt-5", api.messages("hello"))
        a, b = await asyncio.gather(engine.call(request), engine.call(request))
        await engine.close()
        assert a["ok"] and b["ok"]
        assert a["cache_hit"] != b["cache_hit"]
        assert a["receipt"] == b["receipt"]
        assert "sensitive-test-key" not in Path(a["receipt"]).read_text()

    asyncio.run(check())
    assert len(calls) == 1


@pytest.mark.parametrize("body", [completion(model="unexpected-new-version"), completion(finish="length")])
def test_version_changes_and_truncated_completions_are_not_success(tmp_path, body):
    async def check():
        engine = api.AzureBatch(tmp_path, key="secret", transport=httpx.MockTransport(lambda r: httpx.Response(200, json=body)))
        result = await engine.call(api.payload("gpt-5", api.messages("hello")))
        await engine.close()
        assert not result["ok"]
        assert result["attempts"][0]["response"] == body
    asyncio.run(check())


def test_auth_failure_stops_batch_and_does_not_record_provider_body(tmp_path):
    calls = []

    def respond(request):
        calls.append(request)
        return httpx.Response(401, json={"error": {"message": "secret echoed"}})

    async def check():
        engine = api.AzureBatch(tmp_path, key="secret", transport=httpx.MockTransport(respond))
        first = await engine.call(api.payload("gpt-5", api.messages("one")))
        second = await engine.call(api.payload("gpt-5", api.messages("two")))
        await engine.close()
        assert not first["ok"] and not second["ok"]
        assert len(calls) == 1
        assert "secret" not in Path(first["receipt"]).read_text()
    asyncio.run(check())


def test_resume_preserves_failed_completion_instead_of_sampling_again(tmp_path):
    calls = []

    def respond(request):
        calls.append(request)
        return httpx.Response(200, json=completion(finish="length" if len(calls) == 1 else "stop"))

    async def check():
        request = api.payload("gpt-5", api.messages("one frozen question"))
        for expected_cache_hit in (False, True):
            engine = api.AzureBatch(tmp_path, key="secret", transport=httpx.MockTransport(respond))
            result = await engine.call(request)
            await engine.close()
            assert not result["ok"]
            assert result["finish_reason"] == "length"
            assert result["cache_hit"] is expected_cache_hit
            assert result["usage"]["completion_tokens"] == 3
        assert len(calls) == 1

    asyncio.run(check())


def test_independent_runners_share_one_immutable_outcome(tmp_path):
    calls = []

    async def respond(request):
        calls.append(request)
        await asyncio.sleep(0.03)
        return httpx.Response(200, json=completion())

    async def check():
        engines = [api.AzureBatch(tmp_path, key="secret", transport=httpx.MockTransport(respond))
                   for _ in range(2)]
        request = api.payload("gpt-5", api.messages("same campaign request"))
        try:
            rows = await asyncio.gather(*(e.call(request) for e in engines))
        finally:
            await asyncio.gather(*(e.close() for e in engines))
        assert all(r["ok"] for r in rows)
        assert sum(r["cache_hit"] for r in rows) == 1
        assert len(calls) == 1

    asyncio.run(check())


def test_frozen_manifest_rejects_different_model_or_question_population(tmp_path):
    path = tmp_path / "PROTOCOL.json"
    api.freeze(path, {"model": "gpt-5", "ids": ["a", "b"]})
    api.freeze(path, {"model": "gpt-5", "ids": ["a", "b"]})
    with pytest.raises(ValueError, match="Frozen inputs changed"):
        api.freeze(path, {"model": "gpt-5.4", "ids": ["a"]})


def test_judge_does_not_pass_negation_or_thinking_containing_yes():
    assert api.strict_yes_no("Yes.") is True
    assert api.strict_yes_no("not yes") is None
    assert api.strict_yes_no("<thinking>yes</thinking> no") is None
    assert api.final_yes_no("<judge_thinking>First I considered yes.</judge_thinking>\nno") is False
    assert api.final_yes_no("<judge_thinking>yes</judge_thinking>") is None
