"""The Memory facade: a session-memory API on top of Graphicle.

::

    from graphicle_sdk import LocalServer, Memory

    with LocalServer("graphicle", "./graphicle-data", "models.json") as client:
        memory = Memory(client)
        added = memory.add(
            [{"role": "user", "content": "I'm vegetarian and allergic to peanuts."},
             {"role": "assistant", "content": "Noted, no peanuts."}],
            user_id="alice", timestamp="2024-05-01")
        hits = memory.search("Is Alice allergic to peanuts?", filters={"user_id": "alice"})
        for hit in hits["results"]:
            print(hit["id"], hit["memory"])            # exact excerpts of the stored session

        project = memory.project_for(user_id="alice")    # the full Graphicle API for this scope
        project.answer("Is Alice allergic to peanuts?", question_time="2024-06-01")

        receipt = memory.delete(added["results"][0]["id"])["receipt"]   # certified forgetting
        project.verify(receipt["id"])["verified_local"]                  # True

The facade's methods are ``add``, ``search``, ``get_all``, ``get``, ``update``, ``delete``,
``delete_all``, ``history`` and ``reset``; their names and argument shapes follow mem0's
``Memory`` API, so code written for that API moves over with few changes. ``prompt`` turns a
search into a system prompt of numbered, exactly quoted excerpts. ``search`` and ``get_all``
take a filters form (``filters={"user_id": ...}``, ``top_k=...``) and also a top-level form
(``user_id=...``, ``limit=...``). Results are dicts
(``{"results": [{"id", "memory", "hash", "metadata", "score", "created_at", "updated_at",
"user_id"/"agent_id"/"run_id"}]}``) with a few Graphicle fields added.

How the facade maps onto Graphicle
----------------------------------
* Scope: each ``user_id`` is one Graphicle project, key ``<namespace>:user_id:<id>`` (namespace
  ``memory-api`` by default). Without a ``user_id`` the ``agent_id``, else the ``run_id``, owns the
  project. The other ids are stored with each session and narrow results inside that project.
  Different users never share a project, so one user's search cannot return another's text.
* add(): one call is one imported session document in the project's ``<namespace> sessions``
  JSONL connector. Messages become ``role: content`` blocks separated by blank lines (``role
  (name): content`` when a message has a ``name``), preceded by ``Conversation date: <timestamp>``
  when ``timestamp`` is given; :func:`render_messages` shows the exact text. That text is stored
  verbatim and is what search quotes and answers cite.
* Ids: a memory id is the Graphicle connector document id ``<project id>.<digest>``; the digest
  covers the scope and the rendered text. The evidence id the service uses is
  ``connector:<connector id>:<memory id>`` (``source_id`` in search results). Adding the same
  session to the same scope again returns the same id with event ``NONE`` (or ``UPDATE`` when the
  stored version differs, for example in metadata) instead of a duplicate.

Behaviour notes (read before migrating)
---------------------------------------
1. Originals, not model-rewritten facts. Graphicle stores the conversation itself and add()
   makes no model call to extract, merge or rewrite facts; ``infer`` is accepted and has no
   effect. There is one result per add() call (one session), not one per extracted fact or per
   message.
2. search() returns exact excerpts: ``memory`` is the byte range ``byte_start``..``byte_end`` of
   the stored session. Retrieval is the service's lexical rank fusion with no model or embedding
   call, so a query must share words with the session; queries are not embedded by default.
   For dense retrieval, configure a local ONNX embedding profile, build its index and
   pass ``Memory(client, context_options={"local_embedding_profile": NAME})``. There is no
   similarity score: ``score`` is ``None`` and ``rank`` is the position. ``threshold`` and
   ``rerank`` are validated but not applied. A long session can contribute several excerpts, so
   an id can repeat. At most 40 excerpts are retrieved per call. The agent/run ids narrow the
   search on the service before ranking; metadata conditions narrow those 40.
3. Model-written material needs review. Observer notes (``Project.observe``) are unreviewed and
   never citable, and memory drafts from ``Project.enqueue("extract")`` stay drafts until they are
   reviewed (``Project.review``; model review happens only when explicitly delegated). This
   facade's search() returns neither: only originals. ``Project.answer`` cites original quotes
   that the service checks against the sources.
4. delete() is certified forgetting (``source.forget``): the session, its stored versions and
   everything derived from it are erased, the store is verified in the same transaction, and the
   returned receipt can be re-checked later with ``Project.verify(receipt["id"])``. The receipt
   lists what it cannot cover (``not_covered``: earlier backups, requests already sent to a model
   provider, exports). A forgotten session stays forgotten: adding the identical session to the
   same scope again is skipped and returns ``{"results": [], "forgotten_skipped": 1}``.
   delete_all() forgets every matching session (one receipt per connector and 1,000 sessions);
   reset() deletes every project of this namespace, with receipts.
5. update() stores a new version of the session document under the same id (the service's normal
   source-update path, which drops notes derived from the old text). Metadata is merged: given
   keys replace stored ones and the others stay. The previous text stays in the store's version
   table until the memory is deleted.
6. history() lists the stored versions of a session (one per distinct text; metadata-only
   updates add none) and needs the workspace owner's capability, because older texts are
   returned only to the owner. Deletions are recorded as receipts (``Project.receipts()``), not
   as ``DELETE`` history entries: a forgotten session's versions are erased with it.
7. Scopes resolve by owner, not as a global filter: ``search(filters={"agent_id": "a"})`` searches
   agent ``a``'s own project, not the sessions stored for users together with ``agent_id="a"``.
8. Not supported (ValueError): ``expiration_date``, ``memory_type``, add()'s ``prompt`` argument,
   ``reference_date``, advanced filter operators (AND/OR/NOT, eq, in, gt, contains ...) and
   ``actor_id`` filters. Metadata filters match values exactly, or ``"*"`` for "key present".
   ``show_expired`` is accepted and ignored: nothing expires.
9. ``created_at`` is recorded by this SDK when a session is first added; ``updated_at`` is the
   service's observation time of the current version. ``hash`` is the SHA-256 of the whole stored
   session, not of the excerpt. System messages are stored like any other role; non-text content
   parts (images) are not stored.

Permissions: add, delete, delete_all, update and reset need a capability that may administer the
workspace (the local owner session of ``graphicle init``), because they create and list
connectors and forget sources. search and get_all need read access only.
"""
from __future__ import annotations

