#!/usr/bin/env python3
"""CPU retrieval recall on public LongMemEval-S/M histories.

Each selected question's sessions are synced as files into a fresh store
through the real CLI; each variant then asks `context` for the retrieval
packet the answer model would read, and the packet is scored against the
dataset's own support labels (which sessions and which turns hold the
answer). No model API is called. Optional local ONNX inference is reported
separately; this never measures answer accuracy or leaderboard performance.

Metrics per variant and question type:
  session_recall   mean fraction of answer sessions with any excerpt
  all_sessions     share of questions whose answer sessions all appear
  turn_coverage    share of labelled answer turns contained whole
  tokens           mean evidence tokens in the packet
"""
import argparse
import datetime
import hashlib
import json
import os
import shutil
import statistics
import subprocess
import sys
import time
from collections import defaultdict
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from qualify_longmemeval import coverage, history

# The runtime crate (crates/graphicle in the repository): the defaults below name
# its debug build, local models and qualification data (the data is not in the
# public repository; pass the dataset's path).
ROOT = Path(__file__).resolve().parents[3] / 'crates' / 'graphicle'
DATASET = ROOT / 'qualification/0.4/datasets/longmemeval_s_cleaned.json'
SEED = 'graphicle-retrieval-recall-v1'
# The observer runner's answer limits: 8000 context tokens, 12 excerpts.
VARIANTS = {
    'default': {'max_tokens': 8000, 'limit': 12},
    'deeper': {'max_tokens': 8000, 'limit': 24},
    'deepest': {'max_tokens': 8000, 'limit': 40},
    'deeper-wider': {'max_tokens': 12000, 'limit': 32},
    'deeper-12k': {'max_tokens': 12000, 'limit': 24},
    'deepest-16k': {'max_tokens': 16000, 'limit': 40},
    'no-expansion': {'max_tokens': 8000, 'limit': 12, 'expand_context': False},
    'rerank': {'max_tokens': 8000, 'limit': 12, 'local_reranker_profile': 'reranker', 'rerank_mode': 'replace'},
    'rerank-blend': {'max_tokens': 8000, 'limit': 12, 'local_reranker_profile': 'reranker', 'rerank_mode': 'blend'},
    'deeper-rerank': {'max_tokens': 8000, 'limit': 24, 'local_reranker_profile': 'reranker', 'rerank_mode': 'replace'},
    'deeper-rerank-blend': {'max_tokens': 8000, 'limit': 24, 'local_reranker_profile': 'reranker', 'rerank_mode': 'blend'},
    # Diagnostics: fused candidates before selection and packing (P1 of the 09-30 port plan).
    'diag': {'max_tokens': 8000, 'limit': 12, 'debug_candidates': 100},
    # Techniques adapted on 09-30, all opt-in.
    'focus': {'max_tokens': 8000, 'limit': 12, 'pack_focus': True},
    'quota': {'max_tokens': 8000, 'limit': 12, 'origin_quota': {'max': 3, 'min': 1}},
    'focus-quota-24': {'max_tokens': 8000, 'limit': 24, 'pack_focus': True, 'origin_quota': {'max': 3, 'min': 1}},
    'rr60-rrf': {'max_tokens': 8000, 'limit': 24, 'local_reranker_profile': 'reranker', 'rerank_depth': 60, 'rerank_mode': 'rrf'},
    'rr60-blend': {'max_tokens': 8000, 'limit': 24, 'local_reranker_profile': 'reranker', 'rerank_depth': 60, 'rerank_mode': 'blend'},
    # `dense` pins weight 1 (the default before SEMANTIC_WEIGHT); `dense-default`
    # takes the binary's own default (0.5), which `dense-w05` pins.
    'dense': {'max_tokens': 8000, 'limit': 12, 'local_embedding_profile': 'embedding', 'arm_weights': {'semantic': 1.0}},
    'dense-default': {'max_tokens': 8000, 'limit': 12, 'local_embedding_profile': 'embedding'},
    'dense-w05': {'max_tokens': 8000, 'limit': 12, 'local_embedding_profile': 'embedding', 'arm_weights': {'semantic': 0.5}},
    'dense-w15': {'max_tokens': 8000, 'limit': 12, 'local_embedding_profile': 'embedding', 'arm_weights': {'semantic': 1.5}},
    'dense-w2': {'max_tokens': 8000, 'limit': 12, 'local_embedding_profile': 'embedding', 'arm_weights': {'semantic': 2.0}},
    'dense-w3': {'max_tokens': 8000, 'limit': 12, 'local_embedding_profile': 'embedding', 'arm_weights': {'semantic': 3.0}},
    # Combinations of the dev120 winners (weighted dense, quotas, CE depth).
    'dense-w2-rr60-rrf': {'max_tokens': 8000, 'limit': 24, 'local_embedding_profile': 'embedding', 'arm_weights': {'semantic': 2.0}, 'local_reranker_profile': 'reranker', 'rerank_depth': 60, 'rerank_mode': 'rrf'},
    # Techniques adapted on 10-02, all opt-in.
    'stem': {'max_tokens': 8000, 'limit': 12, 'stem': True},
    'stem-fq24': {'max_tokens': 8000, 'limit': 24, 'pack_focus': True, 'origin_quota': {'max': 3, 'min': 1}, 'stem': True},
    # LongMemEval's own retrieval keys index the user's turns only.
    'speaker': {'max_tokens': 8000, 'limit': 12, 'speaker': True},
    'speaker-half': {'max_tokens': 8000, 'limit': 12, 'speaker': True, 'arm_weights': {'speaker': 0.5}},
    'speaker-2': {'max_tokens': 8000, 'limit': 12, 'speaker': True, 'arm_weights': {'speaker': 2.0}},
    'speaker-stem': {'max_tokens': 8000, 'limit': 12, 'stem': True, 'speaker': True},
    'speaker-stem-fq24': {'max_tokens': 8000, 'limit': 24, 'pack_focus': True, 'origin_quota': {'max': 3, 'min': 1}, 'stem': True, 'speaker': True},
    # `auto` picks the side from the question: the assistant's turns for what
    # "you said", no arm for a neutral "our previous chat", else the user's.
    'speaker-auto': {'max_tokens': 8000, 'limit': 12, 'speaker': 'auto'},
    'speaker-auto-stem': {'max_tokens': 8000, 'limit': 12, 'stem': True, 'speaker': 'auto'},
    'speaker-auto-stem-fq24': {'max_tokens': 8000, 'limit': 24, 'pack_focus': True, 'origin_quota': {'max': 3, 'min': 1}, 'stem': True, 'speaker': 'auto'},
    # Fixed evidence budgets over the same 60 reranked candidates.
    **{f'rr60-l40-{k}k': {'max_tokens': k * 1000, 'limit': 40, 'local_reranker_profile': 'reranker', 'rerank_depth': 60, 'rerank_mode': 'rrf'} for k in (4, 6, 8, 12)},
    'rr60-replace': {'max_tokens': 8000, 'limit': 24, 'local_reranker_profile': 'reranker', 'rerank_depth': 60, 'rerank_mode': 'replace'},
}
# Combined recipes. `lex-` fuses Porter stems with the auto speaker arm; `hyb-`
# adds the dense arm at weight 2 (needs --embedder). `hyb-nr-` packs the fused
# ranking directly. The others rerank 60 candidates with the cross-encoder
# (RRF) and pack 24 excerpts in 8K tokens or up to 40 in a fixed budget, `-fq-`
# with focus windows and an origin quota. `hyb-w05` is `hyb-rrf` as the
# defaults now have it: the dense arm at weight 0.5 and no cross-encoder,
# which did not raise held-out recall on a single conversation's history.
LEXICAL = {'stem': True, 'speaker': 'auto'}
DENSE = {'local_embedding_profile': 'embedding', 'arm_weights': {'semantic': 2.0}}
RERANK = {'local_reranker_profile': 'reranker', 'rerank_depth': 60, 'rerank_mode': 'rrf'}
FOCUS = {'pack_focus': True, 'origin_quota': {'max': 3, 'min': 1}}
VARIANTS['hyb-nr-fq24'] = {'max_tokens': 8000, 'limit': 24, **LEXICAL, **DENSE, **FOCUS}
VARIANTS['hyb-w05'] = {'max_tokens': 8000, 'limit': 24, **LEXICAL, **DENSE, 'arm_weights': {'semantic': 0.5}}
# The retrieval of a `basic` job given 8,000 context tokens (search.rs): the
# recipe defaults and default_evidence_limit's 24 excerpts.
VARIANTS['basic-8k'] = {'max_tokens': 8000, 'limit': 24, **LEXICAL, 'pack_focus': True}
for prefix, arms in (('lex', LEXICAL), ('hyb', {**LEXICAL, **DENSE})):
    VARIANTS[f'{prefix}-rrf'] = {'max_tokens': 8000, 'limit': 24, **arms, **RERANK}
    for k in (4, 5, 6, 8):
        VARIANTS[f'{prefix}-l40-{k}k'] = {'max_tokens': k * 1000, 'limit': 40, **arms, **RERANK}
        VARIANTS[f'{prefix}-fq-l40-{k}k'] = {'max_tokens': k * 1000, 'limit': 40, **arms, **RERANK, **FOCUS}
