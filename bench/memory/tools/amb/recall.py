#!/usr/bin/env python3
"""Retrieval recall on the Agent Memory Benchmark's datasets, with no model API.

AMB (github.com/vectorize-io/agent-memory-benchmark) scores answers with an LLM
judge. This runner asks only the retrieval question, on AMB's datasets as AMB's
own loaders produce them: does the `context` packet the answer model would read
hold the documents AMB labels gold, and on LoCoMo-format datasets (LoCoMo,
LifeBench) the evidence turns inside them? AMB is imported at run time and
nothing is copied from it (its repository had no license at the pinned commit).
Documents are written, synced and embedded exactly as graphicle_provider.py
ingests them in an AMB run, and the variants are tools/retrieval_recall.py's,
so a recipe measured here is the request an AMB run would send.

    PYTHONPATH=<amb checkout>/src LOCOMO_DATA_PATH=/abs/locomo10.json \\
      .venv/bin/python tools/amb/recall.py --dataset locomo --split locomo10 \\
        --variants lex-rrf,hyb-rrf --budgets 2000,4000,8000 \\
        --embedder .models/arctic-embed-s --reranker .models/mxbai-rerank-xsmall-v1 \\
        --binary target/release/graphicle --out runs/amb-locomo

Metrics per variant and AMB category (retrieval_recall.summarize):
  session_recall  mean fraction of gold documents with any excerpt
  all_sessions    share of queries whose gold documents all appear
  turn_coverage   share of located evidence turns contained whole
  tokens          mean evidence tokens in the packet

Queries without gold documents (LifeBench's unanswerable category) are counted
apart, never as retrieval successes. Evidence strings naming several turns
(`D8:6; D9:17`) are split; ids naming no turn are reported as unlocated and
left out of turn coverage. Gold documents and evidence ids are read for scoring
only; nothing but each document's turns and date reaches the store.
"""
import argparse
import datetime
import hashlib
import importlib.util
import inspect
import json
import os
import re
import shutil
import sys
import time
import types
from concurrent.futures import ThreadPoolExecutor
from dataclasses import asdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3] / 'crates' / 'graphicle'  # the runtime crate: its release build and local models
sys.path.insert(0, str(HERE.parent))
import retrieval_recall  # noqa: E402
from qualify_longmemeval import coverage  # noqa: E402

_spec = importlib.util.spec_from_file_location('graphicle_provider', HERE / 'graphicle_provider.py')
amb = sys.modules['graphicle_provider'] = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(amb)

CONTRACT = 'graphicle.amb-retrieval-recall-protocol.v1'
# memory_bench.dataset imports AMB's LLM package (beam.py imports
# memory_bench.llm), and memory_bench/llm/gemini.py imports google.genai at
# module level, using its types in annotations. No model is called here, so
# an absent client becomes a stub whose attributes are classes (annotations
# such as `types.X | None` must evaluate) that cannot be instantiated.
LLM_CLIENTS = ('google.genai',)
EVIDENCE_SEPARATORS = re.compile(r'[;,\s]+')
SAFE = re.compile(r'[A-Za-z0-9_-][A-Za-z0-9_.-]{0,119}')
# LoCoMo's QA category numbers as Mem0's and MemOS's LoCoMo evaluations name
# them (MemOS evaluation/scripts/locomo/locomo_metric.py). AMB's loader names
# 1, 3 and 4 single-hop, multi-hop and open-domain; both groupings are kept.
LOCOMO_LABELS = {1: 'multi-hop', 2: 'temporal', 3: 'open-domain', 4: 'single-hop', 5: 'adversarial'}


class _Absent(type):
    def __getattr__(cls, attr):
        if attr.startswith('__'):
            raise AttributeError(attr)
        return _Absent(f'{cls.__name__}.{attr}', (), {})

    def __call__(cls, *args, **kwargs):
        raise ImportError(f'{cls.__name__} is not installed; tools/amb/recall.py calls no model')