import hashlib
import json
import logging
import re
from datetime import date, datetime, timezone
from typing import Any, Callable, Dict, List, Mapping, Optional, Sequence, Tuple, TypeVar

from .client import GraphicleError, GraphiclePermissionError, Client, Project, _clip

__all__ = ["Memory", "render_messages"]

logger = logging.getLogger("graphicle")

#: Marker stored in each session document's metadata.
SCHEMA = "graphicle.memory-session.v1"
SCOPES = ("user_id", "agent_id", "run_id")
_IDENTITY = frozenset(SCOPES) | {"actor_id"}
_DIGEST = re.compile(r"[0-9a-f]{32}")
_MAX_EXCERPTS = 40  # the service's per-request excerpt limit
_FORGET_BATCH = 1000  # the service's per-request forget limit
_UNSUPPORTED = {
    "expiration_date": "expiration_date is not supported: Graphicle does not expire sources; "
                       "delete() forgets a session",
    "memory_type": "memory_type is not supported: Graphicle stores the conversation itself",
    "prompt": "prompt is not supported: add() makes no model call",
}
T = TypeVar("T")
Turn = Tuple[str, Optional[str], str]


def _entity(value: Any, name: str) -> Optional[str]:
    """Scope id rules: coerced to str, trimmed, not empty, no internal whitespace."""
    if value is None:
        return None
    text = str(value).strip()
    if not text:
        raise ValueError(f"Invalid {name}: cannot be empty or whitespace-only. Provide a valid identifier.")
    if any(character.isspace() for character in text):
        raise ValueError(f"Invalid {name}: cannot contain whitespace. "
                         "Provide a valid identifier without spaces.")
    return text


def _top_k(value: Any) -> int:
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise ValueError(f"Invalid top_k: {value!r}. Must be a non-negative integer.")
    return value


def _query(value: Any) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ValueError("Invalid query: must be a non-empty string.")
    return value.strip()


def _text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, (list, tuple)):
        parts = [part if isinstance(part, str) else part.get("text")
                 for part in content
                 if isinstance(part, str) or (isinstance(part, Mapping) and part.get("type") == "text")]
        return "\n".join(part for part in parts if isinstance(part, str))
    raise TypeError("Message content must be text or a list of text parts")


