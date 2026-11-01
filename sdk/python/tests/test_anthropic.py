import anthropic
import pytest
from anthropic import Anthropic

MODEL = "claude-opus-4-7"


@pytest.fixture
def client(server):
    return Anthropic(base_url=server.url, api_key="test", max_retries=0)


def user(text):
    return [{"role": "user", "content": text}]


def test_sync_fallback(client):
    r = client.messages.create(model=MODEL, max_tokens=256, messages=user("hello"))
    assert r.content[0].type == "text" and r.content[0].text
    assert r.stop_reason == "end_turn"
    assert r.usage.input_tokens > 0


def test_scripted_text(server, client):
    server.put_rules([{"match": {"last_user": "ping"}, "reply": {"text": "pong"}}])
    r = client.messages.create(model=MODEL, max_tokens=64, messages=user("ping"))
    assert r.content[0].text == "pong"


def test_stream_text(server, client):
    text = "the quick brown fox jumps"
    server.put_rules([{"reply": {"text": text}}])
    with client.messages.stream(model=MODEL, max_tokens=64, messages=user("go")) as s:
        out = "".join(s.text_stream)
        final = s.get_final_message()
    assert out == text
    assert final.stop_reason == "end_turn"


def test_tool_use(server, client):
    server.put_rules(
        [{"reply": {"tool_calls": [{"name": "get_weather", "arguments": {"city": "Paris"}, "id": "toolu_1"}]}}]
    )
    tools = [{"name": "get_weather", "description": "w", "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}]
    r = client.messages.create(model=MODEL, max_tokens=128, messages=user("weather?"), tools=tools)
    assert r.stop_reason == "tool_use"
    block = next(b for b in r.content if b.type == "tool_use")
    assert block.input == {"city": "Paris"}
    with client.messages.stream(model=MODEL, max_tokens=128, messages=user("weather?"), tools=tools) as s:
        final = s.get_final_message()
    assert next(b for b in final.content if b.type == "tool_use").input == {"city": "Paris"}


def test_stream_with_thinking(server, client):
    server.put_rules([{"reply": {"reasoning": "let me think", "text": "answer"}}])
    with client.messages.stream(
        model=MODEL,
        max_tokens=4096,
        thinking={"type": "enabled", "budget_tokens": 1024},
        messages=user("why?"),
    ) as s:
        final = s.get_final_message()
    assert final.content[0].type == "thinking"
    assert final.content[0].thinking == "let me think"
    assert final.content[0].signature
    assert final.content[1].text == "answer"


def test_429_without_retries(server, client):
    server.put_rules([{"reply": {"error": {"status": 429, "message": "slow down"}}}])
    with pytest.raises(anthropic.RateLimitError) as e:
        client.messages.create(model=MODEL, max_tokens=16, messages=user("x"))
    assert e.value.status_code == 429
    assert server.ctl("GET", "/requests")["total"] == 1


def test_529_overloaded(server, client):
    server.put_rules([{"reply": {"error": {"status": 529, "message": "busy"}}}])
    with pytest.raises(anthropic.APIStatusError) as e:
        client.messages.create(model=MODEL, max_tokens=16, messages=user("x"))
    assert e.value.status_code == 529
    assert e.value.body["error"]["type"] == "overloaded_error"


def test_request_log(server, client):
    client.messages.create(
        model=MODEL,
        max_tokens=16,
        system="You are terse.",
        messages=user("hi"),
        tools=[{"name": "lookup", "description": "d", "input_schema": {"type": "object", "properties": {}}}],
    )
    log = server.ctl("GET", f"/requests?model={MODEL}")
    assert log["total"] == 1
    req = log["requests"][0]["request"]
    assert "You are terse." in str(req)
    assert "lookup" in str(req["tools"])