def stub_clients():
    for name in LLM_CLIENTS:
        try:
            present = name in sys.modules or importlib.util.find_spec(name) is not None
        except (ImportError, ValueError):
            present = False
        if present:
            continue
        parent, _, leaf = name.rpartition('.')
        if parent and parent not in sys.modules:
            try:
                importlib.import_module(parent)
            except ImportError:
                sys.modules[parent] = types.ModuleType(parent)
        module = types.ModuleType(name)

        def absent(attr, _name=name):
            if attr.startswith('__'):
                raise AttributeError(attr)
            return _Absent(f'{_name}.{attr}', (), {})

        module.__getattr__ = absent
        sys.modules[name] = module
        if parent:
            setattr(sys.modules[parent], leaf, module)


def evidence_ids(values):
    return [part for value in values or [] for part in EVIDENCE_SEPARATORS.split(str(value)) if part]


def load(name, split, category=None, limit=None, units=None):
    """AMB's queries and documents for one split, and each query's evidence
    turn ids and QA category number where the raw data has LoCoMo-format `qa`
    items."""
    stub_clients()
    from memory_bench.dataset import get_dataset
    dataset = get_dataset(name)
    queries = dataset.load_queries(split, category=category, limit=limit)
    if units:
        queries = [q for q in queries if q.user_id in units]
    users = {q.user_id for q in queries}
    documents = dataset.load_documents(split, user_ids=None if None in users else users)
    evidence, categories = {}, {}
    raw = getattr(dataset, '_load_raw', None)
    if callable(raw):
        try:
            items = raw()
        except TypeError:
            items = []
        for item in items:
            if isinstance(item, dict) and isinstance(item.get('qa'), list) and 'sample_id' in item:
                for qi, qa in enumerate(item['qa']):
                    if isinstance(qa, dict):
                        evidence[f"{item['sample_id']}_q{qi}"] = evidence_ids(qa.get('evidence'))
                        categories[f"{item['sample_id']}_q{qi}"] = qa.get('category')
    return dataset, queries, documents, evidence, categories


def fingerprint(queries, documents):
    """sha256 of the loaded split, whatever file or download it came from."""
    digest = hashlib.sha256()
    for item in [asdict(q) for q in queries] + [asdict(d) for d in documents]:
        digest.update(json.dumps(item, sort_keys=True, default=str).encode())
        digest.update(b'\n')
    return digest.hexdigest()


def resolve(variants, defaults):
    """retrieval_recall.evaluate's pinning: on a binary that defaults options
    when unset, every variant but `-server` ones sends them as off."""
    out = {}
    for name, knobs in variants.items():
        knobs = dict(knobs)
        server = knobs.pop('server_defaults', False)
        if defaults is not None and not server:
            knobs = {**retrieval_recall.RECIPE_OFF, **knobs}
        out[name] = (knobs, server)
    return out


def probe(binary, config, workdir):
    """The binary's `retrieval_defaults` on a one-file store."""
    folder = workdir / 'sessions'
    folder.mkdir(parents=True, exist_ok=True)
    (folder / 'probe.txt').write_text('user: probe\n\n')
    retrieval_recall.run(binary, workdir / 'store', config, 'sync', folder)
    arguments = workdir / 'probe.json'
    arguments.write_text(json.dumps({'question': 'probe', 'max_tokens': 200, 'limit': 1}))
    return retrieval_recall.run(binary, workdir / 'store', config, 'call', 'context', '--arguments',
                                arguments).get('retrieval_defaults')