def _turns(messages: Any) -> List[Turn]:
    if isinstance(messages, str):
        messages = [{"role": "user", "content": messages}]
    elif isinstance(messages, Mapping):
        messages = [messages]
    elif not isinstance(messages, (list, tuple)):
        raise TypeError("messages must be str, dict, or list[dict]")
    turns: List[Turn] = []
    for message in messages:
        if not isinstance(message, Mapping):
            raise TypeError("Each message must be a dict with role and content")
        if message.get("content") is None:
            continue  # e.g. an assistant tool call without text
        content = _text(message["content"])
        if not content.strip():
            continue
        role = message.get("role")
        if not isinstance(role, str) or not role.strip():
            raise ValueError("Each message needs a role")
        name = message.get("name")
        turns.append((role.strip(), name.strip() if isinstance(name, str) and name.strip() else None,
                      content))
    return turns


def _rfc3339(moment: datetime) -> Optional[str]:
    if moment.tzinfo is None or moment.utcoffset() is None:
        return None  # a naive time has no offset, and none is invented
    return moment.astimezone(timezone.utc).isoformat().replace("+00:00", "Z")


def _session_time(value: Any) -> Tuple[Optional[str], Optional[str]]:
    """(text for the ``Conversation date:`` header, RFC 3339 source timestamp or None)."""
    if value is None:
        return None, None
    if isinstance(value, bool):
        raise TypeError("timestamp must be a datetime, date, ISO 8601 text or Unix time")
    if isinstance(value, (int, float)):
        value = datetime.fromtimestamp(value, tz=timezone.utc)
    if isinstance(value, str):
        text = value.strip()
        if not text or "\n" in text or "\r" in text:
            raise ValueError("timestamp text must be one non-empty line")
        try:
            parsed = datetime.fromisoformat(text[:-1] + "+00:00" if text.endswith("Z") else text)
        except ValueError:
            return text, None  # e.g. "2023/05/20 (Sat) 02:21": kept as written
        return text, _rfc3339(parsed)
    if isinstance(value, datetime):
        return value.isoformat(), _rfc3339(value)
    if isinstance(value, date):
        return value.isoformat(), None
    raise TypeError("timestamp must be a datetime, date, ISO 8601 text or Unix time")


def _render(turns: Sequence[Turn], header: Optional[str]) -> str:
    text = f"Conversation date: {header}\n\n" if header else ""
    for role, name, content in turns:
        text += f"{role} ({name}): {content}\n\n" if name else f"{role}: {content}\n\n"
    return text


def render_messages(messages: Any, timestamp: Any = None) -> str:
    """The exact session text :meth:`Memory.add` stores for these messages."""
    return _render(_turns(messages), _session_time(timestamp)[0])


def _clean_metadata(metadata: Optional[Mapping[str, Any]]) -> Dict[str, Any]:
    if metadata is None:
        return {}
    if not isinstance(metadata, Mapping):
        raise TypeError("metadata must be a dict")
    clean = {}
    for key, value in metadata.items():
        if key in _IDENTITY:
            logger.warning("Ignoring metadata[%r]: identity fields are set by user_id, agent_id and run_id", key)
            continue
        clean[str(key)] = value
    return json.loads(json.dumps(clean))  # a JSON copy; raises TypeError for values JSON cannot hold


