"""Graphicle memory provider for the Agent Memory Benchmark (AMB).

AMB (github.com/vectorize-io/agent-memory-benchmark) runs the public
leaderboard at agentmemorybenchmark.ai. This script registers a `graphicle`
provider in an unmodified AMB install and then runs AMB's own command line, so
the dataset loaders, answer prompts, answer and judge models, and scoring are
all AMB's. Nothing from AMB is copied here: its repository had no license at
the pinned commit, so it is imported at run time only.

Install AMB and the light dependencies of its command line. The heavy packages
of other providers (qdrant_client, sentence_transformers, mem0) are stubbed
when missing, so only those providers become unusable:

    uv venv amb-venv --python 3.12
    uv pip install --python amb-venv --no-deps \\
        'git+https://github.com/vectorize-io/agent-memory-benchmark@f618ed7b1f0eb9cad7b42e876f91a42f0eadb150'
    uv pip install --python amb-venv typer rich python-dotenv rank-bm25 \\
        google-genai openai groq httpx tiktoken

Run LongMemEval-S with the leaderboard's reader and judge as published in
October 2026 (GEMINI_API_KEY comes from a private env file, never the command
line):

    export GRAPHICLE_BIN=/abs/graphicle GRAPHICLE_CONFIG=/abs/models.json
    export OMB_ANSWER_LLM=gemini OMB_ANSWER_MODEL=gemini-3.1-pro-preview
    amb-venv/bin/python tools/amb/graphicle_provider.py run \\
        --dataset longmemeval --split s --memory graphicle --mode rag

AMB judges with gemini-2.5-flash-lite unless OMB_JUDGE_LLM and OMB_JUDGE_MODEL
say otherwise. Any OpenAI-compatible server can serve either role through
OMB_*_LLM=openai and OPENAI_BASE_URL.

Settings (environment):
- GRAPHICLE_BIN: the graphicle binary (required).
- GRAPHICLE_CONFIG: models.json defining the `embedding` profile the default
  recipe uses (required), and `reranker` for knobs that name it.
- GRAPHICLE_AMB_KNOBS: JSON object of `context` options that replaces RECIPE.
- GRAPHICLE_AMB_MAX_TOKENS: evidence budget per question, default 8000.
- GRAPHICLE_AMB_CONCURRENCY: isolation units run in parallel, default 8.
- GRAPHICLE_EMBEDDING_CACHE and ORT_DYLIB_PATH reach the binary unchanged.

Ingest writes one source file per AMB document under
<store>/graphicle/<unit>/sessions, then syncs and embeds it. LongMemEval
sessions are written exactly as tools/qualify_longmemeval.py writes them and
named as tools/retrieval_recall.py --stable-names names them, so the two share
embedding-cache entries. LoCoMo and LifeBench sessions, which AMB passes as
JSON turn lists, are written as `speaker: text` turns, a shared image as its
caption; their turn ids, image URLs and image search queries are not written.
Document ids and AMB's `context` hints are never written: LongMemEval ids such
as `answer_...` would leak support labels.

Retrieve makes one `context` call. Each evidence excerpt becomes one AMB
document, in rank order, headed by its conversation date. The raw response is
None, so AMB's answer prompt shows these documents rather than a JSON dump.
Each request and its packet are kept beside the unit's store as receipts.
"""
from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import types
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

AMB_COMMIT = 'f618ed7b1f0eb9cad7b42e876f91a42f0eadb150'
# The `hyb-w05` recipe of tools/retrieval_recall.py with every option pinned:
# Porter stems, the auto speaker arm and the dense arm at its default weight
# 0.5, packed into 24 excerpts, with no cross-encoder. Every AMB dataset gives
# each conversation (LoCoMo) or user history (LongMemEval) its own store, the
# single-conversation case where the cross-encoder did not raise held-out
# recall. The earlier default, `hyb-rrf` (weight 2, 60 candidates reranked by
# the cross-encoder in RRF mode), is GRAPHICLE_AMB_KNOBS with those options.
RECIPE = {'limit': 24, 'stem': True, 'speaker': 'auto', 'pack_focus': False, 'origin_quota': False,
          'local_embedding_profile': 'embedding', 'arm_weights': {'semantic': 0.5}}
