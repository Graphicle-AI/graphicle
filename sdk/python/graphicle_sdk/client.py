"""HTTP client for the Graphicle memory service.

Every project operation of the service is ``POST /api/workspaces/{workspace}/repositories/command``
with the JSON body ``{"operation": NAME, "arguments": {...}}`` and the workspace capability in the
``X-Workspace-Capability`` header. :class:`Client` sends those requests; :class:`Project` binds one
project (the service calls it a repository, ``repository_id``) and wraps the common operations.

Connecting::

    from graphicle_sdk import Client

    client = Client("http://127.0.0.1:8854", workspace="<workspace id>", capability="<secret>")
    # Or read the owner session that `graphicle init` saved in a local data directory
    # (default: GRAPHICLE_DATA_DIR, else ~/Library/Application Support/Graphicle on macOS,
    # else $XDG_DATA_HOME/graphicle or ~/.local/share/graphicle):
    client = Client.from_session("http://127.0.0.1:8854")

Projects, sources and retrieval (no model call)::

    project = client.project("support-notes")        # registered once, then reused by key
    chats = project.connector("chats")               # a JSONL connector, created once per name
    kickoff = {"id": "s1", "title": "Kickoff",
               "content": "user: We ship on Friday.\\n\\nassistant: Noted.\\n\\n"}
    project.import_documents(chats, [kickoff])
    packet = project.context("When do we ship?", max_tokens=3000)
    for item in packet["evidence"]:                  # exact windows of the stored originals
        print(item["id"], item["byte_start"], item["byte_end"], item["text"])
    project.read_document(f"connector:{chats}:s1")["content"]   # the whole original

Answers and background work (need a model configuration and a running worker, which
``graphicle serve`` and :class:`graphicle_sdk.LocalServer` provide)::

    result = project.answer("When do we ship?", question_time="2024-06-01")
    result["answer"], result["abstain"], result["evidence"]   # citations were checked against sources
    project.observe(chats, max_calls=16)       # observer notes, resumed round by round until complete
    job = project.enqueue("embed", max_calls=8)   # any workflow kind: basic, reflect, embed, extract,
    project.wait(job["id"], timeout=600)          # observer_notes, recall ...

Certified forgetting::

    receipt = project.forget(chats, ["s1"])    # erases the document and everything derived from it
    project.verify(receipt["id"])["verified_local"]            # re-checked against the current store
    project.import_documents(chats, [kickoff])["forgotten_skipped"]   # 1: it stays forgotten

Extending: :meth:`Client.command` and :meth:`Project.command` reach every service operation,
including those this module does not wrap (``graph.query``, ``memory.export``, ``credential.list``
...), and :meth:`Client.request` reaches the other HTTP routes. The wrappers
drop arguments whose value is ``None``; ``command`` sends exactly what it is given.

Errors: an HTTP error response raises :class:`GraphicleError` with ``status``, ``message`` (the
service's ``error`` text) and ``body``. HTTP 403 raises :class:`GraphiclePermissionError`, which is
also a built-in :class:`PermissionError`. A failed or cancelled job raises :class:`GraphicleError`
with ``status=None`` and the job record in ``job``; an unreachable service raises it with
``status=None`` too. :meth:`Project.wait` raises :class:`TimeoutError`.

Security: the capability is a bearer secret. Plain ``http://`` is accepted only for loopback
addresses (``insecure_http=True`` overrides this for a network you trust). Requests to loopback
addresses ignore proxy settings, redirects are never followed (so the header cannot be
forwarded), and ``repr()`` omits it. Only the Python standard library is used.
"""
from __future__ import annotations

import ipaddress
import json
import math
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import date
from pathlib import Path
from typing import Any, Dict, Iterable, Iterator, List, Mapping, Optional, Sequence, Union

__all__ = [
    "GraphicleError",
    "GraphiclePermissionError",
    "Client",
    "Project",
    "PENDING_STATES",
    "default_data_dir",
]

#: Job states that are still in progress.
PENDING_STATES = frozenset({"queued", "retry", "running"})

