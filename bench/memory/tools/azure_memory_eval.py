#!/usr/bin/env python3
"""Auditable Azure batch transport for frozen memory benchmark prompts.

Credentials and the endpoint (AZURE_OPENAI_API_KEY1, AZURE_OPENAI_BASE_URL)
stay in the process environment. Every distinct request has an
atomic, content-addressed receipt containing the prompt, model snapshot,
response, usage and latency. Failed/missing questions remain in denominators.
This module does not choose prompts, retrieve memories or inspect gold labels.
"""
from __future__ import annotations

import asyncio
from contextlib import asynccontextmanager
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import random
import re
import statistics
import time

import httpx

# The Azure OpenAI v1 endpoint of the evaluation's resource, for example
# https://<resource>.openai.azure.com/openai/v1. Like the key, it comes from
# the process environment; each receipt's request identity names it.
BASE_URL = os.environ.get("AZURE_OPENAI_BASE_URL", "").rstrip("/")
MODELS = {
    "gpt-4.1-mini": "gpt-4.1-mini-2025-04-14",
    "gpt-4o": "gpt-4o-2024-11-20",
    "gpt-5": "gpt-5-2025-08-07",
    "gpt-5.4": "gpt-5.4-2026-03-05",
}


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, ensure_ascii=False,
                                     separators=(",", ":")).encode()).hexdigest()


def file_sha(path):
    h = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def save(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + f".{os.getpid()}.tmp")
    with tmp.open("w") as stream:
        json.dump(value, stream, indent=2, ensure_ascii=False, allow_nan=False)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    tmp.replace(path)


def freeze(path, value):
    path = Path(path)
    if path.exists():
        if json.loads(path.read_text()) != value:
            raise ValueError(f"Frozen inputs changed: {path}; choose a new run directory")
    else:
        save(path, value)


def payload(model, messages, *, max_tokens=4096, temperature=0, reasoning=None,
            json_output=False):
    if model not in MODELS:
        raise ValueError(f"Unprobed deployment: {model}")
    value = {"model": model, "messages": messages, "store": False}
    if model.startswith("gpt-5"):
        value["max_completion_tokens"] = max_tokens
        # None preserves the upstream client's default; medium is explicit
        # for the separately named GPT-5.4 comparison.
        if reasoning is not None:
            value["reasoning_effort"] = reasoning
    else:
        value.update(max_tokens=max_tokens, temperature=temperature)
    if json_output:
        value["response_format"] = {"type": "json_object"}
    return value


def messages(user, system=""):
    return ([{"role": "system", "content": system}] if system else []) + [
        {"role": "user", "content": user}]


