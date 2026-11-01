import openai
import pytest
from openai import OpenAI


@pytest.fixture
def client(server):
    return OpenAI(base_url=server.base_url, api_key="test", max_retries=0)


def user(text):
    return [{"role": "user", "content": text}]


def test_sync_fallback(client):
    r = client.chat.completions.create(model="gpt-4o", messages=user("hello"))
    assert r.choices[0].message.content
    assert r.usage.total_tokens == r.usage.prompt_tokens + r.usage.completion_tokens


def test_scripted_text(server, client):
    server.put_rules([{"match": {"last_user": "ping"}, "reply": {"text": "pong"}}])
    r = client.chat.completions.create(model="gpt-4o", messages=user("ping"))
    assert r.choices[0].message.content == "pong"


def test_stream_with_usage(server, client):
    text = "the quick brown fox jumps"
    server.put_rules([{"reply": {"text": text}}])
    stream = client.chat.completions.create(
        model="gpt-4o", messages=user("go"), stream=True, stream_options={"include_usage": True}
    )
    out, usage = "", None
    for chunk in stream:
        if chunk.choices:
            out += chunk.choices[0].delta.content or ""
        if chunk.usage:
            usage = chunk.usage
    assert out == text
    assert usage is not None and usage.completion_tokens > 0


def test_tool_call(server, client):
    server.put_rules(
        [{"reply": {"tool_calls": [{"name": "get_weather", "arguments": {"city": "Paris"}, "id": "call_1"}]}}]
    )
    r = client.chat.completions.create(
        model="gpt-4o",
        messages=user("weather?"),
        tools=[{"type": "function", "function": {"name": "get_weather", "parameters": {"type": "object"}}}],
    )
    call = r.choices[0].message.tool_calls[0]
    assert r.choices[0].finish_reason == "tool_calls"
    assert call.function.name == "get_weather"
    assert call.function.arguments == '{"city":"Paris"}' or '"Paris"' in call.function.arguments


def test_429_without_retries(server, client):
    server.put_rules([{"reply": {"error": {"status": 429, "message": "slow down"}}}])
    with pytest.raises(openai.RateLimitError) as e:
        client.chat.completions.create(model="gpt-4o", messages=user("x"))
    assert e.value.status_code == 429
    assert "slow down" in str(e.value)
    assert server.ctl("GET", "/requests")["total"] == 1


def test_429_retried_when_allowed(server):
    server.put_rules(
        [
            {"times": 2, "reply": {"error": {"status": 429, "retry_after": 0}}},
            {"reply": {"text": "finally"}},
        ]
    )
    c = OpenAI(base_url=server.base_url, api_key="test", max_retries=2)
    r = c.chat.completions.create(model="gpt-4o", messages=user("x"))
    assert r.choices[0].message.content == "finally"
    assert server.ctl("GET", "/requests")["total"] == 3


def test_request_log(server, client):
    client.chat.completions.create(model="gpt-4o", messages=user("logged"))
    log = server.ctl("GET", "/requests?model=gpt-4o")
    assert log["total"] == 1
    assert "logged" in str(log["requests"][0]["request"])
