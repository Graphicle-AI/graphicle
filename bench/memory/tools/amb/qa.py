#!/usr/bin/env python3
"""Answer accuracy for the packets of a tools/amb/recall.py run.

recall.py keeps every `context` request and packet beside each unit's store.
This script turns each saved packet into the documents graphicle_provider.py
returns to AMB and sends them through AMB's own answer and judge code, imported
at run time from --amb (nothing of AMB is copied): RAGMode.answer_from_context
with the dataset's RAG prompt, then GeminiJudge.score with the dataset's judge
prompt, as AMB's runner calls them. The one thing rebuilt here is RAGMode's
context layout, a `## Memory <n>` heading over each document.

The reader and the judge are AMB LLM providers (`gemini:<model>`,
`groq:<model>`, `openai:<model>`; OPENAI_BASE_URL reaches a vLLM server) or a
local MLX model (`mlx:<repo or path>`), which is given AMB's prompt with a
request for a JSON object holding the schema's fields in place of an
API-enforced schema, and decodes greedily. A local reader and judge are not
the ones AMB publishes with, so the accuracies compare the variants of one run
with each other; they are not leaderboard numbers.

    .venv/bin/python tools/amb/qa.py --run runs/amb-locomo --amb <amb checkout>/src \\
      --reader mlx:mlx-community/Qwen3.5-4B-MLX-4bit --judge mlx:mlx-community/Qwen3.5-9B-MLX-4bit \\
      --variants hyb-rrf@2k,hyb-rrf-fc@2k --paired hyb-rrf@2k,hyb-rrf-fc@2k

Questions go through in blocks of --block. AMB's code runs over a block once
to collect the prompts it sends, which a local model then decodes together,
--batch at a time and shortest first, and runs again with the replies, until
the block is answered and judged. Replies are kept in replies-reader.jsonl and
replies-judge.jsonl, so --resume reuses them and batches as an unbroken run
does, and a prompt asked twice is decoded once. With two local models, every
answer is decoded before the judge is loaded, so one model is in memory at a
time.

Results go to <run>/qa/<reader>/: one file per question, SUMMARY.json
(accuracy per variant, by AMB category and LoCoMo label, and exact McNemar
tests on paired questions) and PROTOCOL.json. `strict_accuracy` and
`paired_strict` also count as wrong an answer saying the memories do not hold
it when no gold answer says so, which AMB's lenient judge prompt lets local
judges pass. A case whose retrieval failed in the run is left out and counted;
a reader or judge error counts as a wrong answer and is reported, as are
replies with no parseable JSON object.
"""
import argparse
import datetime
import hashlib
import importlib.metadata
import json
import math
import os
import re
import sys
import time
import types
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import recall  # noqa: E402  (loads graphicle_provider.py as recall.amb)

amb, retrieval_recall = recall.amb, recall.retrieval_recall
CONTRACT = 'graphicle.amb-qa-protocol.v1'
ROUNDS = 4  # replays per block: the reader's reply, the judge's, and spare ones
# MLX keeps freed buffers for reuse; past this they return to the system, so
# batches of different prompt lengths do not grow the process without bound.
MLX_CACHE_BYTES = 1 << 30
THINKING = re.compile(r'<think>.*?</think>', re.S)
TRUE = {'true', 'yes', 'correct', '1'}
ABSTAIN = re.compile(
    r"\b(?:not (?:mentioned|stated|specified|provided|available|given|known|recorded|included|found)"
    r"|no (?:information|mention|details?|record|evidence|indication)"
    r"|(?:does|do|did)(?:n't| not) (?:say|state|mention|specify|contain|include|indicate|provide|know|have)"
    r"|(?:cannot|can't|can not) (?:be )?(?:answer|determine|tell|know|find)"
    r"|unanswerable|insufficient (?:information|context))\b", re.I)


def instruction(schema):
    """What stands in for an API-enforced schema with a local model: the
    schema's fields, explanations before verdicts."""
    fields = sorted(schema.properties.items(), key=lambda item: item[1].get('type', 'string') != 'string')
    lines = [f'- "{name}" ({spec.get("type", "string")})' + (f': {spec["description"]}' if spec.get('description')
                                                              else '') for name, spec in fields]
    return '\n\nReply with only a JSON object with these keys:\n' + '\n'.join(lines)


