#!/usr/bin/env python3
"""mock_meta.py HOST PORT: a stand-in for the Meta Model API's Responses endpoint, for the demo's tests and the flow.

POST /v1/responses answers as the real models' formats do, with made-up content and no model:
- `sam-*`: one segmentation record, a 160x28 box around the prompt's point;
- the reading call of `graphicle pixel` (instructions "You read one element"): the strings in MOCK_STRINGS
  (separated by `|`, default "Open Folder...") and the labels in MOCK_NEARBY (default none);
- computer use (the tool {"type":"computer"}, `graphicle see`): the scripted run of MOCK_COMPUTER_SCRIPT whose
  "match" text the task holds (see below), with its replay checked;
- `fix`'s plan ("You plan a change") and edits ("You fix an issue"): MOCK_FIX_SCRIPT's "plan" and "rounds" (round N
  gets the Nth, the last repeats); without the file, a plan of the pack's first file and no edits;
- another structured call (`text.format`): a ranking of the first file the pack shows, and an explanation;
- anything else (`ask --pack`): a short answer that cites the first file the pack shows.
Usage counts are a quarter of the bytes sent (text bytes and 1,500 tokens an image for computer use, with the
replayed prefix as cached). Nothing is logged.

MOCK_COMPUTER_SCRIPT is a JSON file: {"runs": [{"match": TEXT, "steps": [STEP, ...]}, ...]}. A STEP is
{"actions": [...], "safety": [{"id","code","message"}, ...]} (a computer_call), or {"answer": {...}} (the final
message, as JSON). The step of a request is the number of computer_call_output items it holds. When the request
asks for include ["reasoning.encrypted_content"], each call comes after a reasoning item with encrypted content.
The mock refuses a request with HTTP 400 when it breaks the replay that Meta's computer-use page describes: a
reasoning item or call not sent back as returned, an output that does not answer its call with a PNG or JPEG data
URL, or a pending safety check not acknowledged.
"""
import json
import os
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

IMAGE_TOKENS = 1500


class Refused(Exception):
    pass


def text_of(value):
    if isinstance(value, str):
        return value
    parts = []
    for message in value or []:
        for part in message.get("content", []) if isinstance(message, dict) else []:
            if isinstance(part, dict) and part.get("type") == "input_text":
                parts.append(part.get("text", ""))
    return "\n".join(parts)


def load(variable):
    path = os.environ.get(variable)
    if not path:
        return None
    with open(path) as f:
        return json.load(f)


def tokens(value):
    """Text bytes over four, and IMAGE_TOKENS for each image data URL."""
    if isinstance(value, str):
        return IMAGE_TOKENS if value.startswith("data:image/") else len(value) // 4
    if isinstance(value, list):
        return sum(tokens(v) for v in value)
    if isinstance(value, dict):
        return sum(len(k) // 4 + tokens(v) for k, v in value.items())
    return 1


def computer_call(step, spec):
    return {"type": "computer_call", "id": f"cc_mock_{step}", "call_id": f"call_mock_{step}",
            "actions": spec.get("actions", []), "pending_safety_checks": spec.get("safety", []),
            "status": "completed", "meta_safety_replay_receipt": f"mock-receipt-{step}"}


def reasoning(step):
    return {"type": "reasoning", "id": f"rs_mock_{step}",
            "summary": [{"type": "summary_text", "text": "A scripted step of the mock."}],
            "encrypted_content": f"mock-encrypted-{step}"}


def computer(request):
    """One step of a scripted computer-use run: the output items and their usage."""
    script = load("MOCK_COMPUTER_SCRIPT") or {"runs": []}
    items = request.get("input") or []
    task = text_of(items[:1])
    run = next((r for r in script.get("runs", []) if r.get("match", "") in task), None)
    if run is None:
        raise Refused("the mock has no scripted run for this task")
    steps = run["steps"]
    replayed = items[1:]
    # With encrypted reasoning asked for, each step comes back as its reasoning item, its call and its output.
    thinking = "reasoning.encrypted_content" in (request.get("include") or [])
    width = 3 if thinking else 2
    for index in range(0, len(replayed), width):
        step = index // width
        group = replayed[index:index + width]
        if thinking:
            if group[0] != reasoning(step):
                raise Refused(f"the reasoning of step {step} was not replayed unchanged")
            group = group[1:]
        expected = computer_call(step, steps[min(step, len(steps) - 1)])
        if not group or group[0] != expected:
            raise Refused(f"the computer_call of step {step} was not replayed unchanged")
        output = group[1] if len(group) > 1 else {}
        url = (output.get("output") or {}).get("image_url") or ""
        if output.get("type") != "computer_call_output" or output.get("call_id") != expected["call_id"] \
                or not re.match(r"data:image/(png|jpeg);base64,", url):
            raise Refused(f"the output of step {step} does not answer its call with a screenshot")
        if output.get("acknowledged_safety_checks") != expected["pending_safety_checks"]:
            raise Refused(f"the pending safety checks of step {step} were not acknowledged")
    step = len(replayed) // width
    spec = steps[min(step, len(steps) - 1)]
    if "answer" in spec:
        text = json.dumps(spec["answer"])
        output = [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}]
        output_tokens = 40 + len(text) // 4
    else:
        call = computer_call(step, spec)
        output = [reasoning(step), call] if thinking else [call]
        output_tokens = 40 + 15 * len(call["actions"])
    instructions = tokens(request.get("instructions") or "")
    total = instructions + tokens(items)
    cached = 0 if step == 0 else min(total, instructions + tokens(items[:-1]))
    return output, {"input_tokens": total, "output_tokens": output_tokens,
                  "input_tokens_details": {"cached_tokens": cached},
                  "output_tokens_details": {"reasoning_tokens": 0}}


