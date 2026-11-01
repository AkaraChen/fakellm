package fakellm_test

import (
	"bytes"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"os/exec"
	"strings"
	"testing"

	"github.com/AkaraChen/fakellm/go"
)

func chatBody(t *testing.T, s *fakellm.Server, base string, msgs []map[string]any) string {
	t.Helper()
	body, _ := json.Marshal(map[string]any{"model": "m", "messages": msgs})
	res, err := http.Post(base+"/chat/completions", "application/json", bytes.NewReader(body))
	if err != nil {
		t.Fatal(err)
	}
	defer res.Body.Close()
	b, _ := io.ReadAll(res.Body)
	return res.Status + " " + string(b)
}

func TestTrajectories(t *testing.T) {
	tr := fakellm.Trajectory{
		Name: "t1", UnknownTool: "pass",
		Steps: []fakellm.TrajectoryStep{
			{Reply: fakellm.ReplySpec{ToolCalls: []fakellm.ToolCallSpec{{Name: "shell", ID: "c1", Arguments: map[string]any{"cmd": "echo hi"}}}}},
			{Reply: fakellm.ReplySpec{Text: fakellm.Ptr("all done")}, Expect: &fakellm.Expect{ToolResults: []fakellm.ToolResultExpect{{Name: "shell", Contains: "hi"}}}},
		},
	}
	s, ctx := start(t, fakellm.Options{
		Trajectories: tr,
		Scripts:      []fakellm.Rule{{Match: fakellm.Match{Any: fakellm.Ptr(true)}, Reply: fakellm.ReplySpec{Trajectory: "t1"}}},
	})
	list, err := s.Trajectories().List(ctx)
	if err != nil || len(list) != 1 || list[0].Steps != 2 || list[0].OnDivergence != "strict" {
		t.Fatalf("list: %+v %v", list, err)
	}
	user := map[string]any{"role": "user", "content": "go"}
	out := chatBody(t, s, s.BaseURL(), []map[string]any{user})
	if !strings.Contains(out, `"shell"`) {
		t.Fatalf("step 1: %s", out)
	}
	call := map[string]any{"role": "assistant", "tool_calls": []map[string]any{{"id": "c1", "type": "function", "function": map[string]any{"name": "shell", "arguments": `{"cmd":"echo hi"}`}}}}
	bad := chatBody(t, s, s.BaseURL(), []map[string]any{user, call, {"role": "tool", "tool_call_id": "c1", "content": "nope"}})
	if !strings.HasPrefix(bad, "400") || !strings.Contains(bad, "diverged") {
		t.Fatalf("expected divergence: %s", bad)
	}
	good := chatBody(t, s, s.BaseURL(), []map[string]any{user, call, {"role": "tool", "tool_call_id": "c1", "content": "hi"}})
	if !strings.Contains(good, "all done") {
		t.Fatalf("step 2: %s", good)
	}
	info, _ := s.Trajectories().List(ctx)
	if info[0].Hits != 3 || info[0].LastStep == nil || *info[0].LastStep != 1 {
		t.Fatalf("info: %+v", info[0])
	}
	got, err := s.Trajectories().Get(ctx, "t1")
	if err != nil || len(got.Steps) != 2 {
		t.Fatalf("get: %+v %v", got, err)
	}
	jl, err := s.Trajectories().JSONL(ctx, "t1")
	if err != nil || !strings.Contains(jl, "t1") {
		t.Fatalf("jsonl: %q %v", jl, err)
	}

	// JSONL and YAML documents, Add replacing by name.
	n, err := s.Trajectories().Put(ctx, "{\"name\":\"a\",\"steps\":[{\"reply\":{\"text\":\"x\"}}]}\n{\"name\":\"b\",\"steps\":[{\"reply\":{\"text\":\"y\"}}]}\n")
	if err != nil || n != 2 {
		t.Fatalf("jsonl put: %d %v", n, err)
	}
	n, err = s.Trajectories().Add(ctx, "name: a\nsteps:\n  - reply: {text: z}\n")
	if err != nil || n != 2 {
		t.Fatalf("yaml add: %d %v", n, err)
	}
	if err := s.Trajectories().Delete(ctx, "a"); err != nil {
		t.Fatal(err)
	}
	var e *fakellm.Error
	if err := s.Trajectories().Delete(ctx, "a"); !errors.As(err, &e) || e.Status != 404 {
		t.Fatalf("want 404, got %v", err)
	}
	if _, err := s.Trajectories().Put(ctx, fakellm.Trajectory{Name: "x"}); !errors.As(err, &e) || e.Status != 400 {
		t.Fatalf("want 400 for empty trajectory, got %v", err)
	}
	if err := s.Trajectories().Clear(ctx); err != nil {
		t.Fatal(err)
	}
	if list, _ := s.Trajectories().List(ctx); len(list) != 0 {
		t.Fatalf("not cleared: %+v", list)
	}
}

