"""A small stdlib-only helper: start the fakellm binary, read its URL, reset, rules, requests, stop.

    with start() as llm:
        llm.scripts.put([{"reply": {"text": "pong"}}])
        # OpenAI(base_url=llm.base_url, api_key="x"), Anthropic(base_url=llm.url, api_key="x")
        assert llm.requests.count() == 0
"""

import json
import os
import shutil
import subprocess
import urllib.error
import urllib.parse
import urllib.request

__all__ = ["FakeLLM", "FakeLLMError", "connect", "start"]


class FakeLLMError(Exception):
    def __init__(self, status: int, body):
        super().__init__(f"fakellm: {status} {body}")
        self.status = status
        self.body = body


class _Scripts:
    def __init__(self, llm):
        self._llm = llm

    def put(self, rules, keep_hits: bool = False):
        return self._llm.ctl("PUT", "/scripts" + ("?keep_hits=true" if keep_hits else ""), rules)

    def add(self, rules):
        return self._llm.ctl("POST", "/scripts", rules)

    def list(self):
        return self._llm.ctl("GET", "/scripts")["rules"]

    def clear(self):
        self._llm.ctl("DELETE", "/scripts")


class _Requests:
    def __init__(self, llm):
        self._llm = llm

    def list(self, **filters):
        query = urllib.parse.urlencode({k: v for k, v in filters.items() if v is not None})
        return self._llm.ctl("GET", "/requests" + ("?" + query if query else ""))["requests"]

    def count(self, **filters) -> int:
        return len(self.list(**filters))

    def clear(self):
        self._llm.ctl("DELETE", "/requests")


class FakeLLM:
    def __init__(self, url: str, process: subprocess.Popen | None = None):
        self.url = url.rstrip("/")
        self.base_url = self.url + "/v1"  # OpenAI SDKs; the Anthropic SDK takes `url`
        self._process = process
        self.scripts = _Scripts(self)
        self.requests = _Requests(self)

    def ctl(self, method: str, path: str, body=None):
        """Raw control-plane call; `path` is relative to `/_fakellm`. Parsed JSON, `None` for an empty body."""
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(
            f"{self.url}/_fakellm{path}", data=data, method=method, headers={"content-type": "application/json"}
        )
        try:
            with urllib.request.urlopen(req, timeout=10) as r:
                raw = r.read()
        except urllib.error.HTTPError as e:
            raw = e.read()
            try:
                parsed = json.loads(raw)
            except ValueError:
                parsed = raw.decode(errors="replace")
            raise FakeLLMError(e.code, parsed) from None
        return json.loads(raw) if raw else None

    def reset(self):
        """Clears rules, request log, clock and stored responses."""
        self.ctl("POST", "/reset")

    def stop(self):
        p, self._process = self._process, None
        if p is not None:
            p.terminate()
            try:
                p.wait(timeout=5)
            except subprocess.TimeoutExpired:
                p.kill()
            if p.stdout:
                p.stdout.close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.stop()


def connect(url: str) -> FakeLLM:
    """Attaches to a server somebody else started."""
    return FakeLLM(url)


def start(*args: str, bin: str | None = None) -> FakeLLM:
    """Spawns `fakellm --host 127.0.0.1 --port 0 *args` and reads the URL from its first stdout line."""
    binary = bin or os.environ.get("FAKELLM_BIN") or shutil.which("fakellm")
    if not binary:
        raise FileNotFoundError("fakellm binary not found: pass bin=, set FAKELLM_BIN or put it on PATH")
    p = subprocess.Popen(
        [binary, "--host", "127.0.0.1", "--port", "0", *args],
        stdout=subprocess.PIPE,
        stdin=subprocess.DEVNULL,
        text=True,
    )
    line = p.stdout.readline().strip() if p.stdout else ""
    if not line.startswith("http://"):
        p.kill()
        raise RuntimeError(f"unexpected first line from fakellm: {line!r}")
    return FakeLLM(line, p)
