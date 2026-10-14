"""Asynchronous use of the SDK. Each call runs the blocking method in a worker thread
(``asyncio.to_thread``), so the SDK stays standard-library only.

::

    import asyncio
    from graphicle_sdk import AsyncClient

    async def main():
        client = AsyncClient.from_session("http://127.0.0.1:8854")
        project = await client.project("notes")
        chats = await project.connector("chats")
        await project.import_documents(chats, [{"id": "s1", "content": "user: We ship on Friday."}])
        packet = await project.context("When do we ship?")
        receipt = await project.forget(chats, ["s1"])

    asyncio.run(main())

Every public method of :class:`~graphicle_sdk.Client`, :class:`~graphicle_sdk.Project`,
:class:`~graphicle_sdk.Memory` and :class:`~graphicle_sdk.Tools` has an awaitable counterpart with the
same arguments and result. Results that are SDK objects come back wrapped, so
``await client.project(...)`` is an :class:`AsyncProject`; other attributes (``id``, ``url``,
``namespace`` ...) are read directly, and ``.sync`` is the wrapped object. Calls may run
concurrently; the service serializes writes, and a :class:`~graphicle_sdk.Memory` caches project and
connector ids in plain dicts, which concurrent threads may fill twice but never corrupt.
"""
from __future__ import annotations

import asyncio
import functools
import os
from typing import Any, Union

from .client import Client, Project
from .memory import Memory
from .tools import Tools

__all__ = ["AsyncClient", "AsyncMemory", "AsyncProject", "AsyncTools"]


class _Async:
    __slots__ = ("_target",)

    def __init__(self, target: Any) -> None:
        self._target = target

    @property
    def sync(self) -> Any:
        """The wrapped blocking object."""
        return self._target

    def __getattr__(self, name: str) -> Any:
        if name == "_target":  # not yet assigned (copying, unpickling): never recurse
            raise AttributeError(name)
        value = getattr(self._target, name)
        if name.startswith("_"):
            return value
        if callable(value) and not isinstance(value, type):
            @functools.wraps(value)
            async def call(*args: Any, **kwargs: Any) -> Any:
                return _wrap(await asyncio.to_thread(value, *args, **kwargs))
            return call
        return _wrap(value)

    def __repr__(self) -> str:
        return f"{type(self).__name__}({self._target!r})"


def _unwrap(value: Any) -> Any:
    return value._target if isinstance(value, _Async) else value


def _wrap(value: Any) -> Any:
    if isinstance(value, Client):
        return AsyncClient.wrap(value)
    if isinstance(value, Project):
        return AsyncProject(value)
    if isinstance(value, Memory):
        return AsyncMemory(value)
    if isinstance(value, Tools):
        return AsyncTools(value)
    return value


class AsyncClient(_Async):
    """:class:`~graphicle_sdk.Client` with awaitable methods; the arguments are the same."""

    __slots__ = ()

    def __init__(self, url: str, workspace: str, capability: str, timeout: float = 60, *,
                 insecure_http: bool = False) -> None:
        super().__init__(Client(url, workspace, capability, timeout, insecure_http=insecure_http))

    @classmethod
    def wrap(cls, client: Client) -> "AsyncClient":
        """Awaitable methods over an existing :class:`~graphicle_sdk.Client`."""
        wrapped = cls.__new__(cls)
        _Async.__init__(wrapped, client)
        return wrapped

    @classmethod
    def from_session(cls, url: str, data_dir: Union[str, "os.PathLike[str]", None] = None,
                     timeout: float = 60, *, insecure_http: bool = False) -> "AsyncClient":
        """Connect with the owner session ``graphicle init`` saved in ``data_dir``."""
        return cls.wrap(Client.from_session(url, data_dir, timeout, insecure_http=insecure_http))


class AsyncProject(_Async):
    """:class:`~graphicle_sdk.Project` with awaitable methods."""

    __slots__ = ()

    def __init__(self, project: Project) -> None:
        super().__init__(project)


class AsyncMemory(_Async):
    """:class:`~graphicle_sdk.Memory` with awaitable methods: ``AsyncMemory(client, **options)``
    takes a (sync or async) client and the options of ``Memory``, or wraps a ``Memory``."""

    __slots__ = ()

    def __init__(self, memory: Any, **options: Any) -> None:
        target = memory if isinstance(memory, Memory) else Memory(_unwrap(memory), **options)
        super().__init__(target)


class AsyncTools(_Async):
    """:class:`~graphicle_sdk.Tools` with awaitable methods: ``AsyncTools(project, include_write=...,
    cross_project=...)`` takes a (sync or async) project, or wraps a ``Tools``."""

    __slots__ = ()

    def __init__(self, tools: Any, **options: Any) -> None:
        target = tools if isinstance(tools, Tools) else Tools(_unwrap(tools), **options)
        super().__init__(target)
