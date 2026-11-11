from graphicle_sdk import Client, Project, Tools


def test_tools_hides_missing_read_only_hint_unless_writes_are_enabled():
    client = Client("http://127.0.0.1:8854", "workspace-1", "capability")
    project = Project(client, "project-1")
    published = [
        {"name": "read", "annotations": {"readOnlyHint": True}},
        {"name": "write", "annotations": {"readOnlyHint": False}},
        {"name": "unspecified", "annotations": {}},
        {"name": "missing"},
    ]

    safe = Tools(project)
    safe._rpc = lambda method, params=None: {"tools": published}
    assert safe.names() == ["read"]

    opted_in = Tools(project, include_write=True)
    opted_in._rpc = lambda method, params=None: {"tools": published}
    assert opted_in.names() == ["read", "write", "unspecified", "missing"]
