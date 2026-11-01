"""Starts the freshly built fakellm binary once per session and resets it before every test."""

import os
import subprocess
from pathlib import Path

import httpx
import pytest

ROOT = Path(__file__).resolve().parents[3]


class Server:
    def __init__(self, url: str):
        self.url = url
        self.base_url = f"{url}/v1"

    def ctl(self, method: str, path: str, body=None):
        r = httpx.request(method, f"{self.url}/_fakellm{path}", json=body, timeout=10)
        r.raise_for_status()
        return r.json() if r.content else None

    def put_rules(self, rules):
        return self.ctl("PUT", "/scripts", rules)


@pytest.fixture(scope="session")
def server():
    binary = os.environ.get("FAKELLM_BIN") or str(ROOT / "target/debug/fakellm")
    proc = subprocess.Popen(
        [binary, "--host", "127.0.0.1", "--port", "0"],
        stdout=subprocess.PIPE,
        stdin=subprocess.DEVNULL,
        text=True,
    )
    try:
        url = proc.stdout.readline().strip()
        assert url.startswith("http://"), f"unexpected first line: {url!r}"
        yield Server(url)
    finally:
        proc.kill()
        proc.wait()


@pytest.fixture(autouse=True)
def fresh(server):
    server.ctl("POST", "/reset")
    yield server