HEADER = 'Conversation date: '
DATE_FORMAT = '%Y/%m/%d (%a) %H:%M'
EMBED_CALLS = 32
EMBED_ROUNDS = 128
# Imported at module level by AMB's provider registry.
HEAVY = {'qdrant_client': ('QdrantClient', 'models'), 'sentence_transformers': ('SentenceTransformer',),
         'mem0': ('Memory', 'MemoryClient')}


class _Missing:
    def __init__(self, name):
        self._name = name

    def __getattr__(self, attr):
        return _Missing(f'{self._name}.{attr}')

    def __call__(self, *args, **kwargs):
        raise ImportError(f'{self._name} is not installed (stubbed by the Graphicle AMB runner)')


def stub_missing_modules():
    for name, attrs in HEAVY.items():
        if name not in sys.modules and importlib.util.find_spec(name) is None:
            module = types.ModuleType(name)
            for attr in attrs:
                setattr(module, attr, _Missing(f'{name}.{attr}'))
            sys.modules[name] = module


try:
    if importlib.util.find_spec('memory_bench') is None:
        raise ImportError('AMB is not installed')
    stub_missing_modules()
    from memory_bench.memory.base import MemoryProvider
    from memory_bench.models import Document
except ImportError:
    # Tests and tools/amb/recall.py, which load AMB's datasets without its
    # providers' dependencies: the fields this module uses. main() imports
    # AMB's providers again and reports what is missing.
    class MemoryProvider:
        pass

    @dataclass
    class Document:
        id: str
        content: str
        user_id: str | None = None
        messages: list | None = None
        timestamp: str | None = None
        context: str | None = None
        source_ids: list | None = None
        tags: list | None = None


def lme_date(value):
    """AMB's ISO-8601 time in LongMemEval's form, '2023/05/20 (Sat) 02:21'."""
    if not value:
        return None
    try:
        return datetime.fromisoformat(str(value).replace('Z', '+00:00')).strftime(DATE_FORMAT)
    except ValueError:
        return str(value)


def turn_text(turn):
    """What a turn says: its `content` (LongMemEval, AMB messages) or `text`
    (LoCoMo, LifeBench), followed by the caption of an image it shares."""
    text = turn.get('content')
    if not isinstance(text, str):
        text = turn.get('text')
    if not isinstance(text, str):
        return None
    caption = turn.get('blip_caption')
    if isinstance(caption, str) and caption.strip():
        shared = f'[shared an image: {caption.strip()}]'
        text = f'{text} {shared}' if text else shared
    return text


def turns_of(doc):
    turns = doc.messages
    if turns is None:
        try:
            turns = json.loads(doc.content)
        except (TypeError, ValueError):
            return None
    if isinstance(turns, list) and turns and all(isinstance(t, dict) and turn_text(t) is not None for t in turns):
        return turns
    return None


def render_spans(doc):
    """render() and the character span of each turn's text, keyed by the turn's
    `dia_id`: the evidence ids of LoCoMo-format datasets. Turn ids, image URLs
    and image search queries are never written."""
    date = lme_date(doc.timestamp)
    text = f'{HEADER}{date}\n\n' if date else ''
    turns = turns_of(doc)
    if turns is None:
        return text + doc.content.rstrip('\n') + '\n', {}
    spans = {}
    for turn in turns:
        speaker = turn.get('role') or turn.get('speaker') or 'unknown'
        said = turn_text(turn)
        start = len(text) + len(speaker) + 2
        text += f'{speaker}: {said}\n\n'
        if turn.get('dia_id') is not None:
            spans.setdefault(str(turn['dia_id']), (start, start + len(said)))
    return text, spans


def render(doc):
    """Source text of one AMB document: a date header, then `speaker: text`
    turns when the document is a list of messages, else its text."""
    return render_spans(doc)[0]