# Reranker depth and mode on the hybrid recipe: `hyb-d32-blend` is what a
# request naming only the reranker profile gets at limit 24 (query.rs: depth
# 32, blend); pair it with the depth-60 modes and RRF read 100, 200 and 300
# deep into the fused order (Hindsight reranks up to 300 candidates).
for name, knobs in (('d32-blend', {'rerank_depth': 32, 'rerank_mode': 'blend'}),
                    ('blend', {'rerank_mode': 'blend'}), ('replace', {'rerank_mode': 'replace'}),
                    ('d100-rrf', {'rerank_depth': 100}), ('d200-rrf', {'rerank_depth': 200}),
                    ('d300-rrf', {'rerank_depth': 300})):
    VARIANTS[f'hyb-{name}'] = {**VARIANTS['hyb-rrf'], **knobs}
# Focus windows (packing.rs `focus_span`, after SmartSearch) on the RRF
# recipes: a window longer than its share of the budget keeps its
# best-matching paragraphs, not its prefix. Ranking is unchanged; pair each
# `-f` with its base.
for base in ('lex-rrf', 'hyb-rrf'):
    VARIANTS[f'{base}-f'] = {**VARIANTS[base], 'pack_focus': True}
# Compact citations (packing.rs `compact_citations`): each excerpt is cited by
# path and line; the ID, SHA-256 and continuation lines stay in `sources`,
# and the tokens they cost go to evidence.
for base in ('lex-rrf', 'hyb-rrf'):
    VARIANTS[f'{base}-c'] = {**VARIANTS[base], 'compact_citations': True}
    VARIANTS[f'{base}-fc'] = {**VARIANTS[f'{base}-f'], 'compact_citations': True}
    # Focus options: `s` matches Porter stems; `t<k>` keeps the matching run
    # plus k paragraphs each side, leaving the rest of the share to later
    # sources. Pair each with `-fc`.
    for suffix, focus in (('s', {'stem': True}), ('t1', {'context': 1}), ('t2', {'context': 2}),
                          ('st1', {'stem': True, 'context': 1})):
        VARIANTS[f'{base}-fc{suffix}'] = {**VARIANTS[f'{base}-fc'], 'pack_focus': focus}
    # One source per origin before any origin gets a second, so more distinct
    # documents (sessions) reach the packet. Pair with `-fc` or `-fct1`.
    for suffix in ('', 't1'):
        VARIANTS[f'{base}-fc{suffix}q1'] = {**VARIANTS[f'{base}-fc{suffix}'], 'origin_quota': {'max': 1, 'min': 1}}