def salvage(text, name, spec):
    """The value of key `name` in a reply that is not a whole JSON object (cut
    short, or with a stray quote elsewhere), or None."""
    key = r'"' + re.escape(name) + r'"\s*:\s*'
    if spec.get('type') == 'boolean':
        found = re.search(key + r'"?(true|false)\b', text, re.I)
        return None if found is None else found.group(1).lower() == 'true'
    found = re.search(key + r'"((?:[^"\\]|\\.)*)"', text)
    if found is None:
        return None
    try:
        return json.loads(f'"{found.group(1)}"')
    except ValueError:
        return found.group(1)


def parse(text, schema):
    """The first JSON object in a model's reply holding the schema's required
    keys, coerced to the schema's types, and whether there was one. Failing
    that, keys are salvaged one by one; a missing string is empty, a missing
    verdict false. A reply that is no attempt at JSON is taken as the value of
    every string key, as a plain answer."""
    text = THINKING.sub('', text).strip()
    decoder = json.JSONDecoder()
    found = None
    for start in (i for i, ch in enumerate(text) if ch == '{'):
        try:
            value, _ = decoder.raw_decode(text, start)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and set(schema.required) <= set(value):
            found = value
            break
    plain = found is None and '{' not in text
    out = {}
    for name, spec in schema.properties.items():
        value = (found or {}).get(name) if found is not None else (None if plain else salvage(text, name, spec))
        if spec.get('type') == 'boolean':
            out[name] = value if isinstance(value, bool) else str(value).strip().lower() in TRUE
        else:
            out[name] = (text if plain else '') if value is None else str(value)
    return out, found is not None


class Pending(Exception):
    """A prompt with no reply yet. AMB's RAGMode and GeminiJudge let it
    through, which ends that question's attempt until its replies are in."""


class LocalModel:
    """An MLX model that replies to many prompts at once: a chat-templated
    prompt (thinking off) asking for a JSON object with the schema's fields,
    decoded greedily in batches of `batch`. A batch's composition can change a
    greedy reply slightly, so the batches are fixed by the prompts' order."""

    def __init__(self, path, batch=8, max_new_tokens=1024):
        self.path, self.batch, self.max_new_tokens = path, max(1, batch), max_new_tokens
        self.model = self.tokenizer = None
        self.calls = self.unparsed = self.prompt_tokens = self.generated_tokens = 0

    def load(self):
        """Loaded at the first prompt, so a run whose replies are all kept
        never loads the model."""
        if self.model is None:
            import mlx.core as mx
            import mlx_lm
            mx.set_cache_limit(MLX_CACHE_BYTES)
            self.model, self.tokenizer = mlx_lm.load(self.path)

    def release(self):
        if self.model is not None:
            import gc
            import mlx.core as mx
            self.model = self.tokenizer = None
            gc.collect()
            mx.clear_cache()

    @property
    def model_id(self):
        return f'mlx:{self.path}'

    def describe(self):
        info = {'model_id': self.model_id, 'mlx_lm': importlib.metadata.version('mlx-lm'), 'decoding': 'greedy',
                'max_new_tokens': self.max_new_tokens, 'batch': self.batch,
                'schema': 'requested in the prompt, parsed leniently'}
        if not Path(self.path).exists():
            try:
                from huggingface_hub import snapshot_download
                info['snapshot'] = Path(snapshot_download(self.path, local_files_only=True)).name
            except Exception:  # noqa: BLE001  a local path or an offline cache without refs
                pass
        return info

    def stats(self):
        return {'calls': self.calls, 'unparsed': self.unparsed, 'prompt_tokens': self.prompt_tokens,
                'generated_tokens': self.generated_tokens}

    def encode(self, prompt):
        if getattr(self.tokenizer, 'chat_template', None) is None:
            return self.tokenizer.encode(prompt)
        return self.tokenizer.apply_chat_template([{'role': 'user', 'content': prompt}],
                                                  add_generation_prompt=True, enable_thinking=False)

    def run(self, requests):
        """[(prompt, schema)] -> [(data, parsed, error)], batched shortest
        prompts first."""
        import mlx.core as mx
        from mlx_lm.generate import batch_generate
        from mlx_lm.sample_utils import make_sampler
        self.load()
        tokens = [self.encode(prompt + instruction(schema)) for prompt, schema in requests]
        order = sorted(range(len(requests)), key=lambda i: (len(tokens[i]), i))
        replies = [None] * len(requests)
        for start in range(0, len(order), self.batch):
            chunk = order[start:start + self.batch]
            try:
                texts = batch_generate(self.model, self.tokenizer, [tokens[i] for i in chunk],
                                       max_tokens=self.max_new_tokens, sampler=make_sampler(temp=0.0),
                                       completion_batch_size=len(chunk), prefill_batch_size=len(chunk)).texts
            except Exception as error:  # noqa: BLE001  each of the batch's questions gets it
                for i in chunk:
                    replies[i] = (None, False, f'{type(error).__name__}: {error}')
                continue
            finally:
                mx.clear_cache()
            for i, text in zip(chunk, texts):
                data, ok = parse(text, requests[i][1])
                replies[i] = (data, ok, None)
                self.calls += 1
                self.unparsed += not ok
                self.prompt_tokens += len(tokens[i])
                self.generated_tokens += len(self.tokenizer.encode(text))
        return replies


