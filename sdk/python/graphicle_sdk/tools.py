"""Function-calling helpers: the service's MCP tools as model tool definitions, and the calls a
model makes run through the same MCP endpoint.

::

    from graphicle_sdk import Client, Tools

    project = Client.from_session("http://127.0.0.1:8854").project("notes")
    tools = Tools(project)
    response = openai_client.chat.completions.create(
        model=MODEL, messages=messages, tools=tools.schemas("openai"))
    message = response.choices[0].message
    messages.append(message)
    for call in message.tool_calls or []:
        messages.append(tools.resolve(call))   # {"role": "tool", "tool_call_id": ..., "content": ...}

One contract: the definitions are the MCP ``tools/list`` of ``/mcp/<workspace>/<project>`` and
every call goes through MCP ``tools/call``. The service therefore checks the arguments against
the published schema, fixes the project, re-authorizes the capability and maps each tool to its
operation exactly as for an MCP client; arguments naming ``repository_id`` or ``workspace_id``
are refused here before anything is sent, and so are ``repo`` on ``recall_project`` and
``read_project_source`` and a ``scope`` other than ``"project"``: a model given these tools reads
no other project's sources or memory, even where the key could. The graph tools and the
graph-history tools still take ``repo`` and read the code graph of another project the key can
read; to keep a model to one project, give it a key whose access to the other projects is
revoked (the ``graph.access`` operation). ``Tools(project,
cross_project=True)`` opts in to the workspace: the definitions and calls are then those of the
workspace connection ``/mcp/<workspace>``, where ``list_projects`` lists the projects the key may
read, ``repo`` selects one (this project by default) and ``recall_project`` searches them all,
this project first, unless ``repo`` names one. Opt in only where everything the key can read may
reach the model. No tool can approve memory or run commands. Write tools
(``propose_memory``, ``propose_observations`` ...) are published only while the service runs with MCP
learning enabled (``graphicle serve --mcp-learning``), and are left out here unless
``include_write=True``.

Formats: ``"openai"`` (Chat Completions ``tools``), ``"openai-responses"`` (Responses API
``tools``), ``"anthropic"`` (Messages API ``tools``) and ``"mcp"`` (the definitions as
published). :meth:`Tools.resolve` accepts the matching call shapes, as dicts or SDK objects: a
Chat Completions tool call, a Responses ``function_call`` item or an Anthropic ``tool_use`` block.
It returns the message that carries the result back. A failed call (unknown tool, invalid
arguments, an error from the service) comes back as an error result the model can read;
:class:`GraphiclePermissionError` is raised instead, because retrying cannot fix it.
"""
from __future__ import annotations

import copy
import json
import urllib.parse
from typing import Any, Dict, List, Mapping, Optional

from .client import GraphicleError, GraphiclePermissionError, Project

__all__ = ["FORMATS", "Tools", "resolve_tool_call", "tool_schemas"]

#: Tool definition formats :meth:`Tools.schemas` produces.
FORMATS = ("openai", "openai-responses", "anthropic", "mcp")
# The service fixes these itself; a model must not choose them.
_FIXED = ("repository_id", "workspace_id")
# What reaches other projects: tools listed only for the workspace, and the tools whose ``repo``
# names another project. Without ``cross_project`` they are left out here even if a service
# publishes them on a project connection.
_WORKSPACE_TOOLS = ("list_projects",)
_REPO_TOOLS = ("recall_project", "read_project_source")


def _field(value: Any, name: str) -> Any:
    """``value[name]`` for a mapping, else the attribute (SDK response objects)."""
    if isinstance(value, Mapping):
        return value.get(name)
    return getattr(value, name, None)