# Query expansion (search.rs, after Onyx's weighted multi-query fusion): `mq-X`
# is X plus the rewrite, keyword and hypothetical-answer variants that
# tools/expand_queries.py wrote for each question (--query-variants).
VARIANTS.update({f'mq-{name}': {**VARIANTS[name], 'expansion': True}
                 for name in ('default', 'deeper', 'stem', 'speaker-auto-stem', 'dense-default', 'hyb-nr-fq24',
                              'hyb-rrf')})
# The API's own budget (query.rs: 3,000 tokens, 8 excerpts), which MCP's
# recall_project always gets, and the observer runner's 8,000 and 12, each
# step of the lexical recipe added in turn. Budgets are explicit so a binary's
# defaults never change what a name means.
for prefix, budget in (('api', {'max_tokens': 3000, 'limit': 8}), ('l12', {'max_tokens': 8000, 'limit': 12})):
    for name, knobs in (('', {}), ('-lex', LEXICAL), ('-lexf', {**LEXICAL, 'pack_focus': True}),
                        ('-lexq', {**LEXICAL, 'origin_quota': FOCUS['origin_quota']}), ('-lexfq', {**LEXICAL, **FOCUS}),
                        ('-server', {'server_defaults': True})):
        VARIANTS[prefix + name] = {**budget, **knobs}
# Options a binary may default when unset (query.rs `recipe_defaults`). On such
# a binary every variant sends the ones it leaves unset as off, so a name means
# the same request before and after a default changes; `-server` variants send
# none and measure the binary's own defaults. Older binaries read an absent
# option as off but `origin_quota: false` as on, so they get nothing added.
RECIPE_OFF = {'stem': False, 'speaker': False, 'pack_focus': False, 'origin_quota': False}
PINNED = {}


def digest(path):
    with Path(path).open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    pending = path.with_suffix(path.suffix + '.tmp')
    pending.write_text(json.dumps(value, indent=2, ensure_ascii=False) + '\n')
    pending.replace(path)


