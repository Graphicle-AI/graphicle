export type Json = null | boolean | number | string | Json[] | {[key: string]: Json};
export type Arguments = {[key: string]: Json | undefined};
export type Result = Record<string, any>;
export interface RequestOptions {signal?: AbortSignal; timeoutMs?: number}
export interface ClientOptions {
  url: string; workspace: string; capability: string;
  timeoutMs?: number; fetch?: typeof globalThis.fetch; insecureHttp?: boolean;
}
export interface Evidence extends Result {id: string; sha256?: string; text?: string; quote?: string}
export interface Context extends Result {evidence: Evidence[]; memories: Result[]}
export class GraphicleError extends Error {
  status: number; body?: unknown; retryAfterSeconds?: number;
  constructor(message: string, options?: {status?: number; body?: unknown; retryAfterSeconds?: number; cause?: unknown});
}
export class Graphicle {
  constructor(options: ClientOptions);
  readonly workspace: string; readonly url: string;
  request<T = Result>(method: "GET" | "POST", path: string, body?: unknown, options?: RequestOptions): Promise<T>;
  project(id: string): Project;
  health(options?: RequestOptions): Promise<Result>;
  projects(options?: RequestOptions): Promise<Result[]>;
  register(key: string, name?: string, options?: RequestOptions): Promise<Project>;
  usage(options?: RequestOptions): Promise<Result>;
  setBudget(monthlyRequests: number | null, options?: RequestOptions): Promise<Result>;
}
export class Project {
  constructor(client: Graphicle, id: string);
  readonly id: string;
  command<T = Result>(operation: string, args?: Arguments, options?: RequestOptions): Promise<T>;
  overview(options?: RequestOptions): Promise<Result>;
  status(options?: RequestOptions): Promise<Result>;
  context(question: string, args?: Arguments, options?: RequestOptions): Promise<Context>;
  read(sourceId: string, args?: Arguments, options?: RequestOptions): Promise<Result>;
  graphSource(episode: string, path: string, args?: Arguments, options?: RequestOptions): Promise<Result>;
  projectGraph(options?: RequestOptions): Promise<Result>;
  episodes(options?: RequestOptions): Promise<Result>;
  graph(args?: Arguments, options?: RequestOptions): Promise<Result>;
  impact(base: string, head: string, args?: Arguments, options?: RequestOptions): Promise<Result>;
  memories(args?: Arguments, options?: RequestOptions): Promise<Result[]>;
  history(memoryId: string, args?: Arguments, options?: RequestOptions): Promise<Result>;
  enqueue(kind: string, args?: Arguments, options?: RequestOptions): Promise<Result>;
  job(jobId: string, options?: RequestOptions): Promise<Result>;
  cancel(jobId: string, options?: RequestOptions): Promise<Result>;
  export(args?: Arguments, options?: RequestOptions): Promise<Result>;
}
