# Graphicle for TypeScript and JavaScript

Node 22+, ESM, no runtime dependencies. This is a **source package**; @graphicle/sdk
is not published to a registry. From the root of a Graphicle checkout:

~~~sh
npm install ./sdk/typescript
~~~

~~~ts
import { Graphicle } from "@graphicle/sdk";

const client = new Graphicle({
  url: process.env.GRAPHICLE_URL!,
  workspace: process.env.GRAPHICLE_WORKSPACE!,
  capability: process.env.GRAPHICLE_KEY!
});
const project = client.project(process.env.GRAPHICLE_PROJECT!); // existing ID
const packet = await project.context("Where is authentication checked?", {max_tokens: 3000});
console.log(packet.evidence);
~~~

register(key, name) explicitly creates/reuses a project. project(id) never creates
one. The Python client's project(key) registers; use find_project for read-only
discovery.

The client exposes read, graphSource, projectGraph, episodes, graph, impact,
memories, history, enqueue, job, cancel, export, and a generic command.
client.usage() and client.setBudget() require an owner credential.
The service's /docs and /v1/openapi.json describe the versioned transport.

All calls accept a final {signal, timeoutMs} request option. Requests never follow
redirects and are never automatically retried. A timeout can occur after a
mutation commits: inspect current state before retrying. GraphicleError provides
status, parsed body, and retryAfterSeconds when the server sends it. HTTP is
restricted to loopback unless insecureHttp: true is explicitly chosen.
Capabilities remain private client fields; never ship them in public browser code.

Run npm test. TypeScript declarations are shipped alongside the JavaScript
runtime; no build step or package downloads are needed to run the client.
