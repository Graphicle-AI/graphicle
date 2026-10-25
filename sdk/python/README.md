# Graphicle for Python

Python 3.10+, standard library only. This is a source package, not published
to a package registry. Version 0.1 covers the Graphicle 0.1 service, including
its product-workspace methods. From the root of a Graphicle checkout:

~~~sh
python -m pip install ./sdk/python
~~~

The distribution is `graphicle-sdk` and the import package is `graphicle_sdk`
(the PyPI name `graphicle` belongs to an unrelated project).

~~~python
import os
from graphicle_sdk import Client

client = Client(
    os.environ["GRAPHICLE_URL"],
    workspace=os.environ["GRAPHICLE_WORKSPACE"],
    capability=os.environ["GRAPHICLE_KEY"],
)
project = client.find_project("my-project")  # existing project key
if project is None:
    raise RuntimeError("Connect or create the project first")
packet = project.context("Where is authentication checked?", max_tokens=3000)
print(packet["evidence"])
~~~

`client.project(key, name)` explicitly registers or reuses a project;
`find_project(key)` only discovers an existing one. The TypeScript SDK's
`project(id)` takes an existing repository ID instead.

`Project` includes context, answer, read, connector imports, reviewed memories,
workflow jobs, graph history and change reports. Use
`graph_source(episode, path)` to read a retained original at a recorded graph
episode.

`client.usage()` and `client.set_budget(monthly_requests)` require owner access.
`None` removes the owner's additional cap without raising a subscription's
allowance. `project.export_memories()` remains available at the request limit.
The service's `/docs` and `/v1/openapi.json` describe the versioned transport.

For async applications, `AsyncClient` exposes the same public methods using
worker threads. `LocalServer` manages a private native service for local use and
tests; `Memory` offers the Memory facade (add, search, get, update, delete,
history and reset over stored sessions), with its behaviour documented in its
module. `Tools` adapts the service's MCP tools to model tool schemas.

Requests do not follow credential-bearing redirects or automatically retry
mutations. A timed-out write may have completed; inspect current state before
resubmitting. A refused request raises `GraphicleError` with `status`, `message`,
the parsed `body` and, when the service sends `Retry-After` (a spent monthly
budget, for example), `retry_after` in seconds. Never put an access capability
in a URL or public frontend bundle.

Licensed under the Apache License 2.0; see LICENSE and NOTICE. The native
server and optional model/runtime dependencies are separate artifacts with
their own notices; this SDK does not bundle them.
