"""The stdlib helper in fakellm_sdk: spawn, first-line URL, reset, rules, requests, stop."""

import json
import os
import urllib.request
from pathlib import Path

import pytest

from fakellm_sdk import FakeLLMError, connect, start

ROOT = Path(__file__).resolve().parents[3]
BIN = os.environ.get("FAKELLM_BIN") or str(ROOT / "target/debug/fakellm")


def chat(llm, text="hi"):
    body = json.dumps({"model": "m", "messages": [{"role": "user", "content": text}]}).encode()
    req = urllib.request.Request(llm.base_url + "/chat/completions", data=body, headers={"content-type": "application/json"})
    with urllib.request.urlopen(req) as r:
        return json.load(r)


def test_spawn_rules_requests_reset_and_stop():
    llm = start(bin=BIN)
    try:
        assert llm.url.startswith("http://127.0.0.1:") and llm.base_url.endswith("/v1")
        assert llm.scripts.put([{"reply": {"text": "pong"}}]) == {"added": 1, "count": 1}
        assert chat(llm)["choices"][0]["message"]["content"] == "pong"
        assert llm.scripts.list()[0]["hits"] == 1
        assert llm.requests.count() == 1
        assert llm.requests.list(protocol="openai.chat")[0]["protocol"] == "openai.chat"
        llm.reset()
        assert llm.scripts.list() == [] and llm.requests.count() == 0
        with pytest.raises(FakeLLMError) as e:
            llm.scripts.put([{"reply": {"text": "x"}, "match": {"after": "nope"}}])
        assert e.value.status == 400
        other = connect(llm.url)
        assert other.requests.count() == 0
    finally:
        llm.stop()
    llm.stop()  # idempotent
    with pytest.raises(OSError):
        urllib.request.urlopen(llm.url + "/_fakellm", timeout=1)


def test_missing_binary():
    with pytest.raises(FileNotFoundError):
        start(bin="/nonexistent/fakellm")
