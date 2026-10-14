# Optional local retrieval models

The ordinary lexical and structural retrieval path needs no model. The
`local-inference` Cargo feature adds CPU ONNX embedding and cross-encoder
reranking. It is off by default, and a build with it reranks only when a call
names a reranker profile. In development runs a cross-encoder reranking
lexical and dense candidates raised evidence recall, at a large CPU cost, but
it failed a held-out check on single-conversation sources, so leave it off for
them (see [Reranking one conversation's history](#reranking-one-conversations-history)).
On code questions it did not show a consistent evidence improvement, so leave
reranking off for code too.

Build from this crate's directory:

```sh
python3 tools/prepare_parser_sources.py   # once per checkout (README.md)
cargo build --locked --release --features local-inference
```

Provide an existing ONNX Runtime shared library with `ORT_DYLIB_PATH`. Each
model directory needs `model.onnx`, `tokenizer.json` and `MODEL.json`. The
manifest names the model, maximum sequence length, dimension, pooling strategy
and SHA-256 digests under `files`. The loader checks the weight and tokenizer
digests. It never downloads a model or library.

An embedding manifest may also set `query_prefix` and `passage_prefix`, for
models trained with asymmetric instructions (for example `"query: "` and
`"passage: "` for E5). Questions get the query prefix; indexed documents get the
passage prefix. Both default to empty, are limited to 512 bytes and are rejected
on a cross-encoder manifest. Because the manifest digest is part of the model
identity, changing a prefix invalidates stored vectors instead of mixing them.

The code questions used `sentence-transformers/all-MiniLM-L6-v2`, revision
`1110a243fdf4706b3f48f1d95db1a4f5529b4d41`, with 384-dimensional masked mean
pooling and L2 normalization, and the reranker
`cross-encoder/ms-marco-MiniLM-L6-v2`, revision
`233902d25c440f23af6f7d6e94d2946bac0bee0a`, with a single output logit and a
256-token input limit. Both used the ARM64 int8 ONNX artifact.

The conversation-history runs used each model's int8
`onnx/model_quantized.onnx` on x86-64 with a 512-token input limit:

| Role | Model | Revision | License |
|---|---|---|---|
| Embedder | `Snowflake/snowflake-arctic-embed-s` | `e596f507467533e48a2e17c007f0e1dacc837b33` | Apache-2.0 |
| Embedder | `Xenova/bge-small-en-v1.5` | `ea104dacec62c0de699686887e3f920caeb4f3e3` | MIT |
| Reranker | `mixedbread-ai/mxbai-rerank-xsmall-v1` | `b5c6e9da73abc3711f593f705371cdbe9e0fe422` | Apache-2.0 |
| Reranker | `mixedbread-ai/mxbai-rerank-base-v1` | `800f24c113213a187e65bde9db00c15a2bb12738` | Apache-2.0 |

Both embedders use the first token's output with L2 normalization (384
dimensions) and the query prefix `"Represent this sentence for searching
relevant passages: "`. The rerankers output a single logit. Other
architectures need an appropriate artifact and a matching manifest; this is
not a universal ONNX model adapter.

Configure named profiles in the existing model config, using absolute directory
URLs. Replace the example asset paths with your local directories:

```json
{
  "embedding_model": "local-embed",
  "models": {
    "local-embed": {
      "protocol": "local_onnx",
      "base_url": "file:///absolute/path/to/embedder/",
      "model": "sentence-transformers/all-MiniLM-L6-v2",
      "input_limit": 1000
    },
    "local-rerank": {
      "protocol": "local_onnx",
      "base_url": "file:///absolute/path/to/reranker/",
      "model": "cross-encoder/ms-marco-MiniLM-L6-v2",
      "input_limit": 1000
    }
  }
}
```

Run the existing `embed` background job to index documents. Model identity
includes the manifest digest, so a changed model does not silently reuse old
vectors. The embedder runs each text on its own, never in a batch. The
bundled int8 models quantize a whole batch tensor with one scale, so in a
batch a passage's vector depended on the passages beside it: on BEAM-100K,
every one of 489 passage vectors moved, question-passage scores by up to
3.8e-2, and every one of 40 `context` packets changed, on macOS and on Linux.
Stored vectors, the shared embedding cache, note vectors and question vectors
all come from single-text runs. Vectors from earlier releases came from
batched runs; the model identity changed with this, so an index built before is
reported unavailable until `embed` runs again. The same model still gives
slightly different vectors on different platforms, so build an index and query
it on the same platform. Querying does not trigger indexing. Add either or both
`local_embedding_profile: "local-embed"` and
`local_reranker_profile: "local-rerank"` to `context` arguments or the
MCP context tool. An unknown profile is rejected.

Dense retrieval uses only current vectors visible to the caller. Reranking scores
the first 32 fused source candidates by default (`rerank_depth`, at most 300) and
happens before the existing token/character packing limit. Past the 60 passages
that ranking selects, the cross-encoder reads on down the fused order. It skips
text it has already scored and origins already at their passage cap, so a deep
read scores each distinct passage once. Reviewed memories retain their separate
grounding rules.

With a reranker profile, `rerank_mode` sets how the cross-encoder order is
used. `blend` (the default) weighs its min-max score equally with the fused
rank. `replace` sorts by the cross-encoder alone. `rrf` fuses the two orders
0.7/0.3, after SmartSearch (arXiv 2603.15599). The cross-encoder reads each
passage up to the model's input limit, so evidence past it is not scored.

Responses report model calls, input tokens, candidates and elapsed time; unavailable or
stale vector sets remain explicit. Revocation removes documents from both model
paths, and an empty authorized candidate set makes no model call.

The implementation keeps at most three loaded sessions, serializes access per
session, limits CPU threads and text/batch sizes, and validates output shape and
finite values. The runtime's tests check the call structure of the model path
with a stand-in model on every run; the test with real model files is marked
`#[ignore]` and needs local model files.

HTTP command and MCP requests run their synchronous graph, SQLite and local
model work on Tokio's blocking pool. They do not occupy the async HTTP worker
while waiting for a database transaction. This preserves unrelated request
responsiveness; it does not remove writer serialization or provide a latency
SLA. Local MCP stdio continues to process one request at a time.

## Reranking one conversation's history

A single-conversation source is a project whose sources are the dated sessions
of one ongoing conversation between the same participants: a person's history
with an assistant, an agent's transcripts, or a dialogue between two people.
BEAM conversations, LongMemEval histories and LoCoMo conversations are all of
this kind; a code repository or a collection of separate documents is not.

Leave the cross-encoder off for these sources. Its held-out check, on
conversations that were not used for tuning, had a pass rule fixed before it
ran: a reranked recipe had to raise mean evidence recall by a set margin, with
a confidence interval above zero. Neither reranked recipe passed, and a
reranked question took many times as long on a server CPU. The same check
settled the dense weight: weight 0.5 tied stemmed BM25 alone, which is why the
default is 0.5 rather than 2.

LoCoMo counts as single-conversation: each of its ten samples is one
conversation between two people, and the benchmark harnesses give each its
own store.

## Local OpenAI-compatible servers

Model profiles can point at a local OpenAI-compatible server, such as vLLM or
llama.cpp's server.

A profile's `base_url` uses HTTPS, or HTTP on loopback. A model server elsewhere
on a private network that serves no TLS, such as a vLLM host, is reachable over
plain HTTP only after whoever runs the service lists its host in
`GRAPHICLE_PROVIDER_HTTP_HOSTS`: exact host names or IP addresses, separated by
commas or spaces, without scheme, port or path (IPv6 in brackets). A listed
host matches only itself, not a subdomain or a neighbouring address, and an
entry that is not exactly a host is an error. A profile with a credential
(`key_env`) is refused on such a host, because the key would cross the network
in clear text. Connectors, sign-in and release feeds ignore the variable.

```sh
GRAPHICLE_PROVIDER_HTTP_HOSTS=192.168.210.33 graphicle serve
```

```json
{"protocol": "chat", "base_url": "http://192.168.210.33:8000/v1", "model": "Qwen/Qwen3-8B"}
```

Sampling fields that the chat-completions API does not define travel in a
profile's `extra_body` object, for example `top_k`, `seed`, and the
`chat_template_kwargs` that turn Qwen's thinking on or off. Graphicle merges
`extra_body` into each request for `chat` and `azure_chat` profiles. It
accepts at most 16 fields and 4 KiB, and refuses fields Graphicle sets itself,
such as the model, messages, token limits, `response_format`, temperature,
streaming and tools. A profile's identity includes `extra_body` only when it is
set, so profiles without it keep their cached outputs.

A thinking model needs a server that returns its reasoning outside the message
content: with vLLM and Qwen, keep `--reasoning-parser qwen3`, so only the
answer is constrained to the JSON schema. If a server cannot combine reasoning
with structured outputs, set `"structured_output": false` on the profile. The
schema then goes into the prompt, and Graphicle drops one leading
`<think>…</think>` block before parsing the reply.
