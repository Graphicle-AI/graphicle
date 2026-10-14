#!/usr/bin/env python3
"""Precompute model-written query variants for recall evals.

Recall runs call `context`, which never calls a model, so model-written
variants are measured by writing them ahead of time and passing them to
`context` as `query_variants` (retrieval_recall.py --query-variants). Basic
answers do not write them themselves (`query_expansion` is not supported
by Graphicle). This tool sends that
feature's instructions (query_expansion_prompt.txt beside this tool, a
byte-identical copy, so the recorded prompt_sha256 is unchanged), input JSON
and schema over the chat protocol to any OpenAI-compatible server, such as a
local llama-server, and keeps the variants exactly as `expansion_variants`
did. Decoding is greedy unless --temperature is set, and a failed or invalid
reply is recorded with no variants, as the service answered without
expansion when it failed.
"""
import argparse
import datetime
import hashlib
import json
import sys
import threading
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

PROMPT = Path(__file__).resolve().parent / 'query_expansion_prompt.txt'
# The expansion's weights and keyword count, and the core's
# MAX_QUERY_VARIANT_BYTES.
WEIGHTS = {'rewrite': 1.3, 'keywords': 1.0, 'hypothetical_answer': 0.7}
KEYWORDS = 3
MAX_BYTES = 4000
# workflows::object as model::strict_schema leaves it, in insertion order.
SCHEMA = {'type': 'object', 'additionalProperties': False,
          'required': ['rewrite', 'keywords', 'hypothetical_answer'],
          'properties': {'rewrite': {'type': 'string'},
                         'keywords': {'type': 'array', 'items': {'type': 'string'}},
                         'hypothetical_answer': {'type': 'string'}}}


def variants(reply):
    out = []

    def push(text, weight, kind):
        text = text.strip().encode()[:MAX_BYTES].decode('utf-8', 'ignore')
        if text:
            out.append({'text': text, 'weight': weight, 'kind': kind})

    if isinstance(reply.get('rewrite'), str):
        push(reply['rewrite'], WEIGHTS['rewrite'], 'query')
    keywords = reply.get('keywords') if isinstance(reply.get('keywords'), list) else []
    for text in [k for k in keywords if isinstance(k, str) and k.strip()][:KEYWORDS]:
        push(text, WEIGHTS['keywords'], 'keyword')
    if isinstance(reply.get('hypothetical_answer'), str):
        push(reply['hypothetical_answer'], WEIGHTS['hypothetical_answer'], 'passage')
    return out


def request(base_url, model, instructions, question, question_time, max_tokens, temperature, timeout):
    # serde_json's compact output with preserve_order: the user message a job sends.
    user = json.dumps({'question': question, 'question_time': question_time}, separators=(',', ':'),
                      ensure_ascii=False)
    body = {'model': model, 'messages': [{'role': 'system', 'content': instructions},
                                         {'role': 'user', 'content': user}],
            # The service sends max_completion_tokens; llama-server reads max_tokens.
            'max_completion_tokens': max_tokens, 'max_tokens': max_tokens,
            'response_format': {'type': 'json_schema',
                                'json_schema': {'name': 'graphicle_result', 'strict': True, 'schema': SCHEMA}}}
    if temperature is not None:
        body['temperature'] = temperature
    call = urllib.request.Request(base_url.rstrip('/') + '/chat/completions', data=json.dumps(body).encode(),
                                  headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(call, timeout=timeout) as response:
        reply = json.load(response)
    choice = reply['choices'][0]
    if choice.get('finish_reason') not in (None, 'stop'):
        raise ValueError(f"finish_reason {choice.get('finish_reason')}")
    parsed = json.loads(choice['message']['content'])
    if not isinstance(parsed, dict) or set(parsed) != set(SCHEMA['properties']):
        raise ValueError('reply does not match the schema')
    return parsed, reply.get('usage') or {}


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('--dataset', type=Path, action='append', required=True,
                   help='LongMemEval JSON; repeat for S and M, which share their questions')
    p.add_argument('--out', type=Path, required=True, help='variants file; an existing one is resumed')
    p.add_argument('--base-url', default='http://127.0.0.1:18080/v1')
    p.add_argument('--model', default='local')
    p.add_argument('--max-output-tokens', type=int, default=512)
    p.add_argument('--temperature', type=float, default=0.0)
    p.add_argument('--concurrency', type=int, default=8)
    p.add_argument('--timeout', type=float, default=600.0)
    p.add_argument('--limit', type=int, default=0, help='first N questions only (smoke tests)')
    a = p.parse_args()
    if a.concurrency < 1 or a.max_output_tokens < 64:
        p.error('--concurrency must be positive and --max-output-tokens at least 64')
    questions = {}
    for path in a.dataset:
        for row in json.loads(path.read_text()):
            item = (row['question'], row['question_date'])
            if questions.setdefault(row['question_id'], item) != item:
                p.error(f"{row['question_id']} differs between datasets")
    ids = sorted(questions)[:a.limit] if a.limit else sorted(questions)
    instructions = PROMPT.read_text()
    settings = {'prompt_sha256': hashlib.sha256(instructions.encode()).hexdigest(), 'schema': SCHEMA,
                'weights': WEIGHTS, 'keywords': KEYWORDS, 'model': a.model, 'base_url': a.base_url,
                'max_output_tokens': a.max_output_tokens, 'temperature': a.temperature}
    if a.out.exists():
        saved = json.loads(a.out.read_text())
        if saved.get('settings') != settings:
            p.error('settings differ from the existing file; write a new one')
    else:
        saved = {'contract': 'graphicle.query-expansion.v1', 'settings': settings, 'items': {}}
    saved['started_utc'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    items = saved['items']
    lock = threading.Lock()
    todo = [qid for qid in ids if 'variants' not in items.get(qid, {})]

    def write():
        temporary = a.out.with_suffix('.tmp')
        temporary.write_text(json.dumps(saved, indent=1, ensure_ascii=False) + '\n')
        temporary.replace(a.out)

    def expand(qid):
        question, question_time = questions[qid]
        started = time.monotonic()
        for attempt in range(3):
            try:
                reply, usage = request(a.base_url, a.model, instructions, question, question_time,
                                       a.max_output_tokens, a.temperature, a.timeout)
                item = {'reply': reply, 'variants': variants(reply), 'usage': usage}
            except OSError as error:
                # Unreachable server or HTTP error: retried, then left without
                # variants so that a resume retries it again.
                item = {'transport_error': f'{type(error).__name__}: {error}'[:600]}
                if attempt < 2:
                    time.sleep(2 ** attempt)
                continue
            except Exception as error:
                item = {'error': f'{type(error).__name__}: {error}'[:600], 'variants': []}
            break
        item['seconds'] = round(time.monotonic() - started, 2)
        with lock:
            items[qid] = item
            write()
        print(json.dumps({'question_id': qid, 'variants': len(item.get('variants', [])),
                          'error': item.get('error') or item.get('transport_error')}), flush=True)

    with ThreadPoolExecutor(max_workers=a.concurrency) as pool:
        list(pool.map(expand, todo))
    invalid = sum(bool(items[qid].get('error')) for qid in ids)
    unreached = sum('variants' not in items[qid] for qid in ids)
    saved['finished_utc'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    write()
    print(json.dumps({'questions': len(ids), 'expanded': len(ids) - invalid - unreached, 'invalid': invalid,
                      'unreached': unreached, 'out': str(a.out)}))
    return 1 if unreached else 0


if __name__ == '__main__':
    sys.exit(main())
