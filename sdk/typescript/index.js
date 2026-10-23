/** Dependency-free client. Requests never follow redirects or retry mutations. */
export class GraphicleError extends Error {
  constructor(message, {status = 0, body, retryAfterSeconds, cause} = {}) {
    super(message, {cause});
    this.name = "GraphicleError";
    this.status = status;
    this.body = body;
    this.retryAfterSeconds = retryAfterSeconds;
  }
}
const identifier = value => {
  if (typeof value !== "string" || !/^[a-zA-Z0-9][a-zA-Z0-9_-]{0,127}$/.test(value)) {
    throw new TypeError("Use a workspace or project ID returned by Graphicle");
  }
  return encodeURIComponent(value);
};
function plainObject(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new TypeError("Arguments must be an object");
  return value;
}

function isLoopbackHost(hostname) {
  const host = hostname.startsWith("[") && hostname.endsWith("]") ? hostname.slice(1, -1) : hostname;
  if (host.toLowerCase() === "localhost" || host === "::1") return true;
  const octets = host.split(".");
  if (octets.length === 4 && octets.every((part) => /^\d{1,3}$/.test(part) && Number(part) <= 255)) {
    return Number(octets[0]) === 127;
  }
  // The URL parser rewrites [::ffff:127.0.0.1] to [::ffff:7f00:1].
  const mapped = /^::ffff:(?:(\d+\.\d+\.\d+\.\d+)|([0-9a-f]{1,4}):([0-9a-f]{1,4}))$/i.exec(host);
  if (!mapped) return false;
  if (mapped[1]) return isLoopbackHost(mapped[1]);
  const hi = Number.parseInt(mapped[2], 16);
  return hi >= 0x7f00 && hi <= 0x7fff;
}