def file_name(doc, user_id, taken):
    """`session-<sha256(session id)[:16]>.txt`, with `-2`, `-3`... for repeats."""
    session = doc.id.removeprefix(f'{user_id}_') if user_id else doc.id
    base = 'session-' + hashlib.sha256(session.encode()).hexdigest()[:16]
    name, n = base + '.txt', 1
    while name in taken:
        n += 1
        name = f'{base}-{n}.txt'
    return name


def documents(packet, index, user_id):
    """AMB documents from a `context` packet: one per evidence excerpt, in rank
    order, each headed by its conversation date."""
    if packet.get('memories'):
        raise RuntimeError('the packet holds reviewed memories, which this provider neither writes nor renders')
    out = []
    for item in packet.get('evidence') or []:
        name = str(item.get('id', '')).removeprefix('file:')
        source = index.get(name, {})
        text = item.get('text') or ''
        if text.startswith(HEADER):
            text = text.partition('\n')[2]
        date = lme_date(source.get('timestamp'))
        head = f'{HEADER}{date}\n' if date else ''
        doc_id = source.get('id', name)
        out.append(Document(id=doc_id, content=head + text.strip('\n'), user_id=user_id,
                            timestamp=source.get('timestamp'), source_ids=[doc_id]))
    return out


def context_request(knobs, query, budget, query_timestamp=None):
    """The `context` arguments for one question, and the key that names their
    receipts (`queries/<key>.json` and `queries/<key>.packet.json`)."""
    request = {**knobs, 'question': query, 'max_tokens': budget, 'max_chars': budget * 4}
    when = lme_date(query_timestamp)
    if when:
        request['question_time'] = when
    return request, hashlib.sha256(json.dumps(request, sort_keys=True).encode()).hexdigest()[:16]


def replace_text(path, text):
    """Write `text` to `path` by rename, so a concurrent identical request
    never reads a half-written receipt."""
    part = path.with_name(f'.{path.name}.{os.getpid()}.{threading.get_ident()}')
    part.write_text(text)
    os.replace(part, path)


def store_root(store_dir):
    """The provider's directory under `store_dir`."""
    return Path(store_dir) / 'graphicle'