class RemoteModel:
    """An AMB LLM provider, called `workers` at a time."""

    def __init__(self, llm, workers=8):
        self.llm, self.workers = llm, max(1, workers)

    @property
    def model_id(self):
        return self.llm.model_id

    def describe(self):
        return {'model_id': self.llm.model_id, 'schema': "the provider's"}

    def release(self):
        pass

    def run(self, requests):
        def one(request):
            try:
                return self.llm.generate(*request), True, None
            except Exception as error:  # noqa: BLE001  that question gets it
                return None, False, f'{type(error).__name__}: {error}'
        with ThreadPoolExecutor(self.workers) as pool:
            return list(pool.map(one, requests))


class Replies:
    """The LLM AMB's RAGMode or GeminiJudge is handed: `generate(prompt,
    schema)` returns the reply to that prompt, or records the prompt and raises
    Pending. `flush` computes the recorded prompts' replies together and
    appends them to `path`, from which a resumed run reloads them. Identical
    prompts share one reply."""

    def __init__(self, engine, path):
        self.engine, self.path, self.replies, self.wanted, self.parsed = engine, path, {}, {}, None
        if path.exists():
            for line in path.read_text().splitlines():
                try:
                    row = json.loads(line)
                except ValueError:  # a line cut short when a run stopped
                    continue
                self.replies[row['prompt_sha256']] = (row['data'], row['parsed'], row.get('error'))

    @property
    def model_id(self):
        return self.engine.model_id

    @staticmethod
    def key(prompt):
        return hashlib.sha256(prompt.encode()).hexdigest()

    def generate(self, prompt, schema):
        reply = self.replies.get(self.key(prompt))
        if reply is None:
            self.wanted.setdefault(self.key(prompt), (prompt, schema))
            raise Pending(prompt)
        data, self.parsed, error = reply
        if error:
            raise RuntimeError(error)
        return json.loads(json.dumps(data))

    def flush(self):
        wanted, self.wanted = list(self.wanted.items()), {}
        if not wanted:
            return
        replies = self.engine.run([request for _, request in wanted])
        with self.path.open('a') as log:
            for (key, _), (data, parsed, error) in zip(wanted, replies):
                self.replies[key] = (data, parsed, error)
                log.write(json.dumps({'prompt_sha256': key, 'data': data, 'parsed': parsed, 'error': error}) + '\n')


