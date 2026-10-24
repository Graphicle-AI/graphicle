import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import {inspect} from "node:util";
import {Graphicle, GraphicleError} from "../index.js";

async function server(t, handler) {
  const instance = http.createServer(handler);
  await new Promise(resolve => instance.listen(0, "127.0.0.1", resolve));
  t.after(() => { instance.closeAllConnections(); return new Promise(resolve => instance.close(resolve)); });
  return "http://127.0.0.1:" + instance.address().port;
}
const client = (url, more = {}) => new Graphicle({url, workspace: "workspace-1", capability: "fixture-secret", ...more});

test("project workflow sends a bound v1 path and preserves evidence hashes", async t => {
  const calls = [];
  const url = await server(t, async (req, res) => {
    let body = ""; for await (const chunk of req) body += chunk;
    calls.push({path: req.url, auth: req.headers["x-workspace-capability"], body: JSON.parse(body)});
    res.writeHead(200, {"content-type": "application/json"});
    res.end(JSON.stringify({evidence: [{id: "file:auth.py", sha256: "original-hash"}], memories: []}));
  });
  const c = client(url), project = c.project("project-1");
  const packet = await project.context("How does auth work?", {max_tokens: 1200});
  assert.equal(packet.evidence[0].sha256, "original-hash");
  assert.equal(calls[0].path, "/v1/workspaces/workspace-1/repos/project-1/commands");
  assert.equal(calls[0].auth, "fixture-secret");
  assert.deepEqual(calls[0].body, {operation: "context", arguments: {max_tokens: 1200, question: "How does auth work?"}});
  assert.ok(!JSON.stringify(c).includes("fixture-secret"));
  assert.ok(!inspect(project).includes("fixture-secret"));
});

test("redirects never forward the capability to another server", async t => {
  let leaked = false;
  const foreign = await server(t, (_req, res) => { leaked = true; res.end("{}"); });
  const origin = await server(t, (_req, res) => { res.writeHead(307, {location: foreign}); res.end(); });
  await assert.rejects(client(origin).health(), GraphicleError);
  assert.equal(leaked, false);
});

test("quota errors preserve Retry-After and do not retry a write", async t => {
  let calls = 0;
  const url = await server(t, (_req, res) => {
    calls++; res.writeHead(429, {"content-type": "application/json", "retry-after": "3600"});
    res.end('{"error":"Request budget reached"}');
  });
  await assert.rejects(client(url).project("p").enqueue("reflect", {question: "why"}),
    error => error.status === 429 && error.retryAfterSeconds === 3600 && error.message === "Request budget reached");
  assert.equal(calls, 1);
});

test("timeouts are explicit about uncertain mutation completion", async t => {
  let calls = 0;
  const url = await server(t, async req => { for await (const _ of req) {} calls++; });
  await assert.rejects(client(url, {timeoutMs: 100}).project("p").command("note", {body: "Hello"}),
    error => error instanceof GraphicleError && /may have completed/.test(error.message));
  assert.equal(calls, 1);
});

test("caller cancellation is preserved", async t => {
  const url = await server(t, () => {});
  const abort = new AbortController(), reason = new Error("Stop this read");
  abort.abort(reason);
  await assert.rejects(client(url).health({signal: abort.signal}), error => error === reason);
});

test("plain http is limited to loopback, including the rest of 127/8 and IPv4-mapped loopback", () => {
  for (const url of ["http://127.0.0.1:8854", "http://127.0.0.2:8854", "http://[::1]:8854", "http://[::ffff:127.0.0.1]:8854", "http://localhost:8854"]) {
    assert.equal(client(url).url, new URL(url).origin);
  }
  assert.throws(() => client("http://192.0.2.1:8854"), /HTTPS|loopback|trusted/i);
});

test("plain http judges the host in the URL parser's normal form, which requests use", () => {
  for (const [url, origin] of [["http://127.1:8854", "http://127.0.0.1:8854"], ["http://0x7f.0.0.1:8854", "http://127.0.0.1:8854"],
    ["http://0177.0.0.1:8854", "http://127.0.0.1:8854"], ["http://2130706433:8854", "http://127.0.0.1:8854"]]) {
    assert.equal(client(url).url, origin);
  }
  for (const url of ["http://localhost.:8854", "http://127.0.0.1.example.com:8854", "http://[::ffff:10.0.0.1]:8854", "http://[::127.0.0.1]:8854", "http://0.0.0.0:8854"]) {
    assert.throws(() => client(url), /HTTPS/);
  }
});

test("credentials and project authority cannot be redirected by arguments", async () => {
  for (const url of ["https://user:pass@example.com", "https://example.com/path", "https://example.com?x=1", "http://example.com", "https://example.com/#x"]) assert.throws(() => client(url));
  const c = client("https://example.com");
  for (const id of ["..", "../other", "p/q", "p?x=1", ""]) assert.throws(() => c.project(id));
  assert.throws(() => c.project("p").command("context", {repository_id: "other"}));
  assert.throws(() => c.project("p").command("context", []));
  await assert.rejects(c.request("GET", "//attacker.invalid"), TypeError);
  await assert.rejects(c.request("GET", "/\\attacker.invalid"), TypeError);
  assert.throws(() => c.setBudget(-1));
  assert.throws(() => c.setBudget(1.5));
});

test("registration is explicit while project selection performs no I/O", async t => {
  let calls = 0;
  const url = await server(t, (_req, res) => { calls++; res.end('{"id":"registered"}'); });
  const c = client(url);
  assert.equal(c.project("existing").id, "existing"); assert.equal(calls, 0);
  assert.equal((await c.register("my-project", "My project")).id, "registered"); assert.equal(calls, 1);
});