def _is_loopback(hostname: str) -> bool:
    """True for localhost and every loopback address, not only three spellings.

    127.0.0.2 and IPv4-mapped ::ffff:127.0.0.1 are loopback. Sending the workspace
    capability to them in cleartext is the same as sending it to 127.0.0.1, and
    refusing them makes a local service on any other 127/8 address unusable.
    """
    if hostname.lower() == "localhost":
        return True
    try:
        return ipaddress.ip_address(hostname).is_loopback
    except ValueError:
        return False


# Service limits: connector.import takes at most 5,000 documents per call and a request body is
# capped at 24 MiB; source.forget takes at most 1,000 document ids.
_IMPORT_DOCUMENTS = 5000
_IMPORT_BYTES = 16 * 1024 * 1024
_READ_LINES = 400


class GraphicleError(Exception):
    """A Graphicle request or job failed.

    ``status`` is the HTTP status code, or ``None`` when there was no HTTP error response (the
    service was unreachable, or a job failed or was cancelled). ``message`` is the service's error
    text, ``body`` the parsed response body and ``job`` the job record for workflow failures.
    """

    def __init__(self, status: Optional[int], message: str, *, body: Any = None,
                 job: Optional[Dict[str, Any]] = None) -> None:
        super().__init__(message)
        self.status = status
        self.message = message
        self.body = body
        self.job = job

    def __str__(self) -> str:
        return self.message if self.status is None else f"{self.message} (HTTP {self.status})"


class GraphiclePermissionError(GraphicleError, PermissionError):
    """HTTP 403: the capability is wrong or revoked, or its role does not permit the operation."""


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    # A followed redirect would resend the capability header to wherever it points.
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def _error(status: int, raw: bytes) -> GraphicleError:
    text = raw.decode("utf-8", "replace").strip()
    try:
        body: Any = json.loads(text) if text else None
    except ValueError:
        body = text
    if isinstance(body, dict) and isinstance(body.get("error"), str):
        message = body["error"]
    else:
        message = text[:2000] or f"HTTP {status}"
    kind = GraphiclePermissionError if status == 403 else GraphicleError
    return kind(status, message, body=body)


def default_data_dir() -> Path:
    """The data directory the ``graphicle`` binary uses when ``--data-dir`` is not given."""
    configured = os.environ.get("GRAPHICLE_DATA_DIR")
    if configured:
        return Path(configured)
    home = Path(os.environ.get("HOME") or ".")
    if sys.platform == "darwin":
        return home / "Library/Application Support/Graphicle"
    return Path(os.environ.get("XDG_DATA_HOME") or home / ".local/share") / "graphicle"


def _present(arguments: Mapping[str, Any]) -> Dict[str, Any]:
    return {key: value for key, value in arguments.items() if value is not None}


def _clip(text: str, limit: int) -> str:
    """At most ``limit`` UTF-8 bytes of ``text``, cut on a character boundary."""
    return text.encode("utf-8")[:limit].decode("utf-8", "ignore")


def _time_text(value: Union[str, date]) -> str:
    return value.isoformat() if isinstance(value, date) else str(value)