class Tools:
    """The MCP tools of one project, for function-calling models.

    They read this project only; ``cross_project=True`` gives the model the workspace
    connection's tools instead, with this project as the default (see the module docstring).
    Definitions are read once and cached; :meth:`definitions` with ``refresh=True`` reads them
    again (for example after the service was restarted with MCP learning enabled).
    """

    def __init__(self, project: Project, *, include_write: bool = False,
                 cross_project: bool = False) -> None:
        self.project = project
        self.include_write = include_write
        self.cross_project = cross_project
        self._definitions: Optional[List[Dict[str, Any]]] = None
        self._sequence = 0

    def __repr__(self) -> str:
        options = f", cross_project={self.cross_project!r}" if self.cross_project else ""
        return f"Tools(project={self.project!r}, include_write={self.include_write!r}{options})"

    def _rpc(self, method: str, params: Optional[Mapping[str, Any]] = None) -> Any:
        self._sequence += 1
        body: Dict[str, Any] = {"jsonrpc": "2.0", "id": self._sequence, "method": method}
        if params is not None:
            body["params"] = dict(params)
        client = self.project.client
        path = "/mcp/{}".format(urllib.parse.quote(client.workspace, safe=""))
        if not self.cross_project:
            path += "/" + urllib.parse.quote(self.project.id, safe="")
        reply = client.request("POST", path, body)
        if not isinstance(reply, Mapping):
            raise GraphicleError(None, f"MCP {method} returned no result")
        if reply.get("error") is not None:
            message = _field(reply["error"], "message") or f"MCP {method} failed"
            raise GraphicleError(None, str(message), body=reply)
        return reply.get("result")

    def definitions(self, *, refresh: bool = False) -> List[Dict[str, Any]]:
        """The published MCP tool definitions (``name``, ``description``, ``inputSchema``,
        ``annotations``); read-only tools only unless ``include_write``. Without
        ``cross_project``, nothing that reaches another project: no ``list_projects``, and no
        ``repo`` on ``recall_project`` or ``read_project_source``."""
        if self._definitions is None or refresh:
            listed = self._rpc("tools/list") or {}
            definitions = []
            for tool in listed.get("tools") or []:
                if not (self.include_write or (tool.get("annotations") or {}).get("readOnlyHint", True)):
                    continue
                if not self.cross_project:
                    if tool.get("name") in _WORKSPACE_TOOLS:
                        continue
                    tool = copy.deepcopy(tool)
                    properties = (tool.get("inputSchema") or {}).get("properties")
                    if tool.get("name") in _REPO_TOOLS and isinstance(properties, dict):
                        properties.pop("repo", None)
                definitions.append(tool)
            self._definitions = definitions
        return copy.deepcopy(self._definitions)

    def names(self) -> List[str]:
        return [tool["name"] for tool in self.definitions()]

    def _takes_repo(self, name: str) -> bool:
        """Whether the published tool ``name`` has a ``repo`` argument."""
        for tool in self.definitions():
            if tool["name"] == name:
                return "repo" in ((tool.get("inputSchema") or {}).get("properties") or {})
        return False

    def schemas(self, format: str = "openai") -> List[Dict[str, Any]]:
        """Tool definitions for a model API; ``format`` is one of :data:`FORMATS`."""
        if format not in FORMATS:
            raise ValueError(f"format must be one of {', '.join(FORMATS)}")
        converted: List[Dict[str, Any]] = []
        for tool in self.definitions():
            name = tool["name"]
            description = tool.get("description", "")
            schema = tool.get("inputSchema") or {"type": "object", "properties": {}}
            if format == "openai":
                converted.append({"type": "function", "function": {
                    "name": name, "description": description, "parameters": schema}})
            elif format == "openai-responses":
                converted.append({"type": "function", "name": name, "description": description,
                                  "parameters": schema})
            elif format == "anthropic":
                converted.append({"name": name, "description": description, "input_schema": schema})
            else:
                converted.append(tool)
        return converted

    def call(self, name: str, arguments: Optional[Mapping[str, Any]] = None) -> Any:
        """Run one tool and return its structured result.

        Raises ``ValueError`` for a tool this project does not publish or for arguments that name
        the project or workspace (without ``cross_project``, also ``repo`` on ``recall_project``
        and ``read_project_source`` and a ``scope`` other than ``"project"``), and
        :class:`GraphicleError` when the service refuses the call. With ``cross_project``, a
        call that names no project with ``repo`` reads this one, and ``recall_project`` then
        searches the workspace from it.
        """
        published = self.names()
        if name not in published:
            raise ValueError(f"Unknown tool {name!r}; this project publishes {', '.join(published)}")
        given = dict(arguments or {})
        fixed = [key for key in _FIXED if key in given]
        if fixed:
            raise ValueError(f"Tool arguments cannot set {', '.join(fixed)}: the project is fixed")
        if not self.cross_project:
            crossing = ["repo"] if name in _REPO_TOOLS and "repo" in given else []
            if given.get("scope") not in (None, "project"):
                crossing.append("scope")
            if crossing:
                raise ValueError(f"Tool arguments cannot set {', '.join(crossing)}: these tools read "
                                 "this project only (Tools(..., cross_project=True) reaches the workspace)")
        elif "repo" not in given and self._takes_repo(name):
            given["repo"] = self.project.id
            if name == "recall_project":
                given.setdefault("scope", "workspace")
        result = self._rpc("tools/call", {"name": name, "arguments": given}) or {}
        parts = [part.get("text", "") for part in result.get("content") or [] if isinstance(part, Mapping)]
        if result.get("isError"):
            raise GraphicleError(None, " ".join(parts).strip() or f"Tool {name} failed", body=result)
        if result.get("structuredContent") is not None:
            return result["structuredContent"]
        text = "".join(parts)
        try:
            return json.loads(text)
        except ValueError:
            return text

    def resolve(self, call: Any) -> Dict[str, Any]:
        """Run a model's tool call and return the message that carries its result back."""
        function = _field(call, "function")
        kind = _field(call, "type")
        if function is not None:
            shape, call_id, name, raw = "openai", _field(call, "id"), _field(function, "name"), _field(function, "arguments")
        elif kind == "function_call":
            shape, call_id, name, raw = "openai-responses", _field(call, "call_id"), _field(call, "name"), _field(call, "arguments")
        elif kind == "tool_use":
            shape, call_id, name, raw = "anthropic", _field(call, "id"), _field(call, "name"), _field(call, "input")
        else:
            raise ValueError("Expected a Chat Completions tool call, a Responses function_call item "
                             "or an Anthropic tool_use block")
        failed = False
        try:
            arguments = json.loads(raw) if isinstance(raw, str) and raw.strip() else (raw or {})
            if not isinstance(arguments, Mapping):
                raise ValueError("Tool arguments must be a JSON object")
            content = json.dumps(self.call(str(name), arguments), ensure_ascii=False)
        except GraphiclePermissionError:
            raise
        except (GraphicleError, ValueError) as error:
            failed = True
            content = json.dumps({"error": str(error)}, ensure_ascii=False)
        if shape == "openai":
            return {"role": "tool", "tool_call_id": call_id, "content": content}
        if shape == "openai-responses":
            return {"type": "function_call_output", "call_id": call_id, "output": content}
        message: Dict[str, Any] = {"type": "tool_result", "tool_use_id": call_id, "content": content}
        if failed:
            message["is_error"] = True
        return message


def tool_schemas(project: Project, format: str = "openai", *, include_write: bool = False,
                 cross_project: bool = False) -> List[Dict[str, Any]]:
    """:meth:`Tools.schemas` for one project (reads the definitions each time)."""
    return Tools(project, include_write=include_write, cross_project=cross_project).schemas(format)


def resolve_tool_call(project: Project, call: Any, *, include_write: bool = False,
                      cross_project: bool = False) -> Dict[str, Any]:
    """:meth:`Tools.resolve` for one project (reads the definitions each time)."""
    return Tools(project, include_write=include_write, cross_project=cross_project).resolve(call)
