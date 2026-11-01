package fakellm_test

import (
	"archive/tar"
	"bufio"
	"bytes"
	"compress/gzip"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AkaraChen/fakellm/go"
)

func binPath(t *testing.T) string {
	t.Helper()
	if p := os.Getenv("FAKELLM_BIN"); p != "" {
		return p
	}
	p, _ := filepath.Abs("../target/debug/fakellm")
	if _, err := os.Stat(p); err != nil {
		t.Skip("fakellm binary not found: cargo build, or set FAKELLM_BIN")
	}
	return p
}

func start(t *testing.T, opts fakellm.Options) (*fakellm.Server, context.Context) {
	t.Helper()
	opts.Bin = binPath(t)
	ctx := context.Background()
	s, err := fakellm.Start(ctx, opts)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = s.Stop() })
	return s, ctx
}

func chat(t *testing.T, s *fakellm.Server, prompt string, stream bool) (*http.Response, error) {
	t.Helper()
	body, _ := json.Marshal(map[string]any{
		"model": "m", "stream": stream,
		"messages": []map[string]string{{"role": "user", "content": prompt}},
	})
	return http.Post(s.BaseURL()+"/chat/completions", "application/json", bytes.NewReader(body))
}

// sseText reads a chat completion SSE body and joins the content deltas.
func sseText(t *testing.T, r io.Reader) string {
	t.Helper()
	var sb strings.Builder
	sc := bufio.NewScanner(r)
	done := false
	for sc.Scan() {
		data, ok := strings.CutPrefix(sc.Text(), "data: ")
		if !ok {
			continue
		}
		if data == "[DONE]" {
			done = true
			continue
		}
		var ev struct {
			Choices []struct {
				Delta struct{ Content string } `json:"delta"`
			} `json:"choices"`
		}
		if err := json.Unmarshal([]byte(data), &ev); err != nil {
			t.Fatalf("bad event %q: %v", data, err)
		}
		for _, c := range ev.Choices {
			sb.WriteString(c.Delta.Content)
		}
	}
	if !done {
		t.Fatal("stream ended without [DONE]")
	}
	return sb.String()
}

func TestStartStop(t *testing.T) {
	s, ctx := start(t, fakellm.Options{Models: []string{"a", "b"}})
	if !strings.HasPrefix(s.URL(), "http://127.0.0.1:") || s.BaseURL() != s.URL()+"/v1" {
		t.Fatalf("url %q", s.URL())
	}
	st, err := s.Status(ctx)
	if err != nil || st.Version == "" || st.Scripts.Count != 0 || st.Strict {
		t.Fatalf("status %+v %v", st, err)
	}
	res, err := http.Get(s.BaseURL() + "/models")
	if err != nil {
		t.Fatal(err)
	}
	b, _ := io.ReadAll(res.Body)
	res.Body.Close()
	if !strings.Contains(string(b), `"b"`) {
		t.Fatalf("models %s", b)
	}
	if err := s.Stop(); err != nil {
		t.Fatal(err)
	}
	if err := s.Stop(); err != nil {
		t.Fatal(err)
	}
	if _, err := s.Status(ctx); err == nil {
		t.Fatal("server still answers after Stop")
	}
}

func TestStartErrors(t *testing.T) {
	if _, err := fakellm.Start(context.Background(), fakellm.Options{Bin: "/nonexistent/fakellm"}); err == nil {
		t.Fatal("expected error for a missing binary")
	}
	bin := binPath(t)
	_, err := fakellm.Start(context.Background(), fakellm.Options{Bin: bin, Scripts: `[{"reply":{}}]`})
	if err == nil || !strings.Contains(err.Error(), "exited") {
		t.Fatalf("invalid scripts should fail the start, got %v", err)
	}
}