def score(packet, texts, labelled, mapping, gold):
    evidence = packet.get('evidence') or []
    scored = coverage({'evidence': evidence}, texts, labelled, mapping, gold)
    gold = set(gold)
    from_gold = [mapping.get(item.get('id')) in gold for item in evidence]
    return {'session_recall': scored['annotated_sessions_touched'] / max(1, scored['annotated_sessions']),
            'all_sessions': scored['all_annotated_sessions_touched'],
            'turns': scored['annotated_answer_turns'], 'turns_complete': scored['complete_annotated_answer_turns'],
            'tokens': packet.get('evidence_tokens') or packet.get('tokens') or 0,
            'excerpts': len(evidence), 'gold_excerpts': sum(from_gold),
            'first_gold_rank': from_gold.index(True) + 1 if any(from_gold) else None,
            'local_model_calls': packet.get('model_calls', 0),
            'invalid_evidence_intervals': scored['invalid_evidence_intervals'],
            'ambiguous_unanchored_quotes': scored['ambiguous_unanchored_quotes'],
            'defaulted': sorted(packet.get('retrieval_defaults') or {})}


class Unit:
    """One isolation unit's store: its documents ingested once through the
    provider, then every query and variant asked in turn."""

    def __init__(self, provider, name, documents):
        self.provider, self.name, self.documents = provider, name, documents
        self.folder = provider._unit(name)[0]

    def ingest(self):
        marker = self.folder / 'INGESTED.json'
        if not marker.exists():
            if self.folder.exists():  # an interrupted ingest starts over
                shutil.rmtree(self.folder)
            started = time.monotonic()
            self.provider.ingest(self.documents)
            retrieval_recall.save(marker, {'documents': len(self.documents),
                                           'seconds': round(time.monotonic() - started, 2)})
        index = self.provider._index(self.folder)
        by_id = {}
        for doc in self.documents:
            by_id.setdefault(doc.id, doc)
        self.texts, spans = {}, {}
        for name, source in index.items():
            text, spans[name] = amb.render_spans(by_id[source['id']])
            if (self.folder / 'sessions' / name).read_text(encoding='utf-8') != text:
                raise RuntimeError(f'{self.name}/{name} differs from its document; delete the unit to re-ingest')
            self.texts[name] = text
        self.mapping = {'file:' + name: source['id'] for name, source in index.items()}
        self.turns = {}
        for name, turn_spans in spans.items():
            for dia, (start, end) in turn_spans.items():
                self.turns.setdefault(dia, ('file:' + name, {'start': start, 'end': end}))
        return marker

    def case(self, query, evidence, variants, locomo_category=None):
        located = [self.turns[dia] for dia in dict.fromkeys(evidence) if dia in self.turns]
        labelled = {}
        for sid, span in located:
            labelled.setdefault(sid, []).append(span)
        gold = sorted(set(query.gold_ids))
        out = {'question_id': query.id, 'type': query.meta.get('category') or 'all', 'unit': self.name,
               'answer_sessions': len(gold), 'abstention': not gold,
               'gold_not_in_store': len(set(gold) - set(self.mapping.values())),
               'evidence_ids': len(set(evidence)), 'annotated_turns': len(located),
               'unlocated_evidence': len(set(evidence)) - len(located), 'locomo_category': locomo_category,
               'variants': {}}
        for name, (knobs, server) in variants.items():
            request = {k: v for k, v in knobs.items() if k != 'max_tokens'}
            started = time.monotonic()
            packet, _ = self.provider.context(query.query, user_id=query.user_id,
                                              query_timestamp=query.meta.get('query_timestamp'),
                                              knobs=request, max_tokens=knobs['max_tokens'])
            seconds = time.monotonic() - started
            if packet is None:
                raise RuntimeError(f'unit {self.name} holds no sources')
            if packet.get('retrieval_defaults') and not server:
                raise RuntimeError(f'{name}: the binary defaulted {sorted(packet["retrieval_defaults"])} '
                                   'although the variant pins them')
            out['variants'][name] = {**score(packet, self.texts, labelled, self.mapping, gold),
                                     'seconds': round(seconds, 2)}
        return out


