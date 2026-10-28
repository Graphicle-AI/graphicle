"""Python SDK for Graphicle, a local-first memory service that keeps original sources.

Install with ``pip install ./sdk/python``, or put ``sdk/python`` on
``PYTHONPATH`` (Python 3.10+, standard library only). The SDK talks to the ``graphicle`` service
over its HTTP API; it does not embed the store.

Quick start: a private local service and the Memory facade::

    from graphicle_sdk import LocalServer, Memory

    with LocalServer("/path/to/graphicle", "./graphicle-data", "models.json") as client:
        memory = Memory(client)
        memory.add([{"role": "user", "content": "I moved to Lisbon in March."}],
                   user_id="alice", timestamp="2024-04-02")
        for hit in memory.search("Where did Alice move in March?", filters={"user_id": "alice"})["results"]:
            print(hit["memory"])                         # an exact excerpt of the stored session

        project = memory.project_for(user_id="alice")    # the full Graphicle API for that scope
        result = project.answer("Where did Alice move in March?", question_time="2024-06-01")
        print(result["answer"], result["evidence"])      # citations checked against the originals

Against a service you already run (``graphicle serve`` listens on http://127.0.0.1:8854)::

    from graphicle_sdk import Client

    client = Client.from_session("http://127.0.0.1:8854")    # the owner session of `graphicle init`
    project = client.project("research-notes")
    connector = project.connector("papers")
    project.import_documents(connector, [{"id": "p1", "title": "Notes", "content": "..."}])
    project.context("What did the notes conclude?")["evidence"]
    receipt = project.forget(connector, ["p1"])            # certified forgetting
    assert project.verify(receipt["id"])["verified_local"]

Modules:

* :mod:`graphicle_sdk.client`: :class:`Client` (one workspace; ``command`` reaches every service
  operation), :class:`Project` (sources, context, answers, workflows, forgetting),
  :class:`GraphicleError` and :class:`GraphiclePermissionError` (HTTP 403, also a
  ``PermissionError``).
* :mod:`graphicle_sdk.memory`: :class:`Memory`, the Memory facade, and notes on its behaviour
  (originals instead of rewritten facts, excerpts without scores, reviewed model drafts,
  forgetting with receipts, document ids).
* :mod:`graphicle_sdk.server`: :class:`LocalServer`, which runs ``graphicle init`` and
  ``graphicle serve --port 0`` and returns a connected :class:`Client`.
* :mod:`graphicle_sdk.tools`: :class:`Tools`, the service's MCP tools as OpenAI or Anthropic tool
  definitions, and :meth:`Tools.resolve`, which runs a model's tool call through MCP.
* :mod:`graphicle_sdk.aio`: :class:`AsyncClient` and the async projects, memories and tools it
  returns; each call runs in a worker thread.

Extending: subclass :class:`Project` or :class:`Memory`, or call ``Project.command(operation,
**arguments)`` for operations without a wrapper (``graph.query``, ``memory.export``,
``credential.list`` ...).
"""
from .client import GraphicleError, GraphiclePermissionError, Client, Project
from .memory import Memory, render_messages
from .server import LocalServer
from .tools import Tools, resolve_tool_call, tool_schemas
from .aio import AsyncClient, AsyncMemory, AsyncProject, AsyncTools

__all__ = [
    "GraphicleError",
    "GraphiclePermissionError",
    "AsyncClient",
    "AsyncMemory",
    "AsyncProject",
    "AsyncTools",
    "Client",
    "LocalServer",
    "Memory",
    "Project",
    "Tools",
    "render_messages",
    "resolve_tool_call",
    "tool_schemas",
]
__version__ = "0.1.9"