def model(spec, batch, max_new_tokens, workers):
    """`mlx:<repo or path>` or an AMB provider, `<name>:<model>` (AMB's
    memory_bench.llm.REGISTRY)."""
    name, _, path = spec.partition(':')
    if not path:
        sys.exit(f'model {spec!r}: expected <provider>:<model>, e.g. mlx:mlx-community/Qwen3.5-4B-MLX-4bit')
    if name == 'mlx':
        return LocalModel(path, batch, max_new_tokens)
    from memory_bench.llm import REGISTRY
    if name not in REGISTRY:
        sys.exit(f'model {spec!r}: unknown provider; AMB has {sorted(REGISTRY)} and this script adds mlx')
    return RemoteModel(REGISTRY[name](path), workers)


BARE = []


def amb_module(name):
    """`name` from AMB. memory_bench/memory/__init__.py and modes/__init__.py
    import every memory provider and mode AMB has, with their clients; where
    those are not installed, the two packages are entered bare (their
    __init__ not run) so their modules load alone, as they are."""
    try:
        return importlib.import_module(name)
    except ImportError:
        import memory_bench
    for package in ('memory_bench.memory', 'memory_bench.modes'):
        if package not in sys.modules:
            bare = types.ModuleType(package)
            bare.__path__ = [str(Path(memory_bench.__file__).parent / package.rpartition('.')[2])]
            sys.modules[package] = bare
            BARE.append(package)
    return importlib.import_module(name)


def describe(llm):
    return llm.describe() if hasattr(llm, 'describe') else {'model_id': llm.model_id}


def abstains(text):
    return bool(ABSTAIN.search(str(text or '')))


def judged(case, row):
    return bool(row['correct'])


def strict(case, row):
    """The judge's verdict, except that an answer saying the memories do not
    hold it is wrong unless a gold answer says so too: a lenient judge's
    commonest mistake, the one measure a reader cannot win by abstaining."""
    return judged(case, row) and not (
        abstains(row.get('answer')) and not any(map(abstains, case.get('gold_answers') or [])))


def mcnemar(cases, a, b, score=judged):
    """Exact two-sided McNemar test on `score`, over questions both variants
    answered."""
    both = [(c, c['variants']) for c in cases if a in c['variants'] and b in c['variants']]
    right = [(score(c, v[a]), score(c, v[b])) for c, v in both]
    only_a = sum(1 for x, y in right if x and not y)
    only_b = sum(1 for x, y in right if y and not x)
    n = only_a + only_b
    tail = sum(math.comb(n, k) for k in range(min(only_a, only_b) + 1)) / 2 ** n if n else 1.0
    right_a, right_b = sum(x for x, _ in right), sum(y for _, y in right)
    return {'pair': [a, b], 'n': len(both), 'correct_a': right_a, 'correct_b': right_b,
            'only_a': only_a, 'only_b': only_b, 'mcnemar_p': round(min(1.0, 2 * tail), 4),
            'accuracy_diff': round((right_b - right_a) / len(both), 4) if both else 0.0}


def summarize(cases, variants, group):
    """Accuracy per variant and group, by the judge and by `strict`.
    `abstained` counts answers that say the memories do not hold the answer,
    and `abstained_judged_correct` those the judge passed although no gold
    answer says so."""
    table = {}
    for name in variants:
        rows = {}
        for case in cases:
            row = case['variants'].get(name)
            if row is not None:
                for kind in ('ALL', group(case)):
                    rows.setdefault(kind, []).append((case, row))
        table[name] = {}
        for kind, found in sorted(rows.items()):
            answers = [r for _, r in found]
            table[name][kind] = {
                'n': len(found), 'accuracy': round(sum(judged(c, r) for c, r in found) / len(found), 4),
                'strict_accuracy': round(sum(strict(c, r) for c, r in found) / len(found), 4),
                'errors': sum('error' in r for r in answers),
                'reader_unparsed': sum(r.get('reader_parsed') is False for r in answers),
                'judge_unparsed': sum(r.get('judge_parsed') is False for r in answers),
                'abstained': sum(abstains(r.get('answer')) for r in answers),
                'abstained_judged_correct': sum(judged(c, r) and not strict(c, r) for c, r in found),
                'context_chars': round(sum(r.get('context_chars', 0) for r in answers) / len(found))}
    return table


def case_name(qid):
    """recall.py's file name for a question's case."""
    return (qid if recall.SAFE.fullmatch(qid) else 'q-' + hashlib.sha256(qid.encode()).hexdigest()[:24]) + '.json'


