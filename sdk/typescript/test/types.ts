import { Graphicle, GraphicleError, type Context } from "../index.js";

const client = new Graphicle({url: "https://memory.example.test", workspace: "workspace", capability: "test"});
const project = client.project("project");
const packet: Context = await project.context("Where is auth?", {max_tokens: 2000});
const source: string = packet.evidence[0].id;
await project.read(source, {}, {signal: new AbortController().signal, timeoutMs: 1000});
await project.graph({limit: 80, edge_limit: 120});
await client.setBudget(null);
// @ts-expect-error outcome() is not part of the SDK (unit learning-and-hints)
await project.outcome("served-context", "useful");
// @ts-expect-error budget must be numeric or null
await client.setBudget("100");
// @ts-expect-error request method is explicit
await client.request("DELETE", "/health");
const error = new GraphicleError("Budget exceeded", {status: 429, retryAfterSeconds: 60});
const status: number = error.status;
void status;