def run(binary, store, config, *args, env=None):
    # A crash is a failed observation, even if a process committed its work
    # first. Silent retries would hide reliability failures from the receipt.
    proc = subprocess.run([str(binary), '--data-dir', str(store), '--config', str(config), *map(str, args)],
                          capture_output=True, text=True, timeout=900, env=env)
    if proc.returncode:
        raise RuntimeError(f'{args[0]}: {proc.stderr[-500:] or proc.stdout[-500:]}')
    return json.loads(proc.stdout)


def candidate_metrics(packet, docs, spans, mapping, required):
    """Recall of the fused candidates before selection and packing, and the rank
    of the first candidate from an answer session (the P1 diagnostics)."""
    listed = packet.get('candidates_debug')
    if not listed:
        return {}
    windows = []
    first = None
    for item in listed:
        name = str(item.get('id', '')).removeprefix('file:')
        raw = docs.get(name, '').encode()
        a, b = item.get('byte_start') or 0, item.get('byte_end') or 0
        windows.append({'id': item.get('id'), 'text': raw[a:b].decode(), 'byte_start': a, 'byte_end': b})
        if first is None and mapping.get(item.get('id')) in set(required):
            first = item.get('rank')
    out = {'first_gold_rank': first}
    need = max(1, len(set(required) & set(mapping.values())))
    for k in (12, 24, 40, 100):
        scored = coverage({'evidence': windows[:k]}, docs, spans, mapping, required)
        out[f'cand_session_recall@{k}'] = scored['annotated_sessions_touched'] / need
        out[f'cand_turns_complete@{k}'] = scored['complete_annotated_answer_turns']
    return out


def stable_history(row):
    """history() with each file named by a hash of its dataset session id.

    A passage's title is its file name, so positional names make a session's
    embedded text differ between questions; hashed names let an embedding
    cache (GRAPHICLE_EMBEDDING_CACHE) reuse vectors across questions. Raw ids
    are never used because 'answer_' ids would leak support labels.
    """
    docs, spans, mapping = history(row)
    renamed = ({}, {}, {})
    for name, text in docs.items():
        sid = mapping['file:' + name]
        base = 'session-' + hashlib.sha256(sid.encode()).hexdigest()[:16]
        new, n = base + '.txt', 1
        while new in renamed[0]:
            n += 1
            new = f'{base}-{n}.txt'
        renamed[0][new] = text
        renamed[1]['file:' + new] = spans['file:' + name]
        renamed[2]['file:' + new] = sid
    return renamed