@asynccontextmanager
async def campaign_lock(path):
    """Share one immutable outcome across concurrent local campaign runners."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.with_suffix('.lock').open('a') as lock:
        try:
            while True:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    await asyncio.sleep(0.1)
            yield
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)


class AzureBatch:
    def __init__(self, cache, *, workers=16, timeout=240, transport=None, key=None):
        self.cache = Path(cache)
        self.semaphore = asyncio.Semaphore(workers)
        self.key = key if key is not None else os.environ.get("AZURE_OPENAI_API_KEY1")
        if not self.key:
            raise ValueError("AZURE_OPENAI_API_KEY1 is not set")
        if not BASE_URL:
            raise ValueError("AZURE_OPENAI_BASE_URL is not set")
        self.client = httpx.AsyncClient(
            base_url=BASE_URL + "/", headers={"api-key": self.key},
            timeout=httpx.Timeout(timeout, connect=20), transport=transport,
            limits=httpx.Limits(max_connections=workers, max_keepalive_connections=workers),
            follow_redirects=False)
        self.locks = {}
        self.stopped = None

    async def close(self):
        await self.client.aclose()

    async def call(self, request):
        identity = {"endpoint": BASE_URL + "/chat/completions", "request": request,
                    "expected_snapshot": MODELS[request["model"]]}
        key = digest(identity)
        path = self.cache / key[:2] / f"{key}.json"
        async with self.locks.setdefault(key, asyncio.Lock()), campaign_lock(path):
            if path.exists():
                old = json.loads(path.read_text())
                if old.get("identity") != identity:
                    raise ValueError(f"Cache identity mismatch: {path}")
                # A campaign resumes the first terminal outcome, including
                # failures. Retrying only failed completions across prefills
                # would silently turn the evaluation into best-of sampling.
                if isinstance(old.get("ok"), bool):
                    error = old.get("error", "")
                    if error.startswith(("Azure authorization failed", "Model snapshot changed")):
                        self.stopped = error
                    return old | {"cache_hit": True, "receipt": str(path)}
            if self.stopped:
                return {"ok": False, "error": self.stopped, "receipt": str(path)}
            attempts = []
            start = time.monotonic()
            async with self.semaphore:
                for attempt in range(5):
                    if self.stopped:
                        break
                    began = time.monotonic()
                    try:
                        response = await self.client.post("chat/completions", json=request)
                        elapsed = time.monotonic() - began
                        item = {"status": response.status_code, "elapsed_s": elapsed}
                        if response.status_code == 200:
                            body = response.json()
                            item["response"] = body
                            attempts.append(item)
                            actual = body.get("model")
                            choice = (body.get("choices") or [{}])[0]
                            content = choice.get("message", {}).get("content")
                            if actual != identity["expected_snapshot"]:
                                self.stopped = f"Model snapshot changed: expected {identity['expected_snapshot']}, got {actual}"
                                break
                            ok = choice.get("finish_reason") == "stop" and isinstance(content, str) and bool(content.strip())
                            result = {"identity": identity, "ok": ok, "text": content or "",
                                      "finish_reason": choice.get("finish_reason"), "model": actual,
                                      "usage": body.get("usage", {}), "attempts": attempts,
                                      "elapsed_s": time.monotonic() - start}
                            if not ok:
                                result["error"] = "Empty, refused or truncated completion"
                            save(path, result)
                            return result | {"cache_hit": False, "receipt": str(path)}
                        # Do not persist arbitrary provider errors: they may echo
                        # credentials or request headers. Status alone is enough.
                        attempts.append(item)
                        if response.status_code in (401, 403):
                            self.stopped = f"Azure authorization failed ({response.status_code})"
                            break
                        if response.status_code not in (408, 409, 429, 500, 502, 503, 504):
                            break
                        delay = min(45, 2 ** (attempt + 1))
                        retry_after = response.headers.get("retry-after")
                        if retry_after and retry_after.replace(".", "", 1).isdigit():
                            delay = min(45, max(delay, float(retry_after)))
                    except (httpx.TransportError, ValueError) as exc:
                        attempts.append({"error_type": type(exc).__name__,
                                         "elapsed_s": time.monotonic() - began})
                        delay = min(45, 2 ** (attempt + 1))
                    if attempt < 4:
                        await asyncio.sleep(delay + random.random())
            result = {"identity": identity, "ok": False, "attempts": attempts,
                      "elapsed_s": time.monotonic() - start,
                      "error": self.stopped or "Azure request failed; see attempt statuses"}
            save(path, result)
            return result | {"cache_hit": False, "receipt": str(path)}


async def run_jobs(jobs, out, *, cache, workers=16, context=None):
    ids = [job["id"] for job in jobs]
    if len(ids) != len(set(ids)):
        raise ValueError("Duplicate evaluation job IDs")
    out = Path(out)
    protocol = {"transport_sha256": file_sha(__file__), "models": MODELS,
                "jobs_sha256": digest(jobs), "job_ids": ids, "context": context or {}}
    freeze(out / "PROTOCOL.json", protocol)
    engine = AzureBatch(cache, workers=workers)
    results = []
    started = time.monotonic()

    async def one(job):
        reply = await engine.call(job["request"])
        result = {k: v for k, v in job.items() if k != "request"} | {"response": reply}
        save(out / "cases" / (digest(job["id"]) + ".json"), result)
        results.append(result)
        if len(results) % 50 == 0 or len(results) == len(jobs):
            print(json.dumps({"run": out.name, "completed": len(results), "total": len(jobs),
                              "successful": sum(r["response"]["ok"] for r in results),
                              "elapsed_s": round(time.monotonic() - started, 1)}), flush=True)

    try:
        await asyncio.gather(*(one(job) for job in jobs))
    finally:
        await engine.close()
    order = {key: i for i, key in enumerate(ids)}
    results.sort(key=lambda r: order[r["id"]])
    save(out / "RESULTS.json", results)
    return results


def wilson(correct, total):
    if not total:
        return None
    z = 1.959963984540054
    p = correct / total
    denom = 1 + z * z / total
    middle = (p + z * z / (2 * total)) / denom
    half = z * math.sqrt(p * (1 - p) / total + z * z / (4 * total * total)) / denom
    return [max(0, middle - half), min(1, middle + half)]


def stats(values):
    values = sorted(values)
    if not values:
        return {"n": 0}
    return {"n": len(values), "mean": statistics.mean(values), "median": statistics.median(values),
            "p95": values[min(len(values) - 1, math.ceil(len(values) * .95) - 1)], "max": max(values)}


def strict_yes_no(text):
    """Official yes/no output: malformed text is an error, not substring 'yes'."""
    value = text.strip().lower().rstrip(".")
    return value == "yes" if value in ("yes", "no") else None


def final_yes_no(text):
    """Mem0's final verdict, with malformed output distinguished from 'no'."""
    region = re.split(r"</judge_thinking>|</thinking>", text, flags=re.I)[-1].strip()
    for line in reversed(region.splitlines()):
        if line.strip().lower() in ("yes", "no"):
            return line.strip().lower() == "yes"
    tokens = re.findall(r"\b(yes|no)\b", region.lower())
    return tokens[-1] == "yes" if tokens else None