class Client:
    """A connection to one Graphicle workspace.

    ``url`` is the service origin (``http://127.0.0.1:8854``), ``workspace`` the workspace id and
    ``capability`` its access key; ``timeout`` bounds each HTTP request in seconds. Plain ``http``
    to a host other than loopback raises ``ValueError`` unless ``insecure_http=True``.
    """

    def __init__(self, url: str, workspace: str, capability: str, timeout: float = 60, *,
                 insecure_http: bool = False) -> None:
        if any(ord(c) <= 32 or ord(c) == 127 for c in url):
            raise ValueError("url must not contain whitespace or control characters")
        parts = urllib.parse.urlsplit(url)
        if (parts.scheme not in ("http", "https") or not parts.hostname
                or parts.username is not None or parts.password is not None
                or parts.path not in ("", "/") or "?" in url or "#" in url):
            raise ValueError("url must be an http(s) origin, for example http://127.0.0.1:8854")
        # Accessing port validates non-numeric and out-of-range port strings.
        _ = parts.port
        if parts.scheme == "http" and not _is_loopback(parts.hostname) and not insecure_http:
            # The capability is a bearer secret; plain HTTP would expose it on the network.
            raise ValueError("Plain http is only for loopback addresses; use https, or pass "
                             "insecure_http=True for a network you trust")
        if not workspace or not capability:
            raise ValueError("workspace and capability are required")
        self.url = url.rstrip("/")
        self.workspace = workspace
        self.timeout = timeout
        self._capability = capability
        self._base = f"/api/workspaces/{urllib.parse.quote(workspace, safe='')}"
        handlers: List[urllib.request.BaseHandler] = [_NoRedirect()]
        if _is_loopback(parts.hostname):
            handlers.append(urllib.request.ProxyHandler({}))
        self._opener = urllib.request.build_opener(*handlers)

    @classmethod
    def from_session(cls, url: str, data_dir: Union[str, os.PathLike, None] = None,
                     timeout: float = 60, *, insecure_http: bool = False) -> "Client":
        """Connect with the owner session ``graphicle init`` saved in ``data_dir``."""
        folder = Path(data_dir).expanduser() if data_dir is not None else default_data_dir()
        session = json.loads((folder / "local-session.json").read_text(encoding="utf-8"))
        return cls(url, session["workspace"], session["capability"], timeout=timeout,
                   insecure_http=insecure_http)

    def __repr__(self) -> str:
        return f"Client(url={self.url!r}, workspace={self.workspace!r})"

    def request(self, method: str, path: str, body: Any = None, *,
                timeout: Optional[float] = None) -> Any:
        """Send a request to an absolute path on this origin and decode its JSON response."""
        if (not path.startswith("/") or path.startswith("//") or "#" in path
                or any(ord(c) <= 32 or ord(c) == 127 for c in path)):
            raise ValueError("path must start with a single / and contain no fragment or whitespace")
        data = None if body is None else json.dumps(body, ensure_ascii=False).encode("utf-8")
        headers = {"Accept": "application/json", "X-Workspace-Capability": self._capability}
        if data is not None:
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(self.url + path, data=data, headers=headers, method=method)
        try:
            with self._opener.open(request, timeout=self.timeout if timeout is None else timeout) as response:
                raw = response.read()
        except urllib.error.HTTPError as error:
            try:
                raw = error.read()
            except OSError:
                raw = b""
            finally:
                error.close()
            raise _error(error.code, raw) from None
        except (urllib.error.URLError, OSError) as error:
            reason = getattr(error, "reason", error)
            if isinstance(reason, TimeoutError):
                limit = self.timeout if timeout is None else timeout
                message = f"Graphicle at {self.url} did not answer {path} within {limit} s"
            else:
                message = f"Cannot reach Graphicle at {self.url}: {reason}"
            raise GraphicleError(None, message) from error
        if not raw:
            return None
        try:
            return json.loads(raw)
        except ValueError as error:
            raise GraphicleError(None, f"Graphicle sent a response that is not JSON for {path}") from error

    def command(self, operation: str, /, **arguments: Any) -> Any:
        """Run one service operation; project operations need ``repository_id``."""
        return self.request("POST", f"{self._base}/repositories/command",
                            {"operation": operation, "arguments": arguments})

    def health(self) -> Dict[str, Any]:
        return self.request("GET", "/health")

    def usage(self) -> Dict[str, Any]:
        """Owner-only UTC request counters and recorded provider usage (service 0.6+)."""
        return self.request("GET", f"/v1/workspaces/{urllib.parse.quote(self.workspace, safe='')}/usage")

    def set_budget(self, monthly_requests: Optional[int]) -> Dict[str, Any]:
        """Set an additional HTTP cap; None removes it but never raises a plan allowance."""
        if monthly_requests is not None and (type(monthly_requests) is not int or not 0 <= monthly_requests <= 1_000_000_000):
            raise ValueError("monthly_requests must be None or an integer from 0 to 1000000000")
        return self.request("POST", f"/v1/workspaces/{urllib.parse.quote(self.workspace, safe='')}/budget",
                            {"monthly_requests": monthly_requests})

    def projects(self) -> List[Dict[str, Any]]:
        """Projects of this workspace the capability may read (``id``, ``project_key``, ``name`` ...)."""
        return self.request("GET", f"{self._base}/repositories")

    def find_project(self, key: str) -> Optional["Project"]:
        """The project registered under ``key``, or ``None``; never creates one."""
        for row in self.projects():
            if row.get("project_key") == key:
                return Project(self, row["id"], key=key)
        return None

    def project(self, key: str, name: Optional[str] = None) -> "Project":
        """Register the project ``key`` (at most 200 bytes) or reuse the one already registered."""
        registered = self.command("register", project_key=key, name=_clip(name or key, 150))
        return Project(self, registered["id"], key=key)