func TestScriptsAndChat(t *testing.T) {
	s, ctx := start(t, fakellm.Options{Scripts: "- match: {last_user: ping}\n  reply: {text: pong}\n"})
	rules, err := s.Scripts().List(ctx)
	if err != nil || len(rules) != 1 || rules[0].Match.LastUser != "ping" || rules[0].Reply.Text == nil || *rules[0].Reply.Text != "pong" {
		t.Fatalf("rules %+v %v", rules, err)
	}

	res, err := chat(t, s, "ping", true)
	if err != nil {
		t.Fatal(err)
	}
	defer res.Body.Close()
	if got := sseText(t, res.Body); got != "pong" {
		t.Fatalf("stream text %q", got)
	}

	_, err = s.Scripts().Put(ctx, []fakellm.Rule{{
		Name:  "weather",
		Match: fakellm.Match{LastUser: "weather"},
		Reply: fakellm.ReplySpec{ToolCalls: []fakellm.ToolCallSpec{{Name: "get_weather", Arguments: map[string]any{"city": "Paris"}}}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	n, err := s.Scripts().Add(ctx, "- reply: {text: catch-all}")
	if err != nil || n != 2 {
		t.Fatalf("add: %d %v", n, err)
	}
	res2, err := chat(t, s, "weather?", false)
	if err != nil {
		t.Fatal(err)
	}
	defer res2.Body.Close()
	var out struct {
		Choices []struct {
			Message struct {
				ToolCalls []struct {
					Function struct{ Name, Arguments string }
				} `json:"tool_calls"`
			}
		}
	}
	if err := json.NewDecoder(res2.Body).Decode(&out); err != nil {
		t.Fatal(err)
	}
	tc := out.Choices[0].Message.ToolCalls
	if len(tc) != 1 || tc[0].Function.Name != "get_weather" || tc[0].Function.Arguments != `{"city":"Paris"}` {
		t.Fatalf("tool calls %+v", tc)
	}
	rules, _ = s.Scripts().List(ctx)
	if len(rules) != 2 || rules[0].Hits != 1 || rules[0].Name != "weather" {
		t.Fatalf("rules %+v", rules)
	}

	// An invalid document is rejected as a whole.
	_, err = s.Scripts().Put(ctx, `[{"match":{"bogus":1},"reply":{"text":"x"}}]`)
	var apiErr *fakellm.Error
	if !errorsAs(err, &apiErr) || apiErr.Status != 400 {
		t.Fatalf("want 400, got %v", err)
	}
	if err := s.Scripts().Clear(ctx); err != nil {
		t.Fatal(err)
	}
	if rules, _ = s.Scripts().List(ctx); len(rules) != 0 {
		t.Fatalf("rules after clear: %+v", rules)
	}
}

func TestStrictAndAPIKey(t *testing.T) {
	s, ctx := start(t, fakellm.Options{Strict: true, APIKey: "sk-test"})
	st, _ := s.Status(ctx)
	if !st.Strict {
		t.Fatal("strict not set")
	}
	res, err := chat(t, s, "hi", false) // no key
	if err != nil {
		t.Fatal(err)
	}
	res.Body.Close()
	if res.StatusCode != 401 {
		t.Fatalf("status %d, want 401", res.StatusCode)
	}
	body, _ := json.Marshal(map[string]any{"model": "m", "messages": []map[string]string{{"role": "user", "content": "hi"}}})
	req, _ := http.NewRequest("POST", s.BaseURL()+"/chat/completions", bytes.NewReader(body))
	req.Header.Set("Authorization", "Bearer sk-test")
	res, err = http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	res.Body.Close()
	if res.StatusCode != 400 { // strict: no rule matches
		t.Fatalf("status %d, want 400", res.StatusCode)
	}
}

func TestClockGatesStream(t *testing.T) {
	s, ctx := start(t, fakellm.Options{})
	if _, err := s.Scripts().Put(ctx, `[{"reply":{"text":"slow","stream":{"ttft_ms":500}}}]`); err != nil {
		t.Fatal(err)
	}
	at := time.Date(2030, 1, 2, 3, 4, 5, 0, time.UTC)
	st, err := s.Clock().Freeze(ctx, at)
	if err != nil || !st.Frozen || !st.Now.Equal(at) {
		t.Fatalf("freeze %+v %v", st, err)
	}
	res, err := chat(t, s, "x", true)
	if err != nil {
		t.Fatal(err)
	}
	defer res.Body.Close()
	got := make(chan string, 1)
	go func() { got <- sseText(t, res.Body) }()
	select {
	case v := <-got:
		t.Fatalf("stream finished while the clock is frozen: %q", v)
	case <-time.After(200 * time.Millisecond):
	}
	st, err = s.Clock().Advance(ctx, 500*time.Millisecond)
	if err != nil || !st.Now.Equal(at.Add(500*time.Millisecond)) {
		t.Fatalf("advance %+v %v", st, err)
	}
	select {
	case v := <-got:
		if v != "slow" {
			t.Fatalf("text %q", v)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("stream not released by advance")
	}

	if st, _ = s.Clock().Set(ctx, at); !st.Now.Equal(at) {
		t.Fatalf("set %+v", st)
	}
	if st, _ = s.Clock().Unfreeze(ctx); st.Frozen || !st.Shifted {
		t.Fatalf("unfreeze %+v", st)
	}
	if st, _ = s.Clock().Reset(ctx); st.Shifted {
		t.Fatalf("reset %+v", st)
	}
	if st, err = s.Clock().Now(ctx); err != nil || time.Since(st.Now) > time.Minute {
		t.Fatalf("now %+v %v", st, err)
	}
}

func TestRequestLogAndReset(t *testing.T) {
	s, ctx := start(t, fakellm.Options{})
	if _, err := s.Scripts().Put(ctx, []fakellm.Rule{{Name: "r1", Match: fakellm.Match{LastUser: "one"}, Reply: fakellm.ReplySpec{Text: fakellm.Ptr("1")}}}); err != nil {
		t.Fatal(err)
	}
	for _, p := range []string{"one", "two", "one"} {
		res, err := chat(t, s, p, p == "two")
		if err != nil {
			t.Fatal(err)
		}
		io.Copy(io.Discard, res.Body)
		res.Body.Close()
	}
	n, err := s.Requests().Count(ctx, fakellm.Filter{})
	if err != nil || n != 3 {
		t.Fatalf("count %d %v", n, err)
	}
	recs, err := s.Requests().List(ctx, fakellm.Filter{Rule: "r1", Full: true})
	if err != nil || len(recs) != 2 {
		t.Fatalf("records %d %v", len(recs), err)
	}
	r := recs[0]
	if r.Protocol != "openai.chat" || r.Path != "/v1/chat/completions" || r.Status != 200 || r.Source.Kind != "script" ||
		r.Reply == nil || r.Reply.Text != "1" || len(r.Raw) == 0 || len(r.Headers) == 0 || r.Time.IsZero() {
		t.Fatalf("record %+v", r)
	}
	if n, _ = s.Requests().Count(ctx, fakellm.Filter{Source: "fallback", Stream: fakellm.Ptr(true)}); n != 1 {
		t.Fatalf("fallback stream count %d", n)
	}
	if recs, _ = s.Requests().List(ctx, fakellm.Filter{After: recs[0].Seq, Limit: 1}); len(recs) != 1 || recs[0].Seq < 2 {
		t.Fatalf("after/limit: %+v", recs)
	}
	if _, err := s.Requests().List(ctx, fakellm.Filter{Protocol: "nope"}); err == nil {
		t.Fatal("bad protocol accepted")
	}
	cfg, err := s.Requests().Config(ctx)
	if err != nil || !cfg.Enabled || cfg.Stored != 3 {
		t.Fatalf("config %+v %v", cfg, err)
	}

	if err := s.Requests().Clear(ctx); err != nil {
		t.Fatal(err)
	}
	if n, _ = s.Requests().Count(ctx, fakellm.Filter{}); n != 0 {
		t.Fatalf("count after clear %d", n)
	}

	res, _ := chat(t, s, "one", false)
	res.Body.Close()
	if err := s.Reset(ctx, fakellm.ResetOptions{KeepScripts: true}); err != nil {
		t.Fatal(err)
	}
	rules, _ := s.Scripts().List(ctx)
	n, _ = s.Requests().Count(ctx, fakellm.Filter{})
	if len(rules) != 1 || n != 0 {
		t.Fatalf("after reset: %d rules, %d requests", len(rules), n)
	}
	if err := s.Reset(ctx, fakellm.ResetOptions{}); err != nil {
		t.Fatal(err)
	}
	if rules, _ = s.Scripts().List(ctx); len(rules) != 0 {
		t.Fatalf("rules after full reset: %+v", rules)
	}
}

func TestResponses(t *testing.T) {
	s, ctx := start(t, fakellm.Options{})
	res, err := http.Post(s.BaseURL()+"/responses", "application/json", strings.NewReader(`{"model":"m","input":"hi"}`))
	if err != nil {
		t.Fatal(err)
	}
	res.Body.Close()
	list, err := s.Responses().List(ctx)
	if err != nil || len(list) != 1 || !strings.HasPrefix(list[0].ID, "resp_") || list[0].CreatedAt.IsZero() {
		t.Fatalf("responses %+v %v", list, err)
	}
	if n, err := s.Responses().Clear(ctx); err != nil || n != 1 {
		t.Fatalf("clear %d %v", n, err)
	}
}

func TestDownload(t *testing.T) {
	var tarball bytes.Buffer
	gz := gzip.NewWriter(&tarball)
	tw := tar.NewWriter(gz)
	payload := []byte("#!/bin/sh\necho hi\n")
	for _, f := range []struct {
		name string
		data []byte
	}{{"fakellm-x/README.md", []byte("readme")}, {"fakellm-x/fakellm", payload}} {
		tw.WriteHeader(&tar.Header{Name: f.name, Mode: 0o755, Size: int64(len(f.data)), Typeflag: tar.TypeReg})
		tw.Write(f.data)
	}
	tw.Close()
	gz.Close()

	hits := 0
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits++
		if !strings.HasPrefix(r.URL.Path, "/v9.9.9/fakellm-") || !strings.HasSuffix(r.URL.Path, "-unknown-linux-musl.tar.gz") {
			http.NotFound(w, r)
			return
		}
		w.Write(tarball.Bytes())
	}))
	defer srv.Close()
	old := fakellm.ReleaseBaseURL
	fakellm.ReleaseBaseURL = srv.URL
	defer func() { fakellm.ReleaseBaseURL = old }()

	cache := t.TempDir()
	p, err := fakellm.Download(context.Background(), "9.9.9", cache)
	if err != nil {
		t.Skipf("download unsupported here: %v", err) // non-linux or unknown arch
	}
	got, _ := os.ReadFile(p)
	st, _ := os.Stat(p)
	if !bytes.Equal(got, payload) || st.Mode()&0o111 == 0 {
		t.Fatalf("extracted %q mode %v", got, st.Mode())
	}
	before := hits
	if p2, err := fakellm.Download(context.Background(), "v9.9.9", cache); err != nil || p2 != p || hits != before {
		t.Fatalf("second download should hit the cache: %v %q hits %d->%d", err, p2, before, hits)
	}
	if _, err := fakellm.Download(context.Background(), "1.0.0", cache); err == nil {
		t.Fatal("expected 404 error")
	}
}

func errorsAs(err error, target **fakellm.Error) bool { return errors.As(err, target) }

func TestLogConfigWireOptionsAndScriptFile(t *testing.T) {
	s, ctx := start(t, fakellm.Options{Host: "0.0.0.0", RequestLogCapacity: 7, RedactAuth: true, TTFT: "1ms", TokensPerSec: 1000,
		Scripts: []fakellm.Rule{{Reply: fakellm.ReplySpec{Text: fakellm.Ptr("")}}}})
	if !strings.HasPrefix(s.URL(), "http://127.0.0.1:") || s.AnthropicBaseURL() != s.URL() {
		t.Fatalf("url %q", s.URL())
	}
	cfg, err := s.Requests().Config(ctx)
	if err != nil || cfg.Capacity != 7 || !cfg.Enabled {
		t.Fatalf("config %+v %v", cfg, err)
	}
	cfg, err = s.Requests().SetConfig(ctx, fakellm.LogUpdate{Enabled: fakellm.Ptr(false), Capacity: fakellm.Ptr(3)})
	if err != nil || cfg.Enabled || cfg.Capacity != 3 {
		t.Fatalf("set %+v %v", cfg, err)
	}
	out, err := s.Wire(ctx, "claude", "shell")
	if err != nil || !strings.Contains(out, "ANTHROPIC_BASE_URL") {
		t.Fatalf("wire %q %v", out, err)
	}
	if _, err := s.Wire(ctx, "nope", ""); err == nil {
		t.Fatal("wire of an unknown target should fail")
	}
	n, err := s.Scripts().Put(ctx, "- reply: {text: a}\n- reply: {text: b}\n")
	if err != nil || n != 2 {
		t.Fatalf("put count %d %v", n, err)
	}
}

func TestCustomEndpointRule(t *testing.T) {
	s, ctx := start(t, fakellm.Options{})
	_, err := s.Scripts().Put(ctx, []fakellm.Rule{{
		Match: fakellm.Match{Path: "/v1/systemone", Body: "angry"},
		Reply: fakellm.ReplySpec{Body: json.RawMessage(`{"mood":"x"}`), Status: 202, Headers: map[string]string{"x-c": "y"}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	res, err := http.Post(s.URL()+"/v1/systemone", "application/json", strings.NewReader(`{"state":"angry"}`))
	if err != nil {
		t.Fatal(err)
	}
	defer res.Body.Close()
	b, _ := io.ReadAll(res.Body)
	if res.StatusCode != 202 || res.Header.Get("x-c") != "y" || !strings.Contains(string(b), `"mood":"x"`) {
		t.Fatalf("got %d %v %s", res.StatusCode, res.Header, b)
	}
}

func TestRawRuleReplaysAnSSEStream(t *testing.T) {
	s, ctx := start(t, fakellm.Options{})
	body := "data: {\"n\":1}\n\ndata: [DONE]\n\n"
	_, err := s.Scripts().Put(ctx, []fakellm.Rule{{
		Match: fakellm.Match{Path: "/v1/chat/completions", Method: "post", Header: map[string]string{"x-replay": "^yes$"}, Nth: fakellm.Ptr(uint32(1))},
		Reply: fakellm.ReplySpec{Raw: &fakellm.RawReply{Status: 203, ContentType: "text/event-stream", Body: body, Headers: map[string]string{"x-c": "y"}}},
	}})
	if err != nil {
		t.Fatal(err)
	}
	post := func(replay string) (*http.Response, string) {
		req, _ := http.NewRequest("POST", s.URL()+"/v1/chat/completions", strings.NewReader(`{"model":"m","stream":true,"messages":[{"role":"user","content":"hi"}]}`))
		req.Header.Set("content-type", "application/json")
		req.Header.Set("x-replay", replay)
		res, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		defer res.Body.Close()
		b, _ := io.ReadAll(res.Body)
		return res, string(b)
	}
	res, got := post("yes")
	if res.StatusCode != 203 || res.Header.Get("x-c") != "y" || res.Header.Get("content-type") != "text/event-stream" || got != body {
		t.Fatalf("got %d %v %q", res.StatusCode, res.Header, got)
	}
	// nth: 1 is spent and the header gate holds: generated replies from here on.
	if res, got = post("yes"); res.StatusCode != 200 || got == body {
		t.Fatalf("second request: %d %q", res.StatusCode, got)
	}
}

func TestRuntimeOptionsAndLastUserRaw(t *testing.T) {
	// MaxSnapshots reaches the binary: the second snapshot is refused.
	s, ctx := start(t, fakellm.Options{MaxSnapshots: 1, PolicyTimeoutMs: 1000, UpstreamTimeoutSecs: 0})
	if err := s.Scripts().Clear(ctx); err != nil {
		t.Fatal(err)
	}
	post := func(name string) int {
		res, err := http.Post(s.URL()+"/_fakellm/snapshots", "application/json", strings.NewReader(`{"name":"`+name+`"}`))
		if err != nil {
			t.Fatal(err)
		}
		res.Body.Close()
		return res.StatusCode
	}
	if a, b := post("one"), post("two"); a != 201 || b != 409 {
		t.Fatalf("snapshots: %d then %d", a, b)
	}
	// Match.LastUserRaw sees the reminder block that last_user strips.
	_, err := s.Scripts().Put(ctx, []fakellm.Rule{
		{Match: fakellm.Match{LastUserRaw: "^<system-reminder>"}, Reply: fakellm.ReplySpec{Text: fakellm.Ptr("raw")}},
		{Reply: fakellm.ReplySpec{Text: fakellm.Ptr("cooked")}},
	})
	if err != nil {
		t.Fatal(err)
	}
	body := `{"model":"claude-sonnet-4-5","max_tokens":8,"messages":[{"role":"user","content":[{"type":"text","text":"<system-reminder>x</system-reminder>"},{"type":"text","text":"q"}]}]}`
	req, _ := http.NewRequest("POST", s.AnthropicBaseURL()+"/v1/messages", strings.NewReader(body))
	req.Header.Set("content-type", "application/json")
	req.Header.Set("anthropic-version", "2023-06-01")
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer res.Body.Close()
	b, _ := io.ReadAll(res.Body)
	if !strings.Contains(string(b), `"raw"`) {
		t.Fatalf("got %s", b)
	}
}