def evaluate(row, variants, binary, config, workdir, env, embed=False, embedding_rounds=128, expansions=None,
             stable_names=False):
    docs, spans, mapping = (stable_history if stable_names else history)(row)
    case = workdir / row['question_id']
    folder = case / 'sessions'
    folder.mkdir(parents=True, exist_ok=True)
    for name, text in docs.items():
        (folder / name).write_text(text)
    store = case / 'store'
    started = time.monotonic()
    synced = run(binary, store, config, 'sync', folder, env=env)
    save(case / 'SYNC.json', synced)
    sync_seconds = time.monotonic() - started
    embedded = None
    if embed:
        # Each job embeds at most 32 batches of 16 passages; repeat until the
        # vector index covers every passage so the dense arm is complete. The
        # input budget must cover 32 full batches: the 64K default stops a job
        # with "Embedding budget exhausted" after about nine 8K-token batches.
        budget = case / 'embed-arguments.json'
        budget.write_text(json.dumps({'max_input_tokens': 512000, 'max_output_tokens': 64}))
        totals = {'embedded_passages': 0, 'cached_passages': 0, 'rounds': 0}
        for round_number in range(embedding_rounds):
            job = run(binary, store, config, 'run', 'embed', '--arguments', budget, '--wait', '--max-calls', '32',
                      env=env)
            save(case / f'EMBED-{round_number:03d}.json', job)
            result = job.get('result') if isinstance(job.get('result'), dict) else job
            if job.get('state') in ('failed', 'cancelled'):
                raise RuntimeError(f"embed: {job.get('state')}: {str(job.get('error'))[:300]}")
            embedded = result
            totals['rounds'] += 1
            for k in ('embedded_passages', 'cached_passages'):
                totals[k] += result.get(k) or 0
            if result.get('complete'):
                break
        if not (embedded or {}).get('complete'):
            raise RuntimeError(f'embed: index incomplete after {embedding_rounds} rounds: {embedded}')
    out = {'question_id': row['question_id'], 'type': row['question_type'], 'sessions': len(docs),
           'answer_sessions': len(set(row['answer_session_ids'])),
           'abstention': row['question_id'].endswith('_abs'), 'sync_seconds': sync_seconds,
           'annotated_turns': sum(len(turns) for turns in spans.values()), 'variants': {}}
    checkpoint = case / 'RESULT.json'
    if checkpoint.exists():
        out['variants'] = json.loads(checkpoint.read_text()).get('variants', {})
    save(checkpoint, out)
    if embedded is not None:
        out['vector_index'] = {**totals, 'complete': embedded.get('complete')}
    if binary not in PINNED:
        probe = case / 'probe.json'
        probe.write_text(json.dumps({'question': row['question'], 'max_tokens': 200, 'limit': 1}))
        PINNED[binary] = run(binary, store, config, 'call', 'context', '--arguments', probe, env=env).get('retrieval_defaults')
    out['binary_defaults'] = PINNED[binary]
    for name, knobs in variants.items():
        if name in out['variants']:
            continue
        knobs = dict(knobs)
        server = knobs.pop('server_defaults', False)
        if PINNED[binary] is not None and not server:
            knobs = {**RECIPE_OFF, **knobs}
        if knobs.pop('expansion', False):
            knobs['query_variants'] = expansions[row['question_id']]['variants']
        args = case / f'{name}.json'
        args.write_text(json.dumps({'question': row['question'], 'question_time': row['question_date'],
                                    'max_chars': knobs['max_tokens'] * 4, **knobs}))
        started = time.monotonic()
        packet = run(binary, store, config, 'call', 'context', '--arguments', args, env=env)
        seconds = time.monotonic() - started
        if packet.get('retrieval_defaults') and not server:
            raise RuntimeError(f'{name}: the binary defaulted {sorted(packet["retrieval_defaults"])} although the variant pins them')
        save(case / f'{name}.packet.json', packet)
        scored = coverage({'evidence': packet.get('evidence', [])}, docs, spans, mapping, row['answer_session_ids'])
        required = max(1, scored['annotated_sessions'])
        out['variants'][name] = {
            'session_recall': scored['annotated_sessions_touched'] / required,
            'all_sessions': scored['all_annotated_sessions_touched'],
            'turns': scored['annotated_answer_turns'], 'turns_complete': scored['complete_annotated_answer_turns'],
            'tokens': packet.get('evidence_tokens') or packet.get('tokens') or 0,
            'excerpts': len(packet.get('evidence', [])), 'seconds': round(seconds, 2),
            'local_model_calls': packet.get('model_calls', 0),
            'query_variants': len(knobs.get('query_variants') or []),
            'invalid_evidence_intervals': scored['invalid_evidence_intervals'],
            'ambiguous_unanchored_quotes': scored['ambiguous_unanchored_quotes'],
            'defaulted': sorted(packet.get('retrieval_defaults') or {}),
            **candidate_metrics(packet, docs, spans, mapping, row['answer_session_ids']),
        }
        save(checkpoint, out)
    return out


def summarize(cases, variants):
    """Headline denominators include every selected answerable case, including failures."""
    table = {}
    for name in variants:
        groups = defaultdict(list)
        abstentions = defaultdict(int)
        for case in cases:
            for kind in (case['type'], 'ALL'):
                if case.get('abstention') or not case.get('answer_sessions', 0):
                    abstentions[kind] += 1
                else:
                    groups[kind].append(case)
        table[name] = {}
        for kind in sorted(set(groups) | set(abstentions)):
            group = groups[kind]
            items = [case['variants'][name] for case in group if name in case.get('variants', {})]
            turns = sum(case.get('annotated_turns', 0) for case in group)
            n = len(group)
            row = {
                'n': n, 'completed': len(items), 'failed_or_incomplete': n - len(items),
                'abstentions_not_scored_as_retrieval_success': abstentions[kind],
                'session_recall': round(sum(i['session_recall'] for i in items) / n, 4) if n else None,
                'all_sessions': round(sum(i['all_sessions'] for i in items) / n, 4) if n else None,
                'turn_coverage': round(sum(i['turns_complete'] for i in items) / turns, 4) if turns else None,
                'tokens': round(statistics.mean(i['tokens'] for i in items)) if items else None,
                'seconds': round(statistics.mean(i['seconds'] for i in items), 2) if items else None,
                'conditional_session_recall': round(statistics.mean(i['session_recall'] for i in items), 4) if items else None,
                'invalid_evidence_intervals': sum(i.get('invalid_evidence_intervals', 0) for i in items),
                'ambiguous_unanchored_quotes': sum(i.get('ambiguous_unanchored_quotes', 0) for i in items),
            }
            with_candidates = [i for i in items if 'cand_turns_complete@100' in i]
            if with_candidates:
                row['candidate_diagnostics_completed'] = len(with_candidates)
                for k in (12, 24, 40, 100):
                    row[f'cand_session_recall@{k}'] = round(sum(i[f'cand_session_recall@{k}'] for i in with_candidates) / n, 4)
                ranks = [i['first_gold_rank'] for i in with_candidates if i.get('first_gold_rank')]
                row['first_gold_rank_median'] = statistics.median(ranks) if ranks else None
                row['first_gold_missing@100'] = sum(1 for i in with_candidates if not i.get('first_gold_rank'))
                candidate_turns = sum(i['cand_turns_complete@100'] for i in with_candidates)
                row['survival'] = round(sum(i['turns_complete'] for i in with_candidates) / candidate_turns, 4) if candidate_turns else None
            table[name][kind] = row
    return table