def _digest(scope: Mapping[str, str], text: str) -> str:
    canonical = json.dumps({"schema": SCHEMA, "scope": dict(scope), "text": text},
                           sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()[:32]


def _now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


class Memory:
    """The Memory facade over a Graphicle :class:`Client`. See the module documentation.

    ``namespace`` prefixes the project keys (so several applications can share a workspace),
    ``max_tokens`` is the evidence budget of each search and ``context_options`` are extra
    arguments for every search's ``context`` request (for example
    ``{"local_embedding_profile": "minilm", "local_reranker_profile": "reranker"}``).
    """

    def __init__(self, client: Client, *, namespace: str = "memory-api", max_tokens: int = 4000,
                 context_options: Optional[Mapping[str, Any]] = None) -> None:
        if not namespace or ":" in namespace or any(c.isspace() for c in namespace):
            raise ValueError("namespace must be non-empty, without ':' or whitespace")
        self.client = client
        self.namespace = namespace
        self.max_tokens = max_tokens
        self.context_options = dict(context_options or {})
        self.connector_name = f"{namespace} sessions"
        self._projects: Dict[str, str] = {}  # project key -> project id
        self._connectors: Dict[str, List[str]] = {}  # project id -> session connector ids

    def __repr__(self) -> str:
        return f"Memory(client={self.client!r}, namespace={self.namespace!r})"

    # close() and the context manager are no-ops: the facade holds no resources.
    def close(self) -> None:
        pass

    def __enter__(self) -> "Memory":
        return self

    def __exit__(self, *exc_info: Any) -> bool:
        return False

    # Scope and project resolution ----------------------------------------------------------

    @staticmethod
    def _scope(user_id: Any = None, agent_id: Any = None, run_id: Any = None) -> Dict[str, str]:
        scope: Dict[str, str] = {}
        for key, value in (("user_id", user_id), ("agent_id", agent_id), ("run_id", run_id)):
            checked = _entity(value, key)
            if checked is not None:
                scope[key] = checked
        return scope

    def _key(self, scope: Mapping[str, str]) -> str:
        owner = next(key for key in SCOPES if key in scope)
        key = f"{self.namespace}:{owner}:{scope[owner]}"
        if len(key.encode("utf-8")) > 200:  # the service's project key limit
            key = f"{self.namespace}:{owner}#sha256:{hashlib.sha256(scope[owner].encode()).hexdigest()}"
        return key

    def _project(self, scope: Mapping[str, str], *, create: bool) -> Optional[Project]:
        key = self._key(scope)
        cached = self._projects.get(key)
        if cached:
            return Project(self.client, cached, key=key)
        if create:
            owner = next(k for k in SCOPES if k in scope)
            project = self.client.project(key, name=_clip(f"{self.namespace} {owner} {scope[owner]}", 150))
        else:
            project = self.client.find_project(key)
            if project is None:
                return None
        self._projects[key] = project.id
        return project

    def _in_scope(self, scope: Mapping[str, str], create: bool,
                  action: Callable[[Optional[Project]], T]) -> T:
        """Run ``action`` on the scope's project; retry once if cached ids went stale."""
        for attempt in (0, 1):
            cached = self._key(scope) in self._projects
            project = self._project(scope, create=create)
            try:
                return action(project)
            except GraphicleError as error:
                # A project or connector removed by another client answers 400; look both up again.
                if attempt or error.status != 400 or not cached or project is None:
                    raise
                self._drop(project.id)
        raise AssertionError("unreachable")

    def _drop(self, project_id: str) -> None:
        self._projects = {k: v for k, v in self._projects.items() if v != project_id}
        self._connectors.pop(project_id, None)

    def _connector_ids(self, project: Project) -> List[str]:
        cached = self._connectors.get(project.id)
        if cached is None:
            cached = [row["id"] for row in project.connectors()
                      if row.get("name") == self.connector_name and row.get("kind") == "jsonl"]
            if cached:
                self._connectors[project.id] = cached
        return cached

    def _writable_connector(self, project: Project) -> str:
        found = self._connector_ids(project)
        if found:
            return found[0]
        created = project.connector(self.connector_name)
        self._connectors[project.id] = [created]
        return created

    def project_for(self, *, user_id: Any = None, agent_id: Any = None, run_id: Any = None,
                    create: bool = False) -> Optional[Project]:
        """The Graphicle project that holds this scope's sessions (``None`` if none exists yet).

        Use it for what the facade does not cover: ``answer``, ``context``, ``observe``,
        ``receipts``, ``verify`` and ``state``.
        """
        scope = self._scope(user_id, agent_id, run_id)
        if not scope:
            raise ValueError("Provide at least one of user_id, agent_id or run_id")
        return self._project(scope, create=create)

    def connector_id(self, project: Project) -> Optional[str]:
        """The session connector of ``project`` (``None`` before the first add)."""
        found = self._connector_ids(project)
        return found[0] if found else None

    # Documents -----------------------------------------------------------------------------

    def _session_id(self, project: Project, source_id: str) -> Optional[str]:
        """The memory id if ``source_id`` is a session document this facade stored in ``project``."""
        parts = source_id.split(":", 2)
        if len(parts) != 3 or parts[0] != "connector":
            return None
        prefix, dot, digest = parts[2].rpartition(".")
        return parts[2] if dot and prefix == project.id and _DIGEST.fullmatch(digest) else None

    def _sessions(self, project: Project) -> List[str]:
        return [row["id"] for row in project.sources() if self._session_id(project, row["id"])]

    @staticmethod
    def _optional_read(project: Project, source_id: str, **options: Any) -> Optional[Dict[str, Any]]:
        try:
            return project.read(source_id, **options)
        except GraphiclePermissionError:
            raise
        except GraphicleError as error:
            if error.status == 400:  # "Source unavailable": absent, forgotten or not visible
                return None
            raise

    def _locate(self, memory_id: Any) -> Optional[Tuple[Project, str]]:
        """(project, evidence id) of a stored session, or ``None``."""
        if not isinstance(memory_id, str):
            return None
        project_id, dot, digest = memory_id.rpartition(".")
        if not dot or not project_id or not _DIGEST.fullmatch(digest):
            return None
        project = Project(self.client, project_id)
        was_cached = project_id in self._connectors
        for attempt in (0, 1):
            if attempt:
                if not was_cached:
                    return None
                self._connectors.pop(project_id, None)  # the cached connector list may be stale
            try:
                connectors = self._connector_ids(project)
            except GraphiclePermissionError:
                # May read but not list connectors: find the session among the readable sources.
                return next(((project, source) for source in self._sessions(project)
                             if source.endswith(":" + memory_id)), None)
            except GraphicleError as error:
                if error.status == 400:  # no such project
                    return None
                raise
            for connector in connectors:
                source_id = f"connector:{connector}:{memory_id}"
                if self._optional_read(project, source_id, line_count=1) is not None:
                    return project, source_id
        return None

    def _item(self, record: Mapping[str, Any], memory_id: str, text: str, **extra: Any) -> Dict[str, Any]:
        stored = record.get("metadata") or {}
        item: Dict[str, Any] = {
            "id": memory_id, "memory": text, "hash": record.get("sha256"),
            "metadata": stored.get("metadata") or None,
            "created_at": stored.get("created_at"), "updated_at": record.get("observed_at"),
        }
        for key in SCOPES:
            if stored.get(key):
                item[key] = stored[key]
        item.update(extra)
        return item

    @staticmethod
    def _matches(stored: Mapping[str, Any], scope: Mapping[str, str],
                 conditions: Mapping[str, Any]) -> bool:
        if any(stored.get(key) != value for key, value in scope.items()):
            return False
        metadata = stored.get("metadata") or {}
        for key, expected in conditions.items():
            if expected == "*":
                if key not in metadata:
                    return False
            elif metadata.get(key) != expected:
                return False
        return True

    def _filters(self, filters: Optional[Mapping[str, Any]], legacy: Dict[str, Any],
                 method: str) -> Tuple[Dict[str, str], Dict[str, Any], Optional[int]]:
        if filters is not None and not isinstance(filters, Mapping):
            raise TypeError("filters must be a dict")
        given = dict(filters or {})
        limit = legacy.pop("limit", None)
        for key in SCOPES:
            value = legacy.pop(key, None)
            if value is None:
                continue
            if given.get(key) is not None and str(given[key]).strip() != str(value).strip():
                raise ValueError(f"{key} is given twice with different values")
            given[key] = value
        if legacy:
            raise TypeError(f"{method}() got unexpected keyword arguments: {', '.join(sorted(legacy))}")
        scope = self._scope(*(given.pop(key, None) for key in SCOPES))
        if not scope:
            raise ValueError("filters must contain at least one of: user_id, agent_id, run_id. "
                             "Example: filters={'user_id': 'u1'}")
        for key, value in given.items():
            if key in ("AND", "OR", "NOT", "actor_id") or isinstance(value, Mapping):
                raise ValueError(f"Unsupported filter {key!r}: metadata filters match a value exactly, "
                                 "or '*' for any value")
        return scope, given, limit

    # Facade API ----------------------------------------------------------------------------

    def add(self, messages: Any, *, user_id: Any = None, agent_id: Any = None, run_id: Any = None,
            metadata: Optional[Mapping[str, Any]] = None, timestamp: Any = None,
            expiration_date: Any = None, infer: bool = True, memory_type: Optional[str] = None,
            prompt: Optional[str] = None) -> Dict[str, Any]:
        """Store the messages as one session document of the scope's project.

        ``timestamp`` (datetime, date, ISO 8601 text, other one-line date text or Unix time) becomes
        the ``Conversation date:`` header; an offset-aware time is also recorded as the source time.
        ``infer`` has no effect: no model is called. A session holds at most 512 KiB of text (a
        service limit). Returns ``{"results": [{"id", "memory", "event", ...}]}`` with event
        ``ADD``, ``NONE`` (this exact session is already stored) or ``UPDATE`` (the stored version
        had other metadata or was edited with update()), or ``{"results": [],
        "forgotten_skipped": 1}`` if this session was forgotten.
        """
        for name, value in (("expiration_date", expiration_date), ("memory_type", memory_type),
                            ("prompt", prompt)):
            if value is not None:
                raise ValueError(_UNSUPPORTED[name])
        scope = self._scope(user_id, agent_id, run_id)
        if not scope:
            raise ValueError("At least one of 'user_id', 'agent_id', or 'run_id' must be provided.")
        header, source_time = _session_time(timestamp)
        turns = _turns(messages)
        if not turns:
            return {"results": []}
        text = _render(turns, header)
        extra = _clean_metadata(metadata)
        digest = _digest(scope, text)
        sha256 = hashlib.sha256(text.encode("utf-8")).hexdigest()

        def action(project: Optional[Project]) -> Dict[str, Any]:
            assert project is not None  # created on demand
            connector = self._writable_connector(project)
            memory_id = f"{project.id}.{digest}"
            existing = self._optional_read(project, f"connector:{connector}:{memory_id}", line_count=1)
            previous = (existing or {}).get("metadata") or {}
            record: Dict[str, Any] = {"schema": SCHEMA, **scope,
                                      "created_at": previous.get("created_at") or _now()}
            if extra:
                record["metadata"] = extra
            result = {"id": memory_id, "memory": text, "metadata": extra or None, **scope}
            if (existing is not None and existing.get("sha256") == sha256
                    and (previous.get("metadata") or {}) == extra):
                return {"results": [dict(result, event="NONE")]}
            document = {"id": memory_id, "title": _clip(f"Conversation {header}" if header else "Conversation", 1000),
                        "content": text, "metadata": record}
            if source_time:
                document["source_timestamp"] = source_time
            imported = project.import_documents(connector, [document])
            if imported.get("forgotten_skipped"):
                return {"results": [], "forgotten_skipped": imported["forgotten_skipped"]}
            return {"results": [dict(result, event="UPDATE" if existing is not None else "ADD")]}

        return self._in_scope(scope, True, action)

    def search(self, query: str, *, top_k: int = 20, filters: Optional[Mapping[str, Any]] = None,
               threshold: Optional[float] = None, rerank: bool = False, explain: bool = False,
               reference_date: Any = None, show_expired: bool = False, **kwargs: Any) -> Dict[str, Any]:
        """Exact excerpts of this scope's stored sessions that are relevant to ``query``.

        Each result also has ``rank``, ``source_id``, ``byte_start``, ``byte_end`` and
        ``total_bytes``; ``explain=True`` adds the service's retrieval reasons in ``score_details``.

        Like Mem0's ``search``, it reads only the stored sessions: reviewed memory and the code
        graph stay off (``memory_retrieval`` and ``graph_retrieval`` are false). ``Project.context``
        and ``Project.answer`` use them.
        """
        if reference_date is not None:
            raise ValueError("reference_date is not supported; pass question_time to Project.answer()")
        if threshold is not None and (isinstance(threshold, bool) or not isinstance(threshold, (int, float))
                                      or not 0 <= threshold <= 1):
            raise ValueError(f"Invalid threshold: {threshold!r}. Must be between 0 and 1 (inclusive).")
        question = _query(query)
        scope, conditions, limit = self._filters(filters, kwargs, "search")
        wanted = _top_k(limit if limit is not None else top_k)
        # The scope ids and the session marker are top-level values of each session's metadata,
        # so the service narrows to them before ranking. Metadata conditions (nested, or "*") are
        # checked here, over the most excerpts one request returns.
        narrowed = bool(conditions)

        def action(project: Optional[Project]) -> Dict[str, Any]:
            if project is None or wanted == 0:
                return {"results": []}
            # Mem0 parity: sessions only, no memory or code graph.
            options = {**self.context_options, "memory_retrieval": False, "graph_retrieval": False,
                       "metadata": {"schema": SCHEMA, **scope},
                       "limit": _MAX_EXCERPTS if narrowed else min(wanted, _MAX_EXCERPTS)}
            packet = project.context(question, max_tokens=self.max_tokens, **options)
            sources = {source.get("id"): source for source in packet.get("sources") or []}
            results: List[Dict[str, Any]] = []
            for position, excerpt in enumerate(packet.get("evidence") or [], 1):
                memory_id = self._session_id(project, str(excerpt.get("id", "")))
                if memory_id is None:
                    continue
                source = sources.get(excerpt["id"]) or {}
                if not self._matches(source.get("metadata") or {}, scope, conditions):
                    continue
                item = self._item(dict(source, sha256=excerpt.get("sha256")), memory_id, excerpt["text"],
                                  score=None, rank=len(results) + 1, source_id=excerpt["id"],
                                  byte_start=excerpt.get("byte_start"), byte_end=excerpt.get("byte_end"),
                                  total_bytes=excerpt.get("total_bytes"))
                if explain:
                    item["score_details"] = {"reason": source.get("reason"), "packet_position": position,
                                             "retrieval": packet.get("retrieval")}
                results.append(item)
                if len(results) >= wanted:
                    break
            return {"results": results}

        return self._in_scope(scope, False, action)

    def prompt(self, query: str, *, top_k: int = 8, filters: Optional[Mapping[str, Any]] = None,
               instructions: Optional[str] = None, **kwargs: Any) -> str:
        """A system prompt carrying the stored excerpts relevant to ``query``, numbered for citation.

        Takes the scope arguments of :meth:`search` (``filters={"user_id": ...}`` or
        ``user_id=...``). Each excerpt is quoted exactly with its ``source_id`` and byte range,
        so an answer citing ``[n]`` can be checked later with ``Project.read``. ``instructions``
        replaces the opening paragraph. No model is called.
        """
        found = self.search(query, top_k=top_k, filters=filters, **kwargs)["results"]
        lines = [instructions or (
            "Stored conversation excerpts that may help. They quote what was said, word for word; "
            "they are not verified facts. Cite an excerpt as [n], and say so when none of them "
            "answers the request.")]
        if not found:
            lines.append("No stored excerpt matched this request.")
        for number, hit in enumerate(found, 1):
            recorded = f", recorded {hit['created_at']}" if hit.get("created_at") else ""
            lines.append(f"\n[{number}] {hit['source_id']} bytes {hit.get('byte_start')}-"
                         f"{hit.get('byte_end')}{recorded}\n{str(hit['memory']).strip()}")
        return "\n".join(lines)

    def get_all(self, *, filters: Optional[Mapping[str, Any]] = None, top_k: int = 20,
                show_expired: bool = False, **kwargs: Any) -> Dict[str, Any]:
        """The scope's stored sessions in full, in document-id order (not by time)."""
        scope, conditions, limit = self._filters(filters, kwargs, "get_all")
        wanted = _top_k(limit if limit is not None else top_k)

        def action(project: Optional[Project]) -> Dict[str, Any]:
            results: List[Dict[str, Any]] = []
            if project is None or wanted == 0:
                return {"results": results}
            for source_id in self._sessions(project):
                try:
                    document = project.read_document(source_id)
                except GraphiclePermissionError:
                    raise
                except GraphicleError as error:
                    if error.status in (400, 409):  # removed or changed since it was listed
                        continue
                    raise
                if self._matches(document.get("metadata") or {}, scope, conditions):
                    results.append(self._item(document, source_id.split(":", 2)[2], document["content"]))
                    if len(results) >= wanted:
                        break
            return {"results": results}

        return self._in_scope(scope, False, action)

    def get(self, memory_id: str) -> Optional[Dict[str, Any]]:
        """One stored session in full, or ``None`` if it does not exist (or was forgotten)."""
        located = self._locate(memory_id)
        if located is None:
            return None
        project, source_id = located
        try:
            document = project.read_document(source_id)
        except GraphiclePermissionError:
            raise
        except GraphicleError as error:
            if error.status == 400:
                return None
            raise
        return self._item(document, memory_id, document["content"], score=None)

    def update(self, memory_id: str, text: Optional[str] = None,
               metadata: Optional[Mapping[str, Any]] = None, expiration_date: Any = None,
               data: Optional[str] = None) -> Dict[str, Any]:
        """Replace a session's text and/or merge metadata; the id stays the same (see note 5)."""
        if expiration_date is not None:
            raise ValueError(_UNSUPPORTED["expiration_date"])
        if text is None:
            text = data
        if text is None and metadata is None:
            raise ValueError("At least one of text or metadata must be provided.")
        if text is not None and (not isinstance(text, str) or not text.strip()):
            raise ValueError("text must be a non-empty string")
        located = self._locate(memory_id)
        if located is None:
            raise ValueError(f"Memory with id {memory_id} not found. Please provide a valid 'memory_id'")
        project, source_id = located
        document = project.read_document(source_id)
        record = dict(document.get("metadata") or {})
        if metadata is not None:
            merged = dict(record.get("metadata") or {})
            merged.update(_clean_metadata(metadata))
            record["metadata"] = merged
        replacement = {"id": memory_id, "title": document.get("title") or "Conversation",
                       "content": document["content"] if text is None else text, "metadata": record}
        if document.get("recorded_at"):
            replacement["source_timestamp"] = document["recorded_at"]
        imported = project.import_documents(source_id.split(":", 2)[1], [replacement])
        if imported.get("forgotten_skipped"):
            raise ValueError(f"Memory with id {memory_id} was forgotten")
        return {"message": "Memory updated successfully!"}

    def delete(self, memory_id: str) -> Dict[str, Any]:
        """Forget one stored session; returns ``{"message", "receipt"}``."""
        located = self._locate(memory_id)
        if located is None:
            raise ValueError(f"Memory with id {memory_id} not found")
        project, source_id = located
        receipt = project.forget(source_id.split(":", 2)[1], [memory_id])
        return {"message": "Memory deleted successfully!", "receipt": receipt}

    def delete_all(self, user_id: Any = None, agent_id: Any = None, run_id: Any = None) -> Dict[str, Any]:
        """Forget every stored session of the scope; returns ``{"message", "deleted", "receipts"}``."""
        scope = self._scope(user_id, agent_id, run_id)
        if not scope:
            raise ValueError("At least one filter is required to delete all memories. "
                             "If you want to delete all memories, use the `reset()` method.")

        def action(project: Optional[Project]) -> Dict[str, Any]:
            receipts: List[Dict[str, Any]] = []
            chosen: Dict[str, List[str]] = {}
            if project is not None:
                for source_id in self._sessions(project):
                    if len(scope) > 1:  # narrower than the project: check each session's scope
                        window = self._optional_read(project, source_id, line_count=1)
                        if window is None or not self._matches(window.get("metadata") or {}, scope, {}):
                            continue
                    _, connector, memory_id = source_id.split(":", 2)
                    chosen.setdefault(connector, []).append(memory_id)
                for connector, ids in chosen.items():
                    for start in range(0, len(ids), _FORGET_BATCH):
                        receipts.append(project.forget(connector, ids[start:start + _FORGET_BATCH]))
            return {"message": "Memories deleted successfully!",
                    "deleted": sum(len(ids) for ids in chosen.values()), "receipts": receipts}

        return self._in_scope(scope, False, action)

    def history(self, memory_id: str) -> List[Dict[str, Any]]:
        """Every stored version of a session, oldest first, as history entries.

        The first entry is ``ADD``; each later version is an ``UPDATE`` with ``old_memory`` and
        ``new_memory``. The service keeps one version per distinct text: an update that changes
        only metadata, or restores an earlier text, adds no entry. Needs the workspace owner's
        capability: the service returns older texts only to the owner (see
        :meth:`Project.versions`). A forgotten or unknown session has no history, so this returns
        ``[]``.
        """
        located = self._locate(memory_id)
        if located is None:
            return []
        project, source_id = located
        _, connector, _ = source_id.split(":", 2)
        try:
            listed = project.versions(connector, memory_id, include_content=True)
        except GraphiclePermissionError:
            raise
        except GraphicleError as error:
            if error.status == 400:  # forgotten or no longer visible since it was located
                return []
            raise
        if not listed.get("content_included"):
            raise GraphiclePermissionError(
                403, "history() needs the workspace owner's capability: older session texts are "
                     "returned only to the owner")
        entries: List[Dict[str, Any]] = []
        previous: Optional[str] = None
        for number, version in enumerate(listed.get("versions") or []):
            entries.append({"id": f"{memory_id}@{version['sha256'][:16]}", "memory_id": memory_id,
                            "old_memory": previous, "new_memory": version.get("content"),
                            "event": "ADD" if number == 0 else "UPDATE",
                            "created_at": version.get("created_at"), "updated_at": None,
                            "is_deleted": 0, "actor_id": None, "role": None,
                            "hash": version.get("sha256")})
            previous = version.get("content")
        return entries

    def reset(self) -> Dict[str, Any]:
        """Delete every project of this namespace (all users, agents and runs), with receipts."""
        receipts = []
        prefix = f"{self.namespace}:"
        for row in self.client.projects():
            key = str(row.get("project_key") or "")
            if key.startswith(prefix):
                project = Project(self.client, row["id"], key=key)
                receipts.append(project.delete(expected_snapshot=row.get("snapshot")))
        self._projects.clear()
        self._connectors.clear()
        return {"message": "Memory store reset", "receipts": receipts}