func TestPolicies(t *testing.T) {
	// No runtime named anywhere: the policy runs on the runtime embedded in the binary, in-process.
	src := `export default function decide(request, context) {
  context.log("hit", context.hits);
  return { text: "n=" + context.hits, state: { n: context.hits } };
}`
	s, ctx := start(t, fakellm.Options{
		Policies: []fakellm.Policy{{Name: "counter", Source: src}},
		Scripts:  []fakellm.Rule{{Match: fakellm.Match{Any: fakellm.Ptr(true)}, Reply: fakellm.ReplySpec{Policy: "counter"}}},
	})
	user := []map[string]any{{"role": "user", "content": "hi"}}
	if out := chatBody(t, s, s.BaseURL(), user); !strings.Contains(out, "n=0") {
		t.Fatal(out)
	}
	if out := chatBody(t, s, s.BaseURL(), user); !strings.Contains(out, "n=1") {
		t.Fatal(out)
	}
	p, err := s.Policies().Get(ctx, "counter")
	if err != nil || p.Hits != 2 || p.Runtime != "tjs" || !p.InProcess || !strings.Contains(string(p.State), `"n":1`) || len(p.Log) == 0 {
		t.Fatalf("get: %+v %v", p, err)
	}
	var e *fakellm.Error
	_, err = s.Policies().Add(ctx, fakellm.Policy{Name: "broken", Source: "export default 5; throw new Error('boom')"})
	if !errors.As(err, &e) || e.Status != 400 {
		t.Fatalf("want 400, got %v", err)
	}
	list, _ := s.Policies().List(ctx)
	if len(list) != 1 {
		t.Fatalf("list: %+v", list)
	}
	if err := s.Policies().Delete(ctx, "counter"); err != nil {
		t.Fatal(err)
	}
	if err := s.Policies().Clear(ctx); err != nil {
		t.Fatal(err)
	}
}

func TestPoliciesSubprocessRuntime(t *testing.T) {
	rt := "node"
	if _, err := exec.LookPath(rt); err != nil {
		if _, err := exec.LookPath("bun"); err != nil {
			t.Skip("no node or bun")
		}
		rt = "bun"
	}
	s, ctx := start(t, fakellm.Options{
		PolicyRuntime: rt,
		Policies:      []fakellm.Policy{{Name: "sub", Source: "export default () => ({ text: 'from sub' });"}},
		Scripts:       []fakellm.Rule{{Match: fakellm.Match{Any: fakellm.Ptr(true)}, Reply: fakellm.ReplySpec{Policy: "sub"}}},
	})
	if out := chatBody(t, s, s.BaseURL(), []map[string]any{{"role": "user", "content": "hi"}}); !strings.Contains(out, "from sub") {
		t.Fatal(out)
	}
	p, err := s.Policies().Get(ctx, "sub")
	if err != nil || p.Runtime != rt || p.InProcess {
		t.Fatalf("get: %+v %v", p, err)
	}
}

func TestPathPrefix(t *testing.T) {
	s, _ := start(t, fakellm.Options{PathPrefix: "anthropic/"})
	if !strings.HasSuffix(s.BaseURL(), "/anthropic/v1") || !strings.HasSuffix(s.AnthropicBaseURL(), "/anthropic") {
		t.Fatalf("urls: %s %s", s.BaseURL(), s.AnthropicBaseURL())
	}
	if out := chatBody(t, s, s.BaseURL(), []map[string]any{{"role": "user", "content": "hi"}}); !strings.HasPrefix(out, "200") {
		t.Fatal(out)
	}
	if out := chatBody(t, s, s.URL()+"/v1", []map[string]any{{"role": "user", "content": "hi"}}); !strings.HasPrefix(out, "200") {
		t.Fatalf("unprefixed route should stay: %s", out)
	}
}