def paired(cases, a, b, seed=7, resamples=2000):
    """Exact McNemar test on all_sessions and a bootstrap CI of the session_recall
    difference (b − a), over questions both variants answered (genome's paired tests)."""
    import math
    import random
    both = [c['variants'] for c in cases if c.get('answer_sessions', 0) and not c.get('abstention')
            and a in c.get('variants', {}) and b in c.get('variants', {})]
    only_b = sum(1 for v in both if v[b]['all_sessions'] and not v[a]['all_sessions'])
    only_a = sum(1 for v in both if v[a]['all_sessions'] and not v[b]['all_sessions'])
    n = only_a + only_b
    tail = sum(math.comb(n, k) for k in range(0, min(only_a, only_b) + 1)) / 2 ** n if n else 1.0
    diffs = [v[b]['session_recall'] - v[a]['session_recall'] for v in both]
    rng = random.Random(seed)
    means = sorted(statistics.mean(rng.choice(diffs) for _ in diffs) for _ in range(resamples)) if diffs else [0.0]
    turn_a = sum(v[a]['turns_complete'] for v in both)
    turn_b = sum(v[b]['turns_complete'] for v in both)
    return {'pair': [a, b], 'scope': 'complete paired answerable cases only', 'excluded': len(cases) - len(both), 'n': len(both), 'all_sessions_only_a': only_a, 'all_sessions_only_b': only_b,
            'mcnemar_p': round(min(1.0, 2 * tail), 4),
            'session_recall_diff': round(statistics.mean(diffs), 4) if diffs else 0.0,
            'session_recall_diff_ci95': [round(means[int(0.025 * len(means))], 4), round(means[int(0.975 * len(means)) - 1], 4)],
            'turns_complete_a': turn_a, 'turns_complete_b': turn_b}


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('--binary', type=Path, default=ROOT / 'target/debug/graphicle')
    p.add_argument('--dataset', type=Path, default=DATASET)
    p.add_argument('--per-type', type=int, default=10, help='questions per question type (0: all)')
    p.add_argument('--offset-per-type', type=int, default=0,
                   help='skip the first N questions of each type (e.g. 20 to hold out the dev split)')
    p.add_argument('--variants', default='default,deeper,deeper-wider,no-expansion')
    p.add_argument('--reranker', type=Path, default=ROOT / '.models/ms-marco-MiniLM-L6-v2',
                   help='local ONNX cross-encoder directory for the rerank variant')
    p.add_argument('--ort', type=Path, help='onnxruntime dynamic library (ORT_DYLIB_PATH) for the rerank variant')
    p.add_argument('--embedder', type=Path,
                   help='local ONNX embedding model directory (MODEL.json); builds the vector index after sync '
                        'so the dense variants measure the arm answers use')
    p.add_argument('--paired', action='append', default=[],
                   help='A,B: exact McNemar on all_sessions and a bootstrap CI of the session_recall difference')
    p.add_argument('--include-abs', action='store_true',
                   help='include abstention questions (their labels are used for scoring only)')
    p.add_argument('--concurrency', type=int, default=4)
    p.add_argument('--out', type=Path, required=True)
    p.add_argument('--freeze-only', action='store_true', help='freeze inputs and stop before outcomes')
    p.add_argument('--resume', action='store_true', help='continue a frozen run; never retry recorded failures')
    p.add_argument('--keep-stores', action='store_true', help='retain completed SQLite stores as well as packets')
    p.add_argument('--embedding-rounds', type=int, default=128,
                   help='maximum local-only indexing jobs per case, each capped at 32 embedding calls')
    p.add_argument('--query-variants', type=Path,
                   help='tools/expand_queries.py output: the precomputed variants mq- variants pass to context')
    p.add_argument('--stable-names', action='store_true',
                   help='name session files by a hash of the session id instead of position, so '
                        'GRAPHICLE_EMBEDDING_CACHE can reuse passage vectors across questions; compare '
                        'only against baselines run with the same naming')
    a = p.parse_args()
    if a.per_type < 0 or a.offset_per_type < 0 or a.concurrency < 1:
        p.error('selection counts must be nonnegative and concurrency positive')
    if not 1 <= a.embedding_rounds <= 1024:
        p.error('--embedding-rounds must be between 1 and 1024')
    names = [v for v in a.variants.split(',') if v]
    if not names or len(names) != len(set(names)):
        p.error('select at least one variant without duplicates')
    unknown = [v for v in names if v not in VARIANTS]
    if unknown:
        p.error(f'unknown variants: {unknown}; choose from {sorted(VARIANTS)}')
    variants = {v: VARIANTS[v] for v in names}
    pairs = [tuple(x.split(',')) for x in a.paired]
    if any(len(x) != 2 or not set(x) <= set(names) for x in pairs):
        p.error('--paired takes A,B with both variants selected')
    dense = any('local_embedding_profile' in knobs for knobs in variants.values())
    if dense and not a.embedder:
        p.error('dense variants need --embedder')
    expansions = None
    if any(knobs.get('expansion') for knobs in variants.values()):
        if not a.query_variants:
            p.error('mq- variants need --query-variants')
        if not a.query_variants.is_file():
            p.error(f'--query-variants {a.query_variants} does not exist')
        expansions = json.loads(a.query_variants.read_text())
        if expansions.get('contract') != 'graphicle.query-expansion.v1':
            p.error('--query-variants is not a tools/expand_queries.py output')
        expansions = expansions['items']
    with a.dataset.open() as source:
        rows = json.load(source)
    ids = [row['question_id'] for row in rows]
    if len(ids) != len(set(ids)) or any(not qid or qid in {'.', '..'} or
            any(not (c.isalnum() or c in '-_.') for c in qid) for qid in ids):
        p.error('question IDs must be unique safe directory names')
    order = lambda r: hashlib.sha256(f"{SEED}:{r['question_id']}".encode()).hexdigest()
    eligible = [r for r in sorted(rows, key=order) if a.include_abs or not r['question_id'].endswith('_abs')]
    if a.offset_per_type:
        seen = defaultdict(int)
        kept = []
        for r in eligible:
            seen[r['question_type']] += 1
            if seen[r['question_type']] > a.offset_per_type:
                kept.append(r)
        eligible = kept
    if a.per_type:
        taken = defaultdict(int)
        chosen = []
        for r in eligible:
            if taken[r['question_type']] < a.per_type:
                taken[r['question_type']] += 1
                chosen.append(r)
        eligible = chosen
    if expansions is not None:
        missing = [r['question_id'] for r in eligible if 'variants' not in expansions.get(r['question_id'], {})]
        if missing:
            p.error(f'--query-variants lacks {len(missing)} selected questions, e.g. {missing[:3]}')
    out = a.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    config = out / 'models.json'
    models = {'models': {}}
    env = dict(os.environ)
    ort = a.ort or next(iter(sorted(ROOT.glob('.qualification-venv/lib/python3*/site-packages/onnxruntime/capi/libonnxruntime*.dylib'))), None)
    if ort:
        env['ORT_DYLIB_PATH'] = str(Path(ort).resolve())
    if any('local_reranker_profile' in knobs for knobs in variants.values()):
        manifest = json.loads((a.reranker / 'MODEL.json').read_text())
        models['models']['reranker'] = {'protocol': 'local_onnx', 'base_url': a.reranker.resolve().as_uri() + '/',
                                        'model': manifest['model'], 'input_limit': 1000}
    if dense:
        manifest = json.loads((a.embedder / 'MODEL.json').read_text())
        models['models']['embedding'] = {'protocol': 'local_onnx', 'base_url': a.embedder.resolve().as_uri() + '/',
                                         'model': manifest['model'], 'input_limit': 8000}
        models['embedding_model'] = 'embedding'
    config_bytes = (json.dumps(models, sort_keys=True) + '\n').encode()
    local_artifacts = {}
    for directory in (([a.reranker] if any('local_reranker_profile' in k for k in variants.values()) else [])
                      + ([a.embedder] if dense else [])):
        for path in sorted(directory.rglob('*')):
            if path.is_file():
                local_artifacts[str(path.resolve())] = digest(path)
    if ort and (dense or any('local_reranker_profile' in knobs for knobs in variants.values())):
        local_artifacts[str(Path(ort).resolve())] = digest(ort)
    inputs = {'binary_sha256': digest(a.binary), 'dataset_sha256': digest(a.dataset),
              'runner_sha256': digest(__file__),
              'scorer_sha256': digest(Path(__file__).with_name('qualify_longmemeval.py')),
              'configuration_sha256': hashlib.sha256(config_bytes).hexdigest(),
              'local_artifacts': local_artifacts, 'variants': variants,
              'selection': {'seed': SEED, 'per_type': a.per_type, 'offset_per_type': a.offset_per_type,
                            'abstention_excluded': not a.include_abs},
              'questions': [{'id': row['question_id'], 'type': row['question_type']} for row in eligible],
              'pairs': pairs, 'keep_stores': a.keep_stores, 'embedding_rounds': a.embedding_rounds,
              'concurrency': max(1, min(32, a.concurrency))}
    if a.stable_names:
        inputs['session_names'] = 'sha256(session_id)[:16]'
    if expansions is not None:
        inputs['query_variants_sha256'] = digest(a.query_variants)
    # JSON round trip makes tuple-valued command line options comparable on resume.
    inputs = json.loads(json.dumps(inputs))
    protocol_file = out / 'PROTOCOL.json'
    binary = out / 'graphicle'
    if protocol_file.exists():
        if not a.resume:
            p.error('run already frozen; use --resume with identical inputs or a new output directory')
        protocol = json.loads(protocol_file.read_text())
        if protocol['inputs'] != inputs or digest(binary) != inputs['binary_sha256'] or digest(config) != inputs['configuration_sha256']:
            raise ValueError('Frozen input changed; create a new run instead of mixing results')
    else:
        if a.resume:
            p.error('no frozen protocol exists to resume')
        shutil.copy2(a.binary, binary)
        config.write_bytes(config_bytes)
        protocol = {'contract': 'graphicle.retrieval-recall-protocol.v2', 'inputs': inputs,
                    'frozen_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                    'remote_model_calls': 0, 'local_inference': bool(local_artifacts),
                    'cpu_only': True, 'answer_accuracy_measured': False, 'leaderboard_claim': False,
                    'public_dataset': True, 'holdout_claim': False,
                    'ingestion': 'Original role/content/date only. Questions and gold labels are not ingested.',
                    'failure_policy': 'No automatic retries. Failures remain in headline denominators.',
                    'abstention_policy': 'No support labels is not a retrieval success or an abstention prediction.'}
        save(protocol_file, protocol)
    if a.freeze_only:
        print(json.dumps({'frozen': len(eligible), 'protocol': str(protocol_file)}))
        return 0
    env.update(CUDA_VISIBLE_DEVICES='', NVIDIA_VISIBLE_DEVICES='void')
    workdir = out / 'cases'
    workdir.mkdir(exist_ok=True)
    started = time.monotonic()

    def attempt(row):
        case = workdir / row['question_id']
        checkpoint = case / 'RESULT.json'
        if checkpoint.exists():
            saved = json.loads(checkpoint.read_text())
            if saved.get('complete') or 'error' in saved:
                return saved
        try:
            result = evaluate(row, variants, binary, config, workdir, env, embed=dense, embedding_rounds=a.embedding_rounds,
                              expansions=expansions, stable_names=a.stable_names)
            result['complete'] = True
        except Exception as error:
            result = json.loads(checkpoint.read_text()) if checkpoint.exists() else {}
            result.update(question_id=row['question_id'], type=row['question_type'],
                          abstention=row['question_id'].endswith('_abs'),
                          answer_sessions=len(set(row['answer_session_ids'])),
                          annotated_turns=sum(turn.get('has_answer') is True for session in row['haystack_sessions'] for turn in session),
                          complete=False, error=str(error)[:600])
        save(checkpoint, result)
        if result.get('complete') and not a.keep_stores:
            shutil.rmtree(case / 'store', ignore_errors=True)
            shutil.rmtree(case / 'sessions', ignore_errors=True)
        print(json.dumps({'question_id': row['question_id'], 'complete': result.get('complete'),
                          'error': result.get('error')}), flush=True)
        return result

    with ThreadPoolExecutor(max_workers=inputs['concurrency']) as pool:
        cases = list(pool.map(attempt, eligible))
    result = {'contract': 'graphicle.retrieval-recall.v2', 'seconds_this_invocation': round(time.monotonic() - started, 1),
              'protocol_sha256': digest(protocol_file), 'binary_sha256': inputs['binary_sha256'],
              'dataset_sha256': inputs['dataset_sha256'], 'selection': inputs['selection'],
              'embedder': str(a.embedder) if dense else None,
              'embedding_cache': bool(dense and env.get('GRAPHICLE_EMBEDDING_CACHE')),
              'remote_model_calls': 0, 'local_inference': bool(local_artifacts),
              'model_calls': None if local_artifacts else 0, 'answer_accuracy_measured': False,
              'variants': variants, 'errors': [case for case in cases if 'error' in case],
              'summary': summarize(cases, variants), 'paired': [paired(cases, x, y) for x, y in pairs],
              'cases': cases}
    save(out / 'RECALL.json', result)
    for name, table in result['summary'].items():
        print(name, json.dumps(table.get('ALL')))
    for test in result['paired']:
        print('paired', json.dumps(test))
    return 1 if result['errors'] else 0


if __name__ == '__main__':
    sys.exit(main())