export class Graphicle {
  #origin; #capability; #fetch; #timeout;
  constructor({url, workspace, capability, timeoutMs = 60_000, fetch: fetcher = globalThis.fetch, insecureHttp = false}) {
    if (typeof url !== "string" || /[\u0000-\u0020\u007f]/.test(url)) throw new TypeError("url must be an HTTP(S) origin");
    const parsed = new URL(url);
    if (!["http:", "https:"].includes(parsed.protocol) || parsed.username || parsed.password ||
        parsed.pathname !== "/" || url.includes("?") || url.includes("#")) throw new TypeError("url must be an origin without credentials, path, query or fragment");
    if (parsed.protocol === "http:" && !isLoopbackHost(parsed.hostname) && !insecureHttp) {
      throw new TypeError("Use HTTPS for remote services; insecureHttp is only for an explicitly trusted network");
    }
    if (typeof capability !== "string" || !capability || capability.length > 512 || /[\r\n]/.test(capability)) throw new TypeError("Supply a workspace capability");
    if (!Number.isFinite(timeoutMs) || timeoutMs <= 0 || timeoutMs > 2_147_483_647) throw new TypeError("timeoutMs must be positive and finite");
    identifier(workspace);
    Object.defineProperty(this, "workspace", {value: workspace, enumerable: true});
    this.#origin = parsed.origin; this.#capability = capability; this.#fetch = fetcher; this.#timeout = timeoutMs;
  }
  get url() { return this.#origin; }
  async request(method, path, body, {signal, timeoutMs = this.#timeout} = {}) {
    if (typeof path !== "string" || !path.startsWith("/") || path.startsWith("//") || path.includes("\\") || /[\u0000-\u0020\u007f]/.test(path)) throw new TypeError("Use an absolute path on this service");
    const target = new URL(path, this.#origin);
    if (target.origin !== this.#origin || target.hash || target.username || target.password) throw new TypeError("Requests must stay on this service");
    if (!["GET", "POST"].includes(method)) throw new TypeError("Use GET or POST");
    if (!Number.isFinite(timeoutMs) || timeoutMs <= 0 || timeoutMs > 2_147_483_647) throw new TypeError("timeoutMs must be positive and finite");
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(new Error("Request deadline exceeded")), timeoutMs);
    try {
      const response = await this.#fetch(target.href, {
        method, redirect: "error", credentials: "omit",
        headers: {"Accept": "application/json", "Content-Type": "application/json", "X-Workspace-Capability": this.#capability},
        ...(body === undefined ? {} : {body: JSON.stringify(body)}),
        signal: signal ? AbortSignal.any([signal, controller.signal]) : controller.signal
      });
      if (response.status === 204 || response.status === 202 && response.headers.get("content-length") === "0") return null;
      const raw = await response.text();
      let value;
      try { value = raw ? JSON.parse(raw) : null; }
      catch { throw new GraphicleError("The service returned a non-JSON response", {status: response.status}); }
      if (!response.ok) {
        const retry = response.headers.get("retry-after");
        const retryAfterSeconds = retry == null ? undefined : /^\d+$/.test(retry) ? Number(retry) : Math.max(0, Math.ceil((Date.parse(retry) - Date.now()) / 1000));
        throw new GraphicleError(typeof value?.error === "string" ? value.error : "Graphicle request failed", {
          status: response.status, body: value,
          retryAfterSeconds: Number.isFinite(retryAfterSeconds) ? retryAfterSeconds : undefined
        });
      }
      return value;
    } catch (error) {
      if (error instanceof GraphicleError) throw error;
      if (signal?.aborted) throw signal.reason || error;
      throw new GraphicleError(controller.signal.aborted
        ? "Request timed out. A submitted write may have completed; inspect state before retrying."
        : "Unable to complete the request. Submitted writes are not retried automatically.", {cause: error});
    } finally { clearTimeout(timer); }
  }
  project(id) { return new Project(this, id); }
  health(options) { return this.request("GET", "/health", undefined, options); }
  projects(options) { return this.request("GET", "/api/workspaces/" + identifier(this.workspace) + "/repositories", undefined, options); }
  async register(key, name = key, options) {
    const row = await this.request("POST", "/api/workspaces/" + identifier(this.workspace) + "/repositories/command",
      {operation: "register", arguments: {project_key: key, name}}, options);
    return this.project(row.id);
  }
  usage(options) { return this.request("GET", "/v1/workspaces/" + identifier(this.workspace) + "/usage", undefined, options); }
  setBudget(monthlyRequests, options) {
    if (monthlyRequests !== null && (!Number.isSafeInteger(monthlyRequests) || monthlyRequests < 0 || monthlyRequests > 1e9)) throw new TypeError("Budget must be null or an integer from 0 to 1000000000");
    return this.request("POST", "/v1/workspaces/" + identifier(this.workspace) + "/budget", {monthly_requests: monthlyRequests}, options);
  }
}

export class Project {
  #client; #path;
  constructor(client, id) {
    this.#client = client;
    this.#path = "/v1/workspaces/" + identifier(client.workspace) + "/repos/" + identifier(id);
    Object.defineProperty(this, "id", {value: id, enumerable: true});
  }
  command(operation, args = {}, options) {
    plainObject(args);
    if (typeof operation !== "string" || !/^[a-z][a-z0-9_.]{0,99}$/.test(operation)) throw new TypeError("Invalid operation name");
    if ("repository_id" in args || "workspace_id" in args) throw new TypeError("The project is bound by the client");
    return this.#client.request("POST", this.#path + "/commands", {operation, arguments: args}, options);
  }
  overview(options) { return this.#client.request("GET", this.#path + "/overview", undefined, options); }
  status(options) { return this.command("status", {}, options); }
  context(question, args = {}, options) { return this.command("context", {...args, question}, options); }
  read(sourceId, args = {}, options) { return this.command("read", {...args, source_id: sourceId}, options); }
  graphSource(episode, path, args = {}, options) { return this.command("graph.source", {...args, episode, path}, options); }
  projectGraph(options) { return this.command("graph.project", {}, options); }
  episodes(options) { return this.command("graph.episodes", {}, options); }
  graph(args = {}, options) { return this.command("graph.as_of", args, options); }
  impact(base, head, args = {}, options) { return this.command("impact.report", {...args, base, head}, options); }
  memories(args = {}, options) { return this.command("memories", args, options); }
  history(memoryId, args = {}, options) { return this.command("memory.history", {...args, memory_id: memoryId}, options); }
  enqueue(kind, args = {}, options) { return this.command("workflow.enqueue", {...args, kind}, options); }
  job(jobId, options) { return this.command("workflow.status", {job_id: jobId}, options); }
  cancel(jobId, options) { return this.command("workflow.cancel", {job_id: jobId}, options); }
  export(args = {}, options) { return this.command("memory.export", args, options); }
}