def parse(argv):
    p = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    p.add_argument('--dataset', required=True, help='AMB dataset name, e.g. locomo, lifebench, longmemeval')
    p.add_argument('--split', required=True, help='AMB split, e.g. locomo10, en, s')
    p.add_argument('--category', help="AMB category filter, passed to the dataset's loaders")
    p.add_argument('--limit', type=int, help="AMB's query limit (the first N queries), for smoke runs")
    p.add_argument('--units', help='comma-separated isolation units (e.g. conv-26) to keep')
    p.add_argument('--amb', type=Path, help="AMB's src directory, when memory_bench is not importable")
    p.add_argument('--binary', type=Path, default=ROOT / 'target/release/graphicle')
    p.add_argument('--variants', default='l12-lexfq,lex-rrf', help='tools/retrieval_recall.py variant names')
    p.add_argument('--budgets', help='comma-separated evidence token budgets; each variant runs at each, named '
                                     'VARIANT@<k>k, instead of at its own max_tokens')
    p.add_argument('--reranker', type=Path, default=ROOT / '.models/mxbai-rerank-xsmall-v1')
    p.add_argument('--embedder', type=Path, help='local ONNX embedding model directory, for dense variants')
    p.add_argument('--ort', type=Path, help='onnxruntime dynamic library (ORT_DYLIB_PATH)')
    p.add_argument('--paired', action='append', default=[], help='A,B: retrieval_recall.paired on two variants')
    p.add_argument('--workers', type=int, default=8, help='isolation units run in parallel')
    p.add_argument('--query-workers', type=int, default=1,
                   help="queries of one unit run in parallel, each a read of the unit's store")
    p.add_argument('--out', type=Path, required=True)
    p.add_argument('--resume', action='store_true', help='continue a frozen run; recorded failures stay failures')
    a = p.parse_args(argv)
    if a.workers < 1 or a.query_workers < 1 or (a.limit is not None and a.limit < 1):
        p.error('--workers, --query-workers and --limit must be positive')
    names = [v for v in a.variants.split(',') if v]
    unknown = [v for v in names if v not in retrieval_recall.VARIANTS]
    if not names or len(names) != len(set(names)) or unknown:
        p.error(f'select known variants without duplicates (unknown: {unknown})')
    if any(retrieval_recall.VARIANTS[v].get('expansion') for v in names):
        p.error('mq- variants need precomputed query variants, which this runner does not take')
    a.variants = {}
    budgets = [int(b) for b in a.budgets.split(',') if b] if a.budgets else []
    if any(b < 100 for b in budgets):
        p.error('--budgets are evidence tokens of at least 100')
    for v in names:
        for budget in budgets or [None]:
            knobs = dict(retrieval_recall.VARIANTS[v])
            if budget:
                knobs['max_tokens'] = budget
            a.variants[f'{v}@{budget / 1000:g}k' if budget else v] = knobs
    a.pairs = [tuple(x.split(',')) for x in a.paired]
    if any(len(x) != 2 or not set(x) <= set(a.variants) for x in a.pairs):
        p.error(f'--paired takes A,B among {sorted(a.variants)}')
    a.dense = any('local_embedding_profile' in k for k in a.variants.values())
    a.rerank = any('local_reranker_profile' in k for k in a.variants.values())
    if a.dense and not a.embedder:
        p.error('dense variants need --embedder')
    if not a.binary.is_file():
        p.error(f'--binary {a.binary} does not exist')
    a.units = set(a.units.split(',')) if a.units else None
    return a


def models(a):
    """models.json for the selected variants, as tools/retrieval_recall.py writes it."""
    config = {'models': {}}
    if a.rerank:
        manifest = json.loads((a.reranker / 'MODEL.json').read_text())
        config['models']['reranker'] = {'protocol': 'local_onnx', 'base_url': a.reranker.resolve().as_uri() + '/',
                                        'model': manifest['model'], 'input_limit': 1000}
    if a.dense:
        manifest = json.loads((a.embedder / 'MODEL.json').read_text())
        config['models']['embedding'] = {'protocol': 'local_onnx', 'base_url': a.embedder.resolve().as_uri() + '/',
                                         'model': manifest['model'], 'input_limit': 8000}
        config['embedding_model'] = 'embedding'
    return (json.dumps(config, sort_keys=True) + '\n').encode()