def fix_reply(instructions, prompt, first):
    script = load("MOCK_FIX_SCRIPT") or {}
    if instructions.startswith("You plan a change"):
        if "plan" in script:
            return json.dumps(script["plan"])
        edits = [{"path": first.group(1), "start_line": int(first.group(2)), "end_line": int(first.group(2)),
                  "reason": "The mock picks the first file."}] if first else []
        return json.dumps({"summary": "A mock plan.", "steps": ["Change the first file"], "edits": edits,
                           "new_files": [], "tests": [], "risks": []})
    rounds = script.get("rounds") or [{"explanation": "The mock has no edits.", "edits": [], "new_files": [],
                                       "tests": []}]
    found = re.findall(r"Round (\d+): return the edits\.", prompt)
    number = int(found[-1]) if found else 1
    return json.dumps(rounds[min(number, len(rounds)) - 1])


def reply(request, length):
    """The output items, model and usage of a reply to a request of `length` bytes."""
    model = request.get("model", "")
    if any(isinstance(t, dict) and t.get("type") == "computer" for t in request.get("tools") or []):
        output, usage = computer(request)
        return model, output, usage
    prompt = text_of(request.get("input"))
    instructions = request.get("instructions") or ""
    first = re.search(r"^### (\S+) lines (\d+)-(\d+)", prompt, re.M)
    cite = f"{first.group(1)}:{first.group(2)}" if first else None
    if model.startswith("sam-"):
        m = re.search(r"point;x=(\d+);y=(\d+);w=(\d+);h=(\d+)", prompt)
        text = ""
        if m:
            x, y, w, h = map(int, m.groups())
            x1, y1, x2, y2 = max(0, x - 80), max(0, y - 14), min(w - 1, x + 80), min(h - 1, y + 14)
            text = f"<0f>1<|box;x1={x1};y1={y1};x2={x2};y2={y2};w={w};h={h};c=0.9|>"
    elif instructions.startswith("You read one element"):
        strings = os.environ.get("MOCK_STRINGS", "Open Folder...").split("|")
        nearby = [t for t in os.environ.get("MOCK_NEARBY", "").split("|") if t]
        text = json.dumps({"strings": strings, "element_type": "button", "icon": "",
                           "nearby_labels": nearby, "names": []})
    elif instructions.startswith(("You plan a change", "You fix an issue")):
        text = fix_reply(instructions, prompt, first)
    elif request.get("text", {}).get("format"):
        ranking = [{"path": first.group(1), "line": int(first.group(2)), "reason": "The mock picks the first file."}] if first else []
        text = json.dumps({"ranking": ranking, "explanation": f"A mock explanation citing {cite}."})
    else:
        text = "This answer comes from a mock model, not Muse Spark."
        if cite:
            text += f" The pack's first file is `{cite}`."
    item = {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}
    usage = None
    if not model.startswith("sam-"):
        usage = {"input_tokens": length // 4, "output_tokens": max(1, len(text) // 4),
                 "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}}
    return model, [item], usage


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def send(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) or b"{}"
        if self.path.rstrip("/").endswith("/responses/input_tokens"):
            self.send(200, {"input_tokens": length // 4})
            return
        try:
            model, output, usage = reply(json.loads(raw), length)
        except Refused as e:
            self.send(400, {"error": {"message": str(e), "type": "invalid_request_error"}})
            return
        body = {"id": "resp_mock", "object": "response", "status": "completed", "model": model, "output": output}
        if usage:
            body["usage"] = usage
        self.send(200, body)


if __name__ == "__main__":
    ThreadingHTTPServer((sys.argv[1], int(sys.argv[2])), Handler).serve_forever()