def _batches(documents: Sequence[Dict[str, Any]]) -> Iterator[List[Dict[str, Any]]]:
    batch: List[Dict[str, Any]] = []
    size = 0
    for document in documents:
        cost = len(json.dumps(document, ensure_ascii=False).encode("utf-8")) + 1
        if batch and (len(batch) >= _IMPORT_DOCUMENTS or size + cost > _IMPORT_BYTES):
            yield batch
            batch, size = [], 0
        batch.append(document)
        size += cost
    if batch:
        yield batch


class Project:
    """One Graphicle project. Create it with :meth:`Client.project` or :meth:`Client.find_project`."""

    def __init__(self, client: Client, id: str, key: Optional[str] = None) -> None:
        self.client = client
        self.id = id
        self.key = key
        self._connector_ids: Dict[str, str] = {}

    def __repr__(self) -> str:
        return f"Project(id={self.id!r}, key={self.key!r})"

    def command(self, operation: str, /, **arguments: Any) -> Any:
        """Run a service operation on this project (``repository_id`` is filled in)."""
        arguments["repository_id"] = self.id
        return self.client.command(operation, **arguments)

    def status(self) -> Dict[str, Any]:
        return self.command("status")

    def overview(self) -> Dict[str, Any]:
        """Source, memory and recorded-graph readiness for this project (service 0.6+)."""
        workspace = urllib.parse.quote(self.client.workspace, safe="")
        project = urllib.parse.quote(self.id, safe="")
        return self.client.request("GET", f"/v1/workspaces/{workspace}/repos/{project}/overview")

    def project_graph(self) -> Dict[str, Any]:
        return self.command("graph.project")

    def episodes(self) -> Dict[str, Any]:
        return self.command("graph.episodes")

    def graph(self, **options: Any) -> Dict[str, Any]:
        return self.command("graph.as_of", **options)

    def graph_source(self, episode: str, path: str, **options: Any) -> Dict[str, Any]:
        """Read the retained original at an episode; never substitutes the current file."""
        return self.command("graph.source", episode=episode, path=path, **options)

    def impact(self, base: str, head: str, **options: Any) -> Dict[str, Any]:
        return self.command("impact.report", base=base, head=head, **options)

    def export_memories(self) -> Dict[str, Any]:
        return self.command("memory.export")

    # Sources -------------------------------------------------------------------------------

    def connectors(self) -> List[Dict[str, Any]]:
        return self.command("connector.list")["connectors"]

    def connector(self, name: str, *, acl: Optional[Mapping[str, Any]] = None) -> str:
        """Id of this project's JSONL connector called ``name``, created on first use.

        ``acl`` applies only when the connector is created; the default shares its documents with
        every reader of the workspace: ``{"workspace_readers": True, "credentials": []}``.
        """
        cached = self._connector_ids.get(name)
        if cached:
            return cached
        found = next((row["id"] for row in self.connectors()
                      if row.get("name") == name and row.get("kind") == "jsonl"), None)
        if found is None:
            policy = dict(acl) if acl is not None else {"workspace_readers": True, "credentials": []}
            found = self.command("connector.add", kind="jsonl", name=name, config={"acl": policy})["id"]
        self._connector_ids[name] = found
        return found

    def observe_connector(self, connector_id: str,
                          policy: Optional[Mapping[str, Any]]) -> Dict[str, Any]:
        """Set, or clear with ``None``, a connector's automatic observation policy.

        With a policy the service queues budget-limited ``observer_notes`` rounds over the
        connector's sources once their unobserved tokens reach ``threshold_tokens`` (default
        30,000). Other fields: ``model``, ``profile``, ``max_notes``, ``max_rounds`` (default 8 per
        import or sync), ``max_calls``, ``max_input_tokens`` and ``max_output_tokens``. Imports
        into the connector report the decision under ``observe``.
        """
        return self.command("connector.observe", connector_id=connector_id,
                            observe=None if policy is None else dict(policy))

    def delete_connector(self, connector_id: str) -> Dict[str, Any]:
        """Forget a whole connector and its documents; returns the deletion receipt.

        Unlike :meth:`forget`, this does not block the same documents from a later import into
        another connector.
        """
        receipt = self.command("connector.delete", connector_id=connector_id)["receipt"]
        self._connector_ids = {n: c for n, c in self._connector_ids.items() if c != connector_id}
        return receipt

    def import_documents(self, connector_id: str, documents: Iterable[Mapping[str, Any]],
                         **options: Any) -> Dict[str, Any]:
        """Import ``{"id", "title", "content", ...}`` documents into a JSONL connector.

        Optional document fields: ``metadata`` (any JSON, returned by reads), ``source_timestamp``
        (RFC 3339 with an offset) and ``url``. A missing title defaults to the id. Re-importing an
        id replaces that document; a forgotten id is skipped and counted in ``forgotten_skipped``.
        Options: ``mode`` ("delta", default, or "snapshot", which removes documents not listed) and
        ``deleted_ids``. Large delta imports are split to fit the service's request limits; the
        counts are summed.
        """
        prepared = []
        for document in documents:
            item = dict(document)
            item.setdefault("title", item.get("id"))
            prepared.append(item)
        batches = [prepared] if options.get("mode") == "snapshot" else list(_batches(prepared))
        imported = skipped = 0
        result: Dict[str, Any] = {}
        for batch in batches or [[]]:
            result = self.command("connector.import", connector_id=connector_id, documents=batch,
                                  **_present(options))
            imported += result.get("imported", 0)
            skipped += result.get("forgotten_skipped", 0)
        return {**result, "imported": imported, "forgotten_skipped": skipped,
                "batches": max(1, len(batches))}

    def files(self, **options: Any) -> Dict[str, Any]:
        """One page of authorized sources (``prefix`` matches the path/title, ``offset``, ``limit`` <= 500)."""
        return self.command("files", **_present(options))

    def sources(self, prefix: str = "") -> List[Dict[str, Any]]:
        """Every authorized source (``id``, ``title``, ``sha256``, ``source_kind``, ``bytes``)."""
        found: List[Dict[str, Any]] = []
        offset = 0
        while True:
            page = self.files(prefix=prefix, offset=offset, limit=500)
            found.extend(page["files"])
            if page.get("next_offset") is None:
                return found
            offset = page["next_offset"]

    def read(self, source_id: str, **options: Any) -> Dict[str, Any]:
        """One window of a source (``start_byte`` or ``start_line``, ``line_count`` <= 400, ``sha256``)."""
        return self.command("read", source_id=source_id, **_present(options))

    def read_document(self, source_id: str) -> Dict[str, Any]:
        """The source record with its whole original text in ``content`` (windows are joined)."""
        first = self.read(source_id, line_count=_READ_LINES)
        parts = [first["quote"]]
        following = first.get("next_byte")
        while following is not None:
            # The hash pins the version, so a concurrent update cannot mix two texts.
            window = self.read(source_id, start_byte=following, line_count=_READ_LINES,
                               sha256=first["sha256"])
            parts.append(window["quote"])
            following = window.get("next_byte")
        document = {key: value for key, value in first.items() if key not in _WINDOW_FIELDS}
        document["content"] = "".join(parts)
        return document

    def versions(self, connector_id: str, document_id: str, *,
                 include_content: bool = False) -> Dict[str, Any]:
        """Stored versions of one connector document, oldest first (``document.versions``).

        Each version has ``sha256``, ``created_at``, ``title`` and ``bytes``. Older texts come back
        (in ``content``) only for the workspace owner with ``include_content=True``, and
        ``content_included`` says whether they did: a document's sharing can widen after an
        earlier version was written. Forgetting a document erases its versions.
        """
        return self.command("document.versions", connector_id=connector_id, document_id=document_id,
                            include_content=include_content)

    # Retrieval and answers -----------------------------------------------------------------

    def context(self, question: str, /, max_tokens: int = 4000, **options: Any) -> Dict[str, Any]:
        """Retrieve original evidence for ``question`` without a model call.

        ``max_tokens`` is at most 16,000. Options include ``limit`` (excerpts, at most 40),
        ``max_chars`` (default four characters per token), ``mode``, ``needs``,
        ``memory_retrieval`` / ``graph_retrieval`` / ``doc_links`` (booleans; ``doc_links=True``
        follows documents' links to the code they name, which are otherwise only counted),
        ``notes_first`` (``True`` orders notes first for a question in prose, quotes one window
        per file and dates each excerpt; off by default), ``identifier_route`` (``True`` answers a
        question that is an identifier, or only asks where named identifiers are defined or used,
        with their definitions and references from the code graph and no excerpts; off by
        default, and the packet's ``route`` says which path ran) and the local ONNX profiles
        ``local_embedding_profile`` / ``local_reranker_profile``.
        ``include`` (1-32 source-id prefixes such as ``connector:<id>:``) and ``metadata`` (1-16
        exact values of a document's metadata) limit the search to matching sources before
        ranking; the packet's ``filter``
        reports what they excluded. ``note_keys="merge"`` (or ``True``) lets observer notes
        find the passages they quote; see :meth:`answer`. ``scope="workspace"`` searches every
        project of the workspace this key may read (this project first; at most 8, or
        ``max_projects`` up to 32): each excerpt names its project (``repository_id``,
        ``project``), and the packet's ``workspace`` and ``projects`` report the search.
        """
        options.setdefault("max_chars", min(64000, max(2000, max_tokens * 4)))
        return self.command("context", question=question, max_tokens=max_tokens, **_present(options))

    def enqueue(self, kind: str, /, **arguments: Any) -> Dict[str, Any]:
        """Queue a workflow (``basic``, ``reflect``, ``embed``, ``extract``, ``observer_notes`` ...).

        Budgets: ``max_calls`` (<= 32), ``max_input_tokens``, ``max_output_tokens``,
        ``max_attempts``. An identical request over unchanged sources returns the recorded job,
        including a failed one (``replayed: true``); use :meth:`retry` for a fresh attempt.
        """
        return self.command("workflow.enqueue", kind=kind, **_present(arguments))

    def job(self, job_id: str, *, timeout: Optional[float] = None) -> Dict[str, Any]:
        """The job record; ``timeout`` optionally bounds this status request in seconds."""
        return self.client.request("POST", f"{self.client._base}/repositories/command",
                                   {"operation": "workflow.status",
                                    "arguments": {"repository_id": self.id, "job_id": job_id}},
                                   timeout=timeout)

    def jobs(self) -> List[Dict[str, Any]]:
        return self.command("workflow.status")["jobs"]

    def retry(self, job_id: str) -> Dict[str, Any]:
        return self.command("workflow.retry", job_id=job_id)

    def cancel(self, job_id: str) -> Dict[str, Any]:
        return self.command("workflow.cancel", job_id=job_id)

    def wait(self, job_id: str, timeout: float = 300.0, *, interval: float = 0.25) -> Dict[str, Any]:
        """Poll until the job is no longer queued or running and return its record.

        Raises :class:`TimeoutError` after ``timeout`` seconds. The job may have failed; check
        ``state``. Jobs run only while a worker runs (``graphicle serve`` or ``graphicle worker``).
        Each status request uses at most the remaining wait timeout.
        """
        if not math.isfinite(timeout) or timeout < 0:
            raise ValueError("timeout must be finite and non-negative")
        if not math.isfinite(interval) or interval <= 0:
            raise ValueError("interval must be finite and positive")
        deadline = time.monotonic() + timeout
        delay = min(0.05, interval)
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"Job {job_id} did not finish within {timeout} s")
            try:
                job = self.job(job_id, timeout=min(self.client.timeout, remaining))
            except GraphicleError as error:
                if error.status is None and time.monotonic() >= deadline:
                    raise TimeoutError(f"Job {job_id} did not finish within {timeout} s") from error
                raise
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"Job {job_id} did not finish within {timeout} s")
            if job.get("state") not in PENDING_STATES:
                return job
            time.sleep(min(delay, remaining))
            delay = min(delay * 2, interval)

    def answer(self, question: str, question_time: Union[str, date, None] = None,
               observer_notes: Optional[bool] = None, *, timeout: float = 300.0,
               **options: Any) -> Dict[str, Any]:
        """Answer from original excerpts with the configured model (a ``basic`` job) and wait.

        Returns the job result: ``answer``, ``abstain``, ``unresolved``, ``evidence`` (quotes the
        service checked against the cited sources) and the retrieval ``context``. ``question_time``
        is the current date for relative time expressions ("2024-06-01"). ``observer_notes=True``
        also gives the model unreviewed observer notes, which are never citable. Options include
        ``context_tokens``, ``evidence_limit``, ``notes_tokens``, ``model``, ``answer_contract`` and
        the budgets of :meth:`enqueue`.

        On a project whose code graph holds code symbols, the answer also reads the code graph and
        reviewed memory, as :meth:`context` does, unless ``graph_retrieval`` or
        ``memory_retrieval`` is false; other projects are answered from their sources alone. The
        result's ``context.integration`` says which applied.

        ``answer_contract="grounded-v3"`` reads observer notes unless ``observer_notes=False``:
        every note when they fit ``notes_tokens`` (up to 64000). Excerpts arrive in time order,
        the model reads before it answers and checks the question's premise, and citations that
        miss by a little are mended against the cited source.
        ``note_keys="merge"`` (or ``True``, with notes) keys each excerpt with the notes whose
        quotes it holds and adds those keys to its lexical ranking, so a question worded like a
        note finds the turn the note came from. Evidence is still the original excerpt. A failed job raises
        :class:`GraphicleError` with the job record.
        """
        arguments = dict(options, question=question)
        if question_time is not None:
            arguments["question_time"] = _time_text(question_time)
        if observer_notes is not None:
            arguments["observer_notes"] = bool(observer_notes)
        job: Dict[str, Any] = {}
        for _ in range(3):
            job = self.wait(self.enqueue("basic", **arguments)["id"], timeout)
            if job.get("state") != "complete":
                raise GraphicleError(None, f"Answer job {job.get('id')} {job.get('state')}: "
                                     f"{job.get('error') or 'no recorded error'}", job=job)
            if job.get("result") is not None:
                return job["result"]
            # Sources changed after the answer was recorded (result_unavailable); ask again.
        raise GraphicleError(None, "Sources kept changing while answering; try again", job=job)

    def observe(self, connector_id: str, *, timeout: float = 600.0, max_rounds: int = 64,
                **limits: Any) -> Dict[str, Any]:
        """Write observer notes for every document of a connector, round by round, until complete.

        ``limits`` go to each ``observer_notes`` job (``max_calls``, ``max_input_tokens``,
        ``max_output_tokens``, ``max_notes``, ``profile``). Notes are model-written, unreviewed
        and not citable; :meth:`answer` can read them with ``observer_notes=True``. Returns the
        last round's result (``complete``, ``units``, ``notes``).
        """
        arguments = dict(limits, include=[f"connector:{connector_id}:"])
        previous = None
        job: Dict[str, Any] = {}
        for _ in range(max_rounds):
            queued = self.enqueue("observer_notes", **arguments)
            if queued["id"] == previous:
                raise GraphicleError(None, "Observer notes stopped making progress; raise max_calls "
                                     "or the token limits", job=queued)
            previous = queued["id"]
            job = self.wait(queued["id"], timeout)
            if job.get("state") != "complete":
                raise GraphicleError(None, f"Observer job {job.get('id')} {job.get('state')}: "
                                     f"{job.get('error') or 'no recorded error'}", job=job)
            result = job.get("result") or {}
            if result.get("complete"):
                return result
        raise GraphicleError(None, f"Observer notes still incomplete after {max_rounds} rounds", job=job)

    def observer_status(self, **options: Any) -> Dict[str, Any]:
        return self.command("observer.status", **_present(options))

    def observer_context(self, question: str, /, **options: Any) -> Dict[str, Any]:
        """Observer notes selected for ``question`` and the original windows that support them.

        Options include ``max_tokens`` (default 4000).
        """
        return self.command("observer.context", question=question, **_present(options))

    # Reviewed memory -----------------------------------------------------------------------

    def memories(self, **options: Any) -> List[Dict[str, Any]]:
        """Memory drafts and reviewed memories with review state and freshness."""
        return self.command("memories", **_present(options))

    def review(self, memory_id: str, decision: str, *, maintainer: str, reason: str,
               expected_revision: Optional[int] = None) -> Dict[str, Any]:
        """Approve or reject a memory draft (``decision`` is "approve" or "reject")."""
        if expected_revision is None:
            expected_revision = next((m.get("revision") for m in self.memories(include_retired=True)
                                      if m.get("id") == memory_id), None)
        return self.command("learning.review", memory_id=memory_id, decision=decision,
                            maintainer=maintainer, reason=reason, expected_revision=expected_revision)

    # Certified forgetting ------------------------------------------------------------------

    def forget(self, connector_id: str, document_ids: Union[str, Iterable[str]]) -> Dict[str, Any]:
        """Forget connector documents (at most 1,000 ids) and return the deletion receipt.

        Erases the originals, their versions and everything derived from them (notes, drafts,
        vectors, caches), verifies the store in the same transaction and records a receipt in the
        workspace's hash chain. The ids stay blocked: a later import of them is skipped.
        """
        ids = [document_ids] if isinstance(document_ids, str) else list(document_ids)
        return self.command("source.forget", connector_id=connector_id, document_ids=ids)["receipt"]

    def state(self) -> Dict[str, Any]:
        """Canonical digest of everything the store holds for this project (``graphicle.state.v2``:
        tables with no rows in the project are left out, so editions with and without extra
        tables digest equal content equally, and one store digests the same whichever
        edition reads it)."""
        return self.command("deletion.state")

    def verify(self, receipt_id: str) -> Dict[str, Any]:
        """Re-check a deletion receipt against the current store and the receipt chain."""
        return self.command("deletion.verify", receipt_id=receipt_id)

    def receipts(self) -> Dict[str, Any]:
        """This project's deletion receipts, newest first; ``chain`` covers the whole workspace."""
        listed = self.command("deletion.receipts")
        return {"receipts": [r for r in listed["receipts"] if r.get("repository_id") == self.id],
                "chain": listed["chain"]}

    def delete(self, expected_snapshot: Optional[str] = None) -> Dict[str, Any]:
        """Delete the whole project and return its deletion receipt.

        ``expected_snapshot`` guards against deleting a project that changed since you inspected
        it; by default the current snapshot is read first.
        """
        if expected_snapshot is None:
            expected_snapshot = self.status()["repository"]["snapshot"]
        return self.command("project.delete", expected_snapshot=expected_snapshot)["receipt"]


# Fields of one read window, dropped when windows are joined into a whole document.
_WINDOW_FIELDS = frozenset({"quote", "line", "end_line", "byte_start", "byte_end", "total_lines",
                            "complete", "truncated", "next_byte"})