def packet(folder, knobs, query):
    """The packet recall.py saved for this question and variant. Its request
    pinned retrieval_recall.RECIPE_OFF when the binary it ran defaulted options
    (recall.resolve), so both forms are looked up."""
    knobs = dict(knobs)
    server = knobs.pop('server_defaults', False)
    budget = knobs.pop('max_tokens')
    for form in ([knobs] if server else [{**retrieval_recall.RECIPE_OFF, **knobs}, knobs]):
        _, key = amb.context_request(form, query.query, budget, query.meta.get('query_timestamp'))
        path = folder / 'queries' / f'{key}.packet.json'
        if path.exists():
            return json.loads(path.read_text())
    raise FileNotFoundError(f'{query.id}: no saved packet in {folder / "queries"}')


def parse_args(argv):
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    p.add_argument('--run', type=Path, required=True, help="a tools/amb/recall.py run's --out")
    p.add_argument('--amb', type=Path, help="AMB's src directory (or set PYTHONPATH)")
    p.add_argument('--reader', required=True, help='mlx:<repo or path> or <AMB provider>:<model>')
    p.add_argument('--judge', help='as --reader (default: the reader)')
    p.add_argument('--variants', help="comma-separated variants of the run, e.g. hyb-rrf@2k (default: all)")
    p.add_argument('--paired', action='append', default=[], help='A,B: exact McNemar test (repeatable)')
    p.add_argument('--limit', type=int, help="the run's first N questions")
    p.add_argument('--batch', type=int, default=8, help='prompts an MLX model decodes together')
    p.add_argument('--block', type=int, default=64, help='answers whose prompts go to a model together')
    p.add_argument('--workers', type=int, default=8, help="concurrent calls to an AMB provider's model")
    p.add_argument('--max-new-tokens', type=int, default=1024, help='per MLX reply')
    p.add_argument('--out', type=Path, help='default: <run>/qa/<reader>')
    p.add_argument('--resume', action='store_true', help='continue an existing --out with the same inputs')
    return p.parse_args(argv)


