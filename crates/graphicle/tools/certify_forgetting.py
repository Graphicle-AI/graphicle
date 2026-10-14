#!/usr/bin/env python3
"""Certify that forgetting documents leaves a Graphicle store equal to one
that never ingested them, on public LongMemEval histories.

For each case (one question and its history of chat sessions), the sessions
become connector documents and the documents to forget are the sessions that
hold the answer:
1. Build the prefix: import the sessions before the first answer session and
   do ordinary work (embed, observe, draft memories, retrieve, answer). Fork
   the store.
2. World A imports the remaining sessions including the answer sessions;
   world B imports the same sessions without them. Both do the same work.
3. World A forgets the answer sessions (`source.forget`) and reruns the work.
4. The certificate records, per case, whether A and B have the same state
   digest (graphicle.state.v2, which leaves out tables with no rows in the
   project), the same retrieval packets and answers, and whether the answer
   text is still visible to retrieval.

The model is a deterministic local stub (same request, same reply), which
makes recomputation reproducible; no request leaves the machine and nothing is
paid for. It certifies the store's data flow, not answer quality.
"""
import argparse
import hashlib
import http.server
import json
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DATASET = ROOT / 'qualification/0.4/datasets/longmemeval_s_cleaned.json'
CONTRACT = 'graphicle.forgetting-certificate.v1'
STATE_CONTRACT = 'graphicle.state.v2'
SEED = 'graphicle-certified-forgetting-v1'
IDS = re.compile(r'\b[0-9a-f]{32}\b')
CLOCK = re.compile(r'\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z')