class GraphicleProvider(MemoryProvider):
    name = 'graphicle'
    description = ('Graphicle: BM25 with Porter stems, a speaker arm and a dense arm, '
                   'date-headed source excerpts packed to a token budget.')
    kind = 'local'
    provider = 'graphicle'
    concurrency = int(os.environ.get('GRAPHICLE_AMB_CONCURRENCY', '8'))

    def __init__(self):
        self.binary = os.environ.get('GRAPHICLE_BIN')
        self.config = os.environ.get('GRAPHICLE_CONFIG')
        knobs = os.environ.get('GRAPHICLE_AMB_KNOBS')
        self.pinned = not knobs
        self.knobs = json.loads(knobs) if knobs else dict(RECIPE)
        self.max_tokens = int(os.environ.get('GRAPHICLE_AMB_MAX_TOKENS', '8000'))
        self.root = None
        # Queries of one unit run one at a time unless this is set; each is a
        # read of the unit's store by its own process.
        self.parallel = os.environ.get('GRAPHICLE_AMB_PARALLEL_QUERIES') == '1'
        self._locks = {}
        self._guard = threading.Lock()

    def initialize(self):
        for variable, value in (('GRAPHICLE_BIN', self.binary), ('GRAPHICLE_CONFIG', self.config)):
            if not value or not Path(value).is_file():
                raise RuntimeError(f'{variable} must name an existing file (got {value!r})')

    def prepare(self, store_dir, unit_ids=None, reset=True):
        self.root = store_root(store_dir)
        if reset and self.root.exists():
            shutil.rmtree(self.root)
        self.root.mkdir(parents=True, exist_ok=True)

    def _unit(self, user_id):
        with self._guard:
            if self.root is None:
                self.root = Path(tempfile.mkdtemp(prefix='graphicle-amb-'))
            name = user_id or '_shared'
            if not re.fullmatch(r'[A-Za-z0-9_-][A-Za-z0-9_.-]{0,79}', name):
                name = 'unit-' + hashlib.sha256(name.encode()).hexdigest()[:16]
            return self.root / name, self._locks.setdefault(name, threading.Lock())

    def _run(self, unit, *args, timeout=900):
        command = [self.binary, '--data-dir', str(unit / 'store'), '--config', self.config, *map(str, args)]
        proc = subprocess.run(command, capture_output=True, text=True, timeout=timeout)
        if proc.returncode:
            raise RuntimeError(f'graphicle {" ".join(map(str, args[:2]))}: {(proc.stderr or proc.stdout)[-800:]}')
        return json.loads(proc.stdout)

    @staticmethod
    def _index(unit):
        path = unit / 'sources.json'
        return json.loads(path.read_text()) if path.exists() else {}

    def ingest(self, documents):
        by_user = {}
        for doc in documents:
            by_user.setdefault(doc.user_id, []).append(doc)
        for user_id, docs in by_user.items():
            unit, lock = self._unit(user_id)
            with lock:
                sessions = unit / 'sessions'
                sessions.mkdir(parents=True, exist_ok=True)
                index = self._index(unit)
                for doc in docs:
                    name = file_name(doc, user_id, index)
                    (sessions / name).write_text(render(doc), encoding='utf-8')
                    index[name] = {'id': doc.id, 'timestamp': doc.timestamp}
                (unit / 'sources.json').write_text(json.dumps(index, indent=1, sort_keys=True))
                self._run(unit, 'sync', sessions)
                if 'local_embedding_profile' in self.knobs:
                    self._embed(unit)

    def _embed(self, unit):
        # As in tools/retrieval_recall.py: repeat 32-call jobs until the vector
        # index covers every passage, with an input budget for 32 full batches.
        budget = unit / 'embed-arguments.json'
        budget.write_text(json.dumps({'max_input_tokens': 512000, 'max_output_tokens': 64}))
        for _ in range(EMBED_ROUNDS):
            job = self._run(unit, 'run', 'embed', '--arguments', budget, '--wait', '--max-calls', EMBED_CALLS)
            if job.get('state') in ('failed', 'cancelled'):
                raise RuntimeError(f'embedding job {job.get("state")}: {str(job.get("error"))[:500]}')
            result = job.get('result') if isinstance(job.get('result'), dict) else job
            if result.get('complete'):
                return
        raise RuntimeError(f'embedding incomplete after {EMBED_ROUNDS} jobs of {EMBED_CALLS} calls')

    def context(self, query, user_id=None, query_timestamp=None, knobs=None, max_tokens=None):
        """One `context` call with `knobs` (default: the provider's) and the
        unit's source index, or (None, {}) when the unit holds no sources."""
        unit, lock = self._unit(user_id)
        index = self._index(unit)
        if not index:
            return None, index
        request, key = context_request(self.knobs if knobs is None else knobs, query,
                                       max_tokens or self.max_tokens, query_timestamp)
        receipts = unit / 'queries'
        with contextlib.nullcontext() if self.parallel else lock:
            receipts.mkdir(exist_ok=True)
            arguments = receipts / f'{key}.json'
            replace_text(arguments, json.dumps(request, sort_keys=True))
            packet = self._run(unit, 'call', 'context', '--arguments', arguments)
            replace_text(receipts / f'{key}.packet.json', json.dumps(packet))
        return packet, index

    def retrieve(self, query, k=10, user_id=None, query_timestamp=None):
        """`k` is unused: excerpts are packed to the token budget instead."""
        packet, index = self.context(query, user_id, query_timestamp)
        if packet is None:
            return [], None
        if self.pinned and packet.get('retrieval_defaults'):
            raise RuntimeError(f'the binary defaulted pinned options: {sorted(packet["retrieval_defaults"])}')
        return documents(packet, index, user_id), None


def main(argv=None):
    if importlib.util.find_spec('memory_bench') is None:
        sys.exit(f'AMB is not installed; see the instructions at the top of {__file__}')
    from memory_bench.memory import REGISTRY
    REGISTRY['graphicle'] = GraphicleProvider
    from memory_bench.cli import app
    app(args=argv)


if __name__ == '__main__':
    main()