def main(argv=None):
    a = parse(argv)
    if a.amb:
        sys.path.insert(0, str(a.amb.resolve()))
    if importlib.util.find_spec('memory_bench') is None:
        sys.exit('memory_bench is not importable: pass --amb <AMB checkout>/src or set PYTHONPATH')
    dataset, queries, documents, evidence, categories = load(a.dataset, a.split, a.category, a.limit, a.units)
    if not queries:
        sys.exit('the selection holds no queries')
    out = a.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    config_bytes = models(a)
    artifacts = {}
    for directory in ([a.reranker] if a.rerank else []) + ([a.embedder] if a.dense else []):
        for path in sorted(directory.rglob('*')):
            if path.is_file():
                artifacts[str(path.resolve())] = retrieval_recall.digest(path)
    ort = a.ort or next(iter(sorted(ROOT.glob('.qualification-venv/lib/python3*/site-packages/onnxruntime/capi/'
                                              'libonnxruntime*.dylib'))), None)
    if ort and (a.dense or a.rerank):
        os.environ['ORT_DYLIB_PATH'] = str(Path(ort).resolve())
        artifacts[str(Path(ort).resolve())] = retrieval_recall.digest(ort)
    os.environ.update(CUDA_VISIBLE_DEVICES='', NVIDIA_VISIBLE_DEVICES='void')
    loader = inspect.getsourcefile(type(dataset))
    inputs = json.loads(json.dumps({
        'binary_sha256': retrieval_recall.digest(a.binary),
        'runner_sha256': retrieval_recall.digest(__file__),
        'provider_sha256': retrieval_recall.digest(HERE / 'graphicle_provider.py'),
        'harness_sha256': retrieval_recall.digest(HERE.parent / 'retrieval_recall.py'),
        'scorer_sha256': retrieval_recall.digest(HERE.parent / 'qualify_longmemeval.py'),
        'configuration_sha256': hashlib.sha256(config_bytes).hexdigest(), 'local_artifacts': artifacts,
        'amb': {'dataset': a.dataset, 'split': a.split, 'category': a.category, 'limit': a.limit,
                'units': sorted(a.units) if a.units else None, 'provider_pinned_commit': amb.AMB_COMMIT,
                'loader_sha256': retrieval_recall.digest(loader) if loader else None,
                'isolation_unit': getattr(dataset, 'isolation_unit', None)},
        'data_sha256': fingerprint(queries, documents), 'variants': a.variants, 'pairs': a.pairs,
        'queries': [{'id': q.id, 'type': q.meta.get('category') or 'all', 'unit': q.user_id} for q in queries]}))
    protocol_file = out / 'PROTOCOL.json'
    config = out / 'models.json'
    if protocol_file.exists():
        if not a.resume:
            sys.exit('run already frozen; use --resume with identical inputs or a new --out')
        if json.loads(protocol_file.read_text())['inputs'] != inputs:
            sys.exit('frozen inputs changed; start a new run instead of mixing results')
    else:
        if a.resume:
            sys.exit('no frozen protocol exists to resume')
        config.write_bytes(config_bytes)
        retrieval_recall.save(protocol_file, {
            'contract': CONTRACT, 'inputs': inputs,
            'frozen_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
            'remote_model_calls': 0, 'local_inference': bool(artifacts), 'cpu_only': True,
            'answer_accuracy_measured': False, 'leaderboard_claim': False,
            'ingestion': "Each document's turns (speaker, text, shared-image caption) and date only; ids, "
                         'gold labels and evidence ids are read for scoring and never ingested.',
            'failure_policy': 'No automatic retries. Failures remain in headline denominators.'})
    defaults = probe(a.binary, config, out / 'probe')
    variants = resolve(a.variants, defaults)
    provider = amb.GraphicleProvider()
    provider.binary, provider.config = str(a.binary.resolve()), str(config)
    provider.knobs = {'local_embedding_profile': 'embedding'} if a.dense else {}
    provider.parallel = a.query_workers > 1
    provider.prepare(out / 'stores', reset=False)
    by_unit = {}
    for doc in documents:
        by_unit.setdefault(doc.user_id, [[], []])[0].append(doc)
    for query in queries:
        by_unit.setdefault(query.user_id, [[], []])[1].append(query)
    cases_dir = out / 'cases'
    cases_dir.mkdir(exist_ok=True)

    def case_path(query):
        name = query.id if SAFE.fullmatch(query.id) else 'q-' + hashlib.sha256(query.id.encode()).hexdigest()[:24]
        return cases_dir / f'{name}.json'

    def attempt(item):
        name, (docs, unit_queries) = item
        results = []
        pending = []
        for query in unit_queries:
            path = case_path(query)
            saved = json.loads(path.read_text()) if path.exists() else None
            if saved and (saved.get('complete') or 'error' in saved):
                results.append(saved)
            else:
                pending.append(query)
        if not pending:
            return results
        unit = Unit(provider, name, docs)
        try:
            unit.ingest()
        except Exception as error:
            failure = f'ingest: {type(error).__name__}: {error}'[:2000]
            for query in pending:
                case = {'question_id': query.id, 'type': query.meta.get('category') or 'all', 'unit': name,
                        'answer_sessions': len(set(query.gold_ids)), 'abstention': not query.gold_ids,
                        'locomo_category': categories.get(query.id),
                        'variants': {}, 'error': failure}
                retrieval_recall.save(case_path(query), case)
                results.append(case)
            return results
        def ask(query):
            try:
                case = unit.case(query, evidence.get(query.id, []), variants, categories.get(query.id))
                case['complete'] = True
            except Exception as error:
                case = {'question_id': query.id, 'type': query.meta.get('category') or 'all', 'unit': name,
                        'answer_sessions': len(set(query.gold_ids)), 'abstention': not query.gold_ids,
                        'locomo_category': categories.get(query.id),
                        'variants': {}, 'error': f'{type(error).__name__}: {error}'[:2000]}
            retrieval_recall.save(case_path(query), case)
            return case

        if a.query_workers > 1:
            with ThreadPoolExecutor(max_workers=min(a.query_workers, len(pending))) as pool:
                results.extend(pool.map(ask, pending))
        else:
            results.extend(map(ask, pending))
        return results

    started = time.monotonic()
    work = [(name, value) for name, value in by_unit.items() if value[1]]
    with ThreadPoolExecutor(max_workers=max(1, min(a.workers, len(work)))) as pool:
        cases = [case for results in pool.map(attempt, work) for case in results]
    order = {q.id: i for i, q in enumerate(queries)}
    cases.sort(key=lambda c: order.get(c['question_id'], len(order)))
    summary = {'contract': 'graphicle.amb-retrieval-recall-summary.v1', 'dataset': a.dataset, 'split': a.split,
               'queries': len(cases), 'units': len(work), 'failed': sum(1 for c in cases if 'error' in c),
               'unlocated_evidence': sum(c.get('unlocated_evidence', 0) for c in cases),
               'evidence_ids': sum(c.get('evidence_ids', 0) for c in cases),
               'gold_not_in_store': sum(c.get('gold_not_in_store', 0) for c in cases),
               'binary_defaults': defaults, 'wall_seconds': round(time.monotonic() - started, 1),
               'by_variant': retrieval_recall.summarize(cases, a.variants),
               'by_variant_locomo_labels': retrieval_recall.summarize(
                   [{**c, 'type': LOCOMO_LABELS.get(c.get('locomo_category'), 'other')} for c in cases],
                   a.variants) if any(c.get('locomo_category') for c in cases) else None,
               'paired': [retrieval_recall.paired(cases, x, y) for x, y in a.pairs]}
    retrieval_recall.save(out / 'SUMMARY.json', summary)
    print(json.dumps({name: rows.get('ALL') for name, rows in summary['by_variant'].items()}, indent=1))
    return 0 if not summary['failed'] else 1


if __name__ == '__main__':
    sys.exit(main())