def sha(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as f:
        for block in iter(lambda: f.read(1 << 20), b''):
            digest.update(block)
    return digest.hexdigest()


def history(row):
    """A LongMemEval row's sessions as dated documents, as
    tools/qualify_longmemeval.py writes them: `session-NNNN.txt`, a
    `Conversation date:` header, then one `role: content` paragraph per turn.
    Returns the documents, the byte spans of turns labelled `has_answer` and
    the file id to session id map. Kept here, so this tool (and the SDK test
    that imports its stub) needs no sibling module."""
    docs, spans, mapping = {}, {}, {}
    for i, (date, sid, turns) in enumerate(zip(row['haystack_dates'], row['haystack_session_ids'],
                                               row['haystack_sessions'], strict=True)):
        name = f'session-{i:04d}.txt'
        text = f'Conversation date: {date}\n\n'
        support = []
        for turn in turns:
            role, content, start = turn['role'], turn['content'], len(text)
            text += f'{role}: {content}\n\n'
            if turn.get('has_answer') is True:
                support.append({'start': start + len(role) + 2, 'end': start + len(role) + 2 + len(content)})
        docs[name] = text
        spans['file:' + name] = support
        mapping['file:' + name] = sid
    return docs, spans, mapping


def user_lines(text, limit):
    lines = [line[len('user: '):].strip() for line in text.splitlines() if line.startswith('user: ')]
    return [line[:300] for line in lines if len(line) >= 3][:limit]


class Stub(http.server.BaseHTTPRequestHandler):
    """Deterministic stand-in for the answer, observer and embedding models."""

    def log_message(self, *args):
        pass

    def reply(self, value):
        body = json.dumps(value).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        if self.path.endswith('/embeddings'):
            vectors = []
            for text in request['input']:
                vector = [1e-3] * 64
                for word in re.findall(r'[a-z0-9]+', text.lower()):
                    vector[int(hashlib.sha256(word.encode()).hexdigest()[:8], 16) % 64] += 1.0
                vectors.append(vector)
            return self.reply({'data': [{'index': i, 'embedding': v} for i, v in enumerate(vectors)], 'usage': {'prompt_tokens': 1}})
        payload = json.loads(request['input'])
        if 'session' in payload:
            result = {'notes': [{'text': f'User said: {line}', 'quotes': [line]} for line in user_lines(payload['session'], 40)]}
        elif 'primary_excerpt' in payload:
            # Memory drafting (`extract`): one claim per user line of the excerpt, quoting it.
            result = {'claims': [{'statement': line, 'subject': 'user', 'relation': 'source_observation', 'object': line,
                                  'speaker': 'user', 'modality': 'report', 'conditions': [], 'action': 'ADD',
                                  'target_id': None, 'valid_at': None, 'invalid_at': None, 'time_basis': 'unknown',
                                  'anchor_id': None, 'evidence': [{'id': payload['primary_source_id'], 'quote': line}]}
                                 for line in user_lines(payload['primary_excerpt'], 8)]}
        else:
            sources = payload.get('sources') or []
            result = ({'answer': sources[0]['text'][:200], 'abstain': False, 'unresolved': [],
                       'evidence': [{'id': sources[0]['id'], 'quote': sources[0]['text']}]} if sources else
                      {'answer': '', 'abstain': True, 'unresolved': ['no sources'], 'evidence': []})
            if 'reading_notes' in request.get('instructions', ''):  # grounded-v3 reads before it answers
                result = {'reading_notes': [{'source': src['id'], 'date': src.get('date') or '', 'note': src['text'][:120]}
                                            for src in sources[:4]], 'premise_holds': True, **result}
        self.reply({'status': 'completed', 'model': 'stub', 'usage': {'input_tokens': 1, 'output_tokens': 1},
                    'output': [{'type': 'message', 'content': [{'type': 'output_text', 'text': json.dumps(result)}]}]})


class World:
    """One store driven through the real CLI."""

    def __init__(self, binary, config, store, extract=True):
        self.binary, self.config, self.store, self.extract = binary, config, store, extract

    def cli(self, *args):
        proc = subprocess.run([str(self.binary), '--data-dir', str(self.store), '--config', str(self.config), *map(str, args)],
                              capture_output=True, text=True, timeout=900)
        try:
            value = json.loads(proc.stdout)
        except ValueError:
            value = None
        if proc.returncode or value is None:
            recorded = value.get('error') if isinstance(value, dict) else None
            detail = f'{recorded} ({proc.stderr.strip()[-300:]})' if recorded else (proc.stderr[-600:] or proc.stdout[-600:])
            raise RuntimeError(f'{args[0]} {args[1] if len(args) > 1 else ""}: {detail}')
        return value

    def arguments(self, name, value):
        path = self.store.parent / f'{self.store.name}-{name}.json'
        path.write_text(json.dumps(value))
        return path

    def call(self, operation, **args):
        return self.cli('call', operation, '--arguments', self.arguments(operation, args))

    def run(self, kind, **args):
        job = self.cli('run', kind, '--arguments', self.arguments(kind, args), '--max-calls', args['max_calls'], '--wait')
        if job.get('state') != 'complete':
            raise RuntimeError(f'{kind} failed: {job.get("error")}')
        return job['result']

    def until_complete(self, kind, rounds=60, **args):
        for _ in range(rounds):
            if self.run(kind, **args)['complete']:
                return
        raise RuntimeError(f'{kind} did not complete')

    def work(self, connector, questions):
        """Ordinary work: index, observe, draft memories (`extract`), retrieve and answer."""
        self.until_complete('embed', max_calls=32, max_input_tokens=512000, max_output_tokens=64)
        self.until_complete('observer_notes', include=[f'connector:{connector}:'], max_calls=32,
                            max_input_tokens=512000, max_output_tokens=64000)
        if self.extract:
            self.until_complete('extract', max_calls=32, max_input_tokens=512000, max_output_tokens=64000)
        packets, answers = [], []
        for question in questions:
            packets.append(self.call('context', question=question, max_tokens=2000))
            answers.append(self.run('basic', question=question, max_calls=2, context_tokens=4000)['answer'])
        return packets, answers

    def state(self):
        return self.call('deletion.state')

    def fork(self, target):
        shutil.copytree(self.store, target)
        return World(self.binary, self.config, target, self.extract)


def masked(packet):
    """Per-store ids, clocks and snapshot keys (digests over per-store memory
    ids) differ by construction; everything they cover is compared."""
    value = json.loads(CLOCK.sub('<time>', IDS.sub('<id>', json.dumps(packet, sort_keys=True))))
    for key in ('source_state', 'request_hash'):
        value.pop(key, None)
    return value


def visible(packets, answer):
    needle = str(answer).strip().lower()
    return bool(needle) and any(needle in json.dumps(p).lower() for p in packets)


def certify_case(row, binary, config, workdir, extract=True):
    docs, _, mapping = history(row)
    names = list(docs)
    target = {name for name in names if mapping['file:' + name] in set(row['answer_session_ids'])}
    first = min(names.index(n) for n in target)
    prefix = [n for n in names[:first]]
    rest = [n for n in names[first:] if n not in target]
    as_docs = lambda chosen: [{'id': n, 'title': n, 'content': docs[n]} for n in chosen]
    questions = [row['question'], 'What did I say about my plans?']
    case = workdir / row['question_id']
    folder = case / 'readme'
    folder.mkdir(parents=True)
    (folder / 'README.md').write_text('# Chat history\n\nuser: This project holds imported chat sessions.\n')
    a = World(binary, config, case / 'a', extract)
    a.cli('sync', folder)
    connector = a.call('connector.add', kind='jsonl', name='Chat',
                       config={'acl': {'workspace_readers': True, 'credentials': []}})['id']
    if prefix:
        a.call('connector.import', connector_id=connector, documents=as_docs(prefix))
    a.work(connector, questions)
    b = a.fork(case / 'b')
    a.call('connector.import', connector_id=connector, documents=as_docs(rest + sorted(target)))
    if rest:
        b.call('connector.import', connector_id=connector, documents=as_docs(rest))
    seen, _ = a.work(connector, questions)
    b_packets, b_answers = b.work(connector, questions)
    before = a.state()['digest'] != b.state()['digest']
    started = time.monotonic()
    forgotten = a.call('source.forget', connector_id=connector, document_ids=sorted(target))
    forget_seconds = time.monotonic() - started
    receipt = forgotten['receipt']
    a_packets, a_answers = a.work(connector, questions)
    state_a, state_b = a.state(), b.state()
    for state in (state_a, state_b):
        if state.get('contract') != STATE_CONTRACT:
            raise RuntimeError(f"expected {STATE_CONTRACT} from the binary, got {state.get('contract')}")
    # v2 lists only tables with rows, so a table present in one state only
    # differs from its absence in the other.
    differing = sorted(t for t in set(state_a['tables']) | set(state_b['tables'])
                       if state_a['tables'].get(t) != state_b['tables'].get(t))
    reimport = a.call('connector.import', connector_id=connector, documents=as_docs(sorted(target)))
    verified = a.call('deletion.verify', receipt_id=receipt['id'])
    removed = receipt['removed']
    answer = str(row['answer']).strip().lower()
    return {
        'question_id': row['question_id'], 'type': row['question_type'], 'sessions': len(names),
        'forgotten_sessions': len(target), 'forgotten_bytes': sum(len(docs[n].encode()) for n in target),
        'documents_made_a_difference': before,
        'removed_rows': sum(removed['rows'].values()) + sum(removed['since_visible'].values()),
        'state_equal': state_a['digest'] == state_b['digest'], 'differing_tables': differing,
        'state_tables': len(state_a['tables']), 'state_rows': sum(t['rows'] for t in state_a['tables'].values()),
        'state_rows_by_table': {t: v['rows'] for t, v in sorted(state_a['tables'].items()) if v['rows']},
        # Whether the answer is a literal span of the forgotten or of the kept
        # sessions; visibility probes only mean something for literal answers.
        'answer_in_forgotten_text': bool(answer) and any(answer in docs[n].lower() for n in target),
        'answer_in_kept_text': bool(answer) and any(answer in docs[n].lower() for n in names if n not in target),
        'probes_equal': [masked(p) for p in a_packets] == [masked(p) for p in b_packets],
        'answers_equal': a_answers == b_answers,
        'receipt_verified_local': receipt['verified_local'], 'receipt_digest': receipt['digest'],
        'residue_matches': receipt['residue']['source_ids']['matches'] + receipt['residue']['verbatim_text']['matches'],
        'recomputed_on_demand': receipt['recomputed_on_demand'], 'forget_seconds': round(forget_seconds, 3),
        'answer_visible_before': visible(seen, row['answer']), 'answer_visible_after': visible(a_packets, row['answer']),
        'answer_visible_never_ingested': visible(b_packets, row['answer']),
        'reimport_skipped': reimport['forgotten_skipped'], 're_verified': verified['verified_local'],
        'state_digest': state_a['digest'], 'state_contract': state_a['contract'],
    }


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('--binary', type=Path, default=ROOT / 'target/debug/graphicle')
    p.add_argument('--dataset', type=Path, default=DATASET)
    p.add_argument('--cases', type=int, default=20)
    p.add_argument('--concurrency', type=int, default=4)
    p.add_argument('--out', type=Path, required=True, help='directory for CERTIFICATE.json and the stores')
    p.add_argument('--keep', action='store_true', help='keep the per-case stores for inspection')
    p.add_argument('--skip-extract', action='store_true', help='leave memory drafting (`extract`) out of the work')
    p.add_argument('--question-ids', nargs='+', help='certify only these questions (for reruns)')
    a = p.parse_args()
    rows = json.loads(a.dataset.read_text())
    order = lambda r: hashlib.sha256(f"{SEED}:{r['question_id']}".encode()).hexdigest()
    eligible = [r for r in sorted(rows, key=order) if not r['question_id'].endswith('_abs')
                and set(r['answer_session_ids']) & set(r['haystack_session_ids'])
                and (not a.question_ids or r['question_id'] in a.question_ids)][:a.cases]
    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Stub)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = f'http://127.0.0.1:{server.server_address[1]}/v1'
    out = a.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    config = out / 'models.json'
    config.write_text(json.dumps({'default_model': 'stub', 'embedding_model': 'embed', 'models': {
        'stub': {'protocol': 'responses', 'base_url': base, 'model': 'stub', 'input_limit': 32000, 'output_limit': 4000},
        'embed': {'protocol': 'chat', 'base_url': base, 'model': 'embed', 'input_limit': 8000, 'output_limit': 64}}}))
    workdir = Path(tempfile.mkdtemp(prefix='certify-', dir=out))
    # Run a private copy, so a rebuild during the run cannot change the binary
    # the certificate names.
    binary = workdir / 'graphicle'
    shutil.copy2(a.binary, binary)
    binary_sha = sha(binary)
    extract = not a.skip_extract
    started = time.monotonic()

    def attempt(row):
        try:
            return certify_case(row, binary, config, workdir, extract)
        except Exception as error:
            return {'question_id': row['question_id'], 'error': str(error)[:800]}

    with ThreadPoolExecutor(max_workers=max(1, min(8, a.concurrency))) as pool:
        cases = list(pool.map(attempt, eligible))
    server.shutdown()
    if not a.keep:
        shutil.rmtree(workdir, ignore_errors=True)
    done = [c for c in cases if 'error' not in c]
    count = lambda key: sum(bool(c.get(key)) for c in done)
    literal = [c for c in done if c['answer_in_forgotten_text'] and not c['answer_in_kept_text']]
    certificate = {
        'contract': CONTRACT, 'created_unix': time.time(), 'seconds': round(time.monotonic() - started, 1),
        'binary_sha256': binary_sha, 'state_contract': STATE_CONTRACT,
        'dataset': str(a.dataset), 'dataset_sha256': sha(a.dataset),
        'provider': 'deterministic local stub (same request, same reply); no network, no paid calls',
        'protocol': 'fork before the answer sessions arrive; A ingests them, works, forgets them, reruns the work; B never ingests them and does the same work',
        'work': ['embed', 'observer_notes'] + (['extract (memory drafts)'] if extract else []) + ['context', 'basic answer'],
        'summary': {'cases': len(cases), 'errors': len(cases) - len(done), 'documents_made_a_difference': count('documents_made_a_difference'),
                    'state_equal': count('state_equal'), 'probes_equal': count('probes_equal'), 'answers_equal': count('answers_equal'),
                    'receipt_verified_local': count('receipt_verified_local'), 're_verified': count('re_verified'),
                    'forgotten_sessions': sum(c.get('forgotten_sessions', 0) for c in done),
                    'sessions': sum(c.get('sessions', 0) for c in done),
                    'removed_rows': sum(c.get('removed_rows', 0) for c in done),
                    'answer_visible_before': count('answer_visible_before'), 'answer_visible_after': count('answer_visible_after'),
                    'answer_visible_never_ingested': count('answer_visible_never_ingested'),
                    # Cases whose answer is a literal span found only in the forgotten sessions.
                    'literal_answer_only_in_forgotten': len(literal),
                    'literal_visible_before': sum(c['answer_visible_before'] for c in literal),
                    'literal_visible_after': sum(c['answer_visible_after'] for c in literal),
                    'reimport_skipped': sum(c.get('reimport_skipped', 0) for c in done)},
        'cases': cases,
    }
    (out / 'CERTIFICATE.json').write_text(json.dumps(certificate, indent=2) + '\n')
    print(json.dumps(certificate['summary'], indent=2))
    return 0 if done and len(done) == len(cases) and all(c['state_equal'] and c['probes_equal'] and c['answers_equal'] for c in done) else 1


if __name__ == '__main__':
    sys.exit(main())
