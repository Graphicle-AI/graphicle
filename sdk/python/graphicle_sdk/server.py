"""Run a private local Graphicle service for scripts, notebooks and tests.

::

    from graphicle_sdk import LocalServer, Memory

    with LocalServer("/path/to/graphicle", "./graphicle-data", "models.json") as client:
        project = client.project("notes")
        ...

On entry, :class:`LocalServer` runs ``graphicle --data-dir DATA [--config CONFIG] init`` (which
creates a workspace and ``DATA/local-session.json`` the first time and reuses them afterwards),
starts ``graphicle --data-dir DATA [--config CONFIG] serve --port 0``, waits for the line
``Graphicle is ready at http://127.0.0.1:PORT`` on the server's stderr and returns a
:class:`graphicle_sdk.Client` for the saved workspace. The server binds to loopback only and runs the
background job worker, so ``Project.wait``, ``answer`` and ``observe`` complete. On exit the
process is stopped (SIGTERM, then SIGKILL after ``stop_timeout`` seconds).

``binary`` defaults to ``$GRAPHICLE_BIN``, then ``graphicle`` on ``PATH``. ``config`` is the model
configuration file (see ``graphicle config-example``); without one the service has no models, and
imports, retrieval and forgetting still work. The data directory holds the SQLite store and the
workspace capability, a secret: keep it private. The service's recent stderr lines are available
from :meth:`LocalServer.logs`.
"""
from __future__ import annotations

import collections
import json
import os
import queue
import re
import shutil
import subprocess
import threading
from pathlib import Path
from typing import IO, Any, Deque, Dict, Mapping, Optional, Sequence, Union

from .client import GraphicleError, Client

__all__ = ["LocalServer"]

_READY = re.compile(r"Graphicle is ready at (https?://\S+)")
PathLike = Union[str, "os.PathLike[str]"]


class LocalServer:
    """Context manager that runs ``graphicle serve`` on a free loopback port."""

    def __init__(self, binary: Optional[PathLike], data_dir: PathLike, config: Optional[PathLike] = None,
                 *, name: str = "My workspace", port: int = 0, env: Optional[Mapping[str, str]] = None,
                 startup_timeout: float = 60.0, stop_timeout: float = 10.0, timeout: float = 60.0,
                 serve_args: Sequence[str] = ()) -> None:
        resolved = binary or os.environ.get("GRAPHICLE_BIN") or shutil.which("graphicle")
        if not resolved:
            raise FileNotFoundError("graphicle binary not found; pass binary= or set GRAPHICLE_BIN")
        executable = os.path.expanduser(os.fspath(resolved))
        search_path = env.get("PATH", os.defpath) if env is not None else None
        self.binary = Path(shutil.which(executable, path=search_path) or executable).resolve()
        self.data_dir = Path(data_dir).expanduser()
        self.config = Path(config).expanduser() if config is not None else None
        self.name = name
        self.port = port
        self.env = dict(env) if env is not None else None
        self.startup_timeout = startup_timeout
        self.stop_timeout = stop_timeout
        self.timeout = timeout
        self.serve_args = list(serve_args)
        self.process: Optional[subprocess.Popen] = None
        self.url: Optional[str] = None
        self.session: Optional[Dict[str, Any]] = None
        self.client: Optional[Client] = None
        self._log: Deque[str] = collections.deque(maxlen=400)
        self._reader: Optional[threading.Thread] = None

    def __repr__(self) -> str:
        return f"LocalServer(data_dir={str(self.data_dir)!r}, url={self.url!r})"

    def _command(self, *arguments: str) -> list:
        command = [str(self.binary), "--data-dir", str(self.data_dir)]
        if self.config is not None:
            command += ["--config", str(self.config)]
        return command + list(arguments)

    def start(self) -> Client:
        """Initialise the data directory if needed, start the server and return its client."""
        if self.process is not None:
            raise RuntimeError("This LocalServer is already running")
        init = subprocess.run(self._command("init", "--name", self.name), env=self.env,
                              capture_output=True, text=True, timeout=self.startup_timeout)
        if init.returncode != 0:
            raise GraphicleError(None, f"graphicle init failed: {(init.stderr or init.stdout).strip()}")
        self.session = json.loads((self.data_dir / "local-session.json").read_text(encoding="utf-8"))
        self._log.clear()
        self.process = subprocess.Popen(
            self._command("serve", "--port", str(self.port), *self.serve_args), env=self.env,
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
            text=True, encoding="utf-8", errors="replace")
        announced: "queue.Queue[Optional[str]]" = queue.Queue()
        # Keep draining stderr for the server's lifetime, so a full pipe never blocks it.
        self._reader = threading.Thread(target=self._drain, args=(self.process.stderr, announced),
                                        name="graphicle-serve-stderr", daemon=True)
        self._reader.start()
        try:
            url = announced.get(timeout=self.startup_timeout)
        except queue.Empty:
            url = None
        if url is None:
            code = self.stop()
            raise GraphicleError(None, f"graphicle serve did not become ready (exit code {code}): "
                                 f"{self.logs()[-2000:]}")
        self.url = url
        try:
            self.client = Client(url, self.session["workspace"], self.session["capability"],
                                 timeout=self.timeout)
        except Exception:
            self.stop()
            raise
        return self.client

    def _drain(self, stream: IO[str], announced: "queue.Queue[Optional[str]]") -> None:
        ready = False
        for line in stream:
            self._log.append(line.rstrip("\n"))
            if not ready:
                match = _READY.search(line)
                if match:
                    ready = True
                    announced.put(match.group(1))
        if not ready:
            announced.put(None)

    def stop(self) -> Optional[int]:
        """Stop the server; returns its exit code (``None`` if it was not running)."""
        process, self.process = self.process, None
        if process is None:
            return None
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=self.stop_timeout)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        if self._reader is not None:
            self._reader.join(timeout=5)
            self._reader = None
        if process.stderr is not None:
            process.stderr.close()
        return process.returncode

    def logs(self) -> str:
        """The server's most recent stderr lines."""
        return "\n".join(self._log)

    def __enter__(self) -> Client:
        return self.start()

    def __exit__(self, *exc_info: Any) -> bool:
        self.stop()
        return False