def main(argv=None):
    a = parse_args(argv)
    if a.amb:
        sys.path.insert(0, str(a.amb.resolve()))
    run_protocol = a.run / 'PROTOCOL.json'
    if not run_protocol.is_file():
        sys.exit(f'{a.run} holds no PROTOCOL.json; pass the --out of a tools/amb/recall.py run')
    frozen = json.loads(run_protocol.read_text())['inputs']
    source = frozen['amb']
    variants = frozen['variants']
    chosen = a.variants.split(',') if a.variants else list(variants)
    unknown = sorted(set(chosen) - set(variants))
    pairs = [pair.split(',') for pair in a.paired]
    unknown += sorted({v for pair in pairs for v in pair} - set(chosen))
    if unknown or any(len(pair) != 2 for pair in pairs):
        sys.exit(f'variants not in the run or not chosen: {unknown}; pairs take two names' if unknown
                 else '--paired takes A,B')
    dataset, queries, documents, _, _ = recall.load(source['dataset'], source['split'], source.get('category'),
                                                    source.get('limit'), source.get('units'))
    if recall.fingerprint(queries, documents) != frozen['data_sha256']:
        sys.exit("AMB's data differs from the run's; point --amb and the data path where the run's did")
    if dataset.task_type != 'open':
        sys.exit(f'{source["dataset"]} is a {dataset.task_type!r} dataset; this script judges open answers')
    GeminiJudge = amb_module('memory_bench.judge').GeminiJudge
    RAGMode = amb_module('memory_bench.modes.rag').RAGMode

    by_id = {q.id: q for q in queries}
    order = [item['id'] for item in frozen['queries']][:a.limit]
    slug = re.sub(r'[^A-Za-z0-9_.-]+', '_', a.reader)
    out = a.out or a.run / 'qa' / slug
    inputs = {'run_protocol_sha256': retrieval_recall.digest(run_protocol),
              'qa_sha256': retrieval_recall.digest(Path(__file__)),
              'provider_sha256': retrieval_recall.digest(HERE / 'graphicle_provider.py'),
              'data_sha256': frozen['data_sha256'], 'amb': source, 'variants': {v: variants[v] for v in chosen},
              'pairs': pairs, 'reader': a.reader, 'judge': a.judge or a.reader,
              'max_new_tokens': a.max_new_tokens, 'batch': a.batch, 'block': a.block, 'questions': order}
    protocol_path = out / 'PROTOCOL.json'
    if protocol_path.exists():
        if not a.resume:
            sys.exit(f'{out} holds a run; pass --resume to continue it or choose another --out')
        if json.loads(protocol_path.read_text())['inputs'] != inputs:
            sys.exit(f'{out} was started with different inputs; choose another --out')
    reader_engine = model(a.reader, a.batch, a.max_new_tokens, a.workers)
    judge_engine = reader_engine if (a.judge or a.reader) == a.reader else model(
        a.judge, a.batch, a.max_new_tokens, a.workers)
    (out / 'cases').mkdir(parents=True, exist_ok=True)
    if not protocol_path.exists():
        retrieval_recall.save(protocol_path, {
            'contract': CONTRACT, 'inputs': inputs,
            'frozen_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(timespec='seconds'),
            'reader': describe(reader_engine), 'judge': describe(judge_engine),
            'context_layout': "RAGMode's: '## Memory <n>' over each document, blank-line separated",
            'batching': f'answers go in blocks of {a.block}, the prompts of a block to a model together, in batches '
                        f'of {a.batch} shortest first; identical prompts share one reply; replies are kept in '
                        'replies-<role>.jsonl, so a resumed run batches as an unbroken one does; with two '
                        'models, every answer comes before the first verdict',
            'amb_bare_packages': BARE,
            'leaderboard_claim': False,
            'note': "the reader and judge are not AMB's published ones; compare variants of this run only"})
    reader = Replies(reader_engine, out / 'replies-reader.jsonl')
    judge_llm = Replies(judge_engine, out / 'replies-judge.jsonl')

    rag, judge = RAGMode(llm=reader), GeminiJudge(llm=judge_llm)
    split, category = source['split'], source.get('category')
    task_type = dataset.task_type

    def prompt(query, context, meta=None):
        return dataset.build_rag_prompt(query, context, task_type, split, category, meta)

    provider = amb.GraphicleProvider()
    provider.root = amb.store_root(a.run / 'stores')
    run_cases, skipped, answered = {}, [], {}
    for qid in order:
        stored = a.run / 'cases' / case_name(qid)
        if not stored.exists() or not json.loads(stored.read_text()).get('complete'):
            skipped.append(qid)
            continue
        run_cases[qid] = json.loads(stored.read_text())
        path = out / 'cases' / case_name(qid)
        if path.exists():
            answered[qid] = json.loads(path.read_text())
    items = [(qid, name) for qid in order if qid in run_cases for name in chosen]

    def attempt(item):
        """The item's row, or None while a reply it needs is pending."""
        qid, name = item
        q = by_id[qid]
        row = {}
        try:
            folder = provider._unit(q.user_id)[0]
            docs = amb.documents(packet(folder, variants[name], q), provider._index(folder), q.user_id)
            context = '\n\n'.join(f'## Memory {i + 1}\n{doc.content}' for i, doc in enumerate(docs))
            meta = {**q.meta, '_prompt_fn': prompt}
            screen = getattr(dataset, 'retrieval_filter', None)
            if callable(screen) and screen(q) is not None:
                meta['retrieval_filter'] = screen(q)
            row.update(documents=len(docs), context_chars=len(context))
            reply = rag.answer_from_context(q.query, context, task_type, meta)
            row.update(answer=reply.answer, reasoning=reply.reasoning, reader_parsed=reader.parsed)
            if hasattr(dataset, 'get_judge_prompt_fn'):
                judge_prompt = dataset.get_judge_prompt_fn(q.meta.get('question_type') or q.meta.get('category'),
                                                           meta=q.meta)
            else:
                judge_prompt = dataset.build_judge_prompt
            verdict = judge.score(q.query, reply.answer, q.gold_answers, judge_prompt)
            row.update(correct=bool(verdict.correct), judge_reason=verdict.reason, judge_parsed=judge_llm.parsed)
        except Pending:
            return None
        except Exception as error:  # noqa: BLE001  a failed question counts as wrong
            row.update(correct=False, error=f'{type(error).__name__}: {error}')
        return row

    def answer_blocks(verdicts):
        """Every block's answers; with `verdicts`, their verdicts too, and the
        cases written. Without, the judge's prompts are dropped, so the reader
        is the one model at work."""
        started, finished = time.monotonic(), 0
        for start in range(0, len(items), max(1, a.block)):
            block = items[start:start + a.block]
            if all(name in answered.get(qid, {}).get('variants', {}) for qid, name in block):
                continue
            rows = {}
            for _ in range(ROUNDS):
                for item in block:
                    if item not in rows:
                        row = attempt(item)
                        if row is not None:
                            rows[item] = row
                if len(rows) == len(block) or not (verdicts or reader.wanted):
                    break
                reader.flush()
                if verdicts:
                    judge_llm.flush()
            judge_llm.wanted.clear()
            finished += len(block)
            rate = finished / max(1e-9, time.monotonic() - started)
            print(f'{start + len(block)}/{len(items)} {"judged" if verdicts else "answered"} ({rate * 60:.1f}/min)',
                  file=sys.stderr, flush=True)
            if not verdicts:
                continue
            for item in block:
                rows.setdefault(item, {'correct': False, 'error': f'a reply was still pending after {ROUNDS} rounds'})
            for qid in dict.fromkeys(qid for qid, _ in block):
                case = answered.setdefault(qid, {
                    'question_id': qid, 'type': run_cases[qid].get('type'), 'unit': run_cases[qid].get('unit'),
                    'locomo_category': run_cases[qid].get('locomo_category'),
                    'gold_answers': by_id[qid].gold_answers, 'variants': {}})
                case['variants'].update((name, rows[(q, name)]) for q, name in block if q == qid)
                amb.replace_text(out / 'cases' / case_name(qid), json.dumps(case, indent=2))

    if judge_engine is not reader_engine:
        # Two models need not fit in memory at once: all answers, then all verdicts.
        answer_blocks(verdicts=False)
        reader_engine.release()
    answer_blocks(verdicts=True)

    cases = [json.loads((out / 'cases' / case_name(qid)).read_text()) for qid in order
             if (out / 'cases' / case_name(qid)).exists()]
    labels = recall.LOCOMO_LABELS
    summary = {'questions': len(order), 'judged': len(cases), 'retrieval_failed_in_run': skipped,
               'by_variant': summarize(cases, chosen, lambda c: c.get('type') or 'all'),
               'paired': [mcnemar(cases, x, y) for x, y in pairs],
               'paired_strict': [mcnemar(cases, x, y, strict) for x, y in pairs],
               'reader': describe(reader_engine), 'judge': describe(judge_engine)}
    if any(c.get('locomo_category') in labels for c in cases):
        summary['by_variant_locomo_labels'] = summarize(
            cases, chosen, lambda c: labels.get(c.get('locomo_category'), 'unlabelled'))
    for role, llm in (('reader_stats', reader_engine), ('judge_stats', judge_engine)):
        if hasattr(llm, 'stats'):
            summary[role] = llm.stats()
    retrieval_recall.save(out / 'SUMMARY.json', summary)
    for name in chosen:
        row = summary['by_variant'][name].get('ALL', {})
        print(f"{name}: accuracy {row.get('accuracy')} (strict {row.get('strict_accuracy')}) over {row.get('n')}",
              file=sys.stderr)
    for kind in ('paired', 'paired_strict'):
        for result in summary[kind]:
            print(f"{kind}: {result['pair'][0]} vs {result['pair'][1]}: {result['correct_a']} vs "
                  f"{result['correct_b']} of {result['n']}, McNemar p={result['mcnemar_p']}", file=sys.stderr)


if __name__ == '__main__':
    if os.environ.get('PYTHONHASHSEED') is None:
        os.environ['PYTHONHASHSEED'] = '0'
    main()
