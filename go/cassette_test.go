package fakellm_test

import (
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"

	fakellm "github.com/AkaraChen/fakellm/go"
)

func chatText(t *testing.T, s *fakellm.Server, prompt string) string {
	t.Helper()
	res, err := chat(t, s, prompt, false)
	if err != nil {
		t.Fatal(err)
	}
	defer res.Body.Close()
	b, _ := io.ReadAll(res.Body)
	if res.StatusCode != 200 {
		t.Fatalf("status %d: %s", res.StatusCode, b)
	}
	return string(b)
}

func TestRecordAndReplayCassette(t *testing.T) {
	up, _ := start(t, fakellm.Options{Scripts: "- reply: {text: UPSTREAM_SAYS_HI}\n"})
	file := filepath.Join(t.TempDir(), "rec.jsonl")
	proxy, ctx := start(t, fakellm.Options{Upstream: up.URL() + "/v1", Record: file, Watch: fakellm.Ptr(false)})
	if !strings.Contains(chatText(t, proxy, "hello"), "UPSTREAM_SAYS_HI") {
		t.Fatal("proxy did not relay")
	}
	list, err := proxy.Cassettes().List(ctx)
	if err != nil || len(list) == 0 || !list[len(list)-1].Recording || list[len(list)-1].Exchanges != 1 {
		t.Fatalf("list %+v %v", list, err)
	}
	jsonl, err := proxy.Cassettes().JSONL(ctx, "recording")
	if err != nil || strings.Count(strings.TrimSpace(jsonl), "\n") < 1 {
		t.Fatalf("jsonl %q %v", jsonl, err)
	}
	if disk, err := os.ReadFile(file); err != nil || len(disk) == 0 {
		t.Fatalf("record file %q %v", disk, err)
	}

	// The recording owns the name "recording"; give it a real one before registering.
	replay, ctx := start(t, fakellm.Options{Strict: true, Cassettes: strings.Replace(jsonl, `"name":"recording"`, `"name":"rec1"`, 1)})
	if !strings.Contains(chatText(t, replay, "hello"), "UPSTREAM_SAYS_HI") {
		t.Fatal("replay differs")
	}
	infos, err := replay.Cassettes().List(ctx)
	if err != nil || len(infos) != 1 || infos[0].Hits != 1 {
		t.Fatalf("infos %+v %v", infos, err)
	}
	c, err := replay.Cassettes().Get(ctx, infos[0].Name)
	if err != nil || len(c.Exchanges) != 1 {
		t.Fatalf("get %+v %v", c, err)
	}
	if err := replay.Cassettes().Delete(ctx, "nope"); !fakellm.IsNotFound(err) {
		t.Fatalf("delete missing: %v", err)
	}
	// A typed cassette round-trips through Put.
	n, err := replay.Cassettes().Put(ctx, []fakellm.Cassette{c})
	if err != nil || n != 1 {
		t.Fatalf("put %d %v", n, err)
	}
	if err := replay.Cassettes().Clear(ctx); err != nil {
		t.Fatal(err)
	}
	if infos, _ := replay.Cassettes().List(ctx); len(infos) != 0 {
		t.Fatalf("after clear %+v", infos)
	}
}

func TestSnapshots(t *testing.T) {
	s, ctx := start(t, fakellm.Options{Scripts: "- reply: {text: base}\n"})
	info, err := s.Snapshots().Create(ctx, "base")
	if err != nil || info.Name != "base" || info.Scripts != 1 {
		t.Fatalf("create %+v %v", info, err)
	}
	if _, err := s.Snapshots().Create(ctx, "base"); err == nil {
		t.Fatal("duplicate name should fail")
	} else {
		var e *fakellm.Error
		if !errorsAs(err, &e) || e.Status != 409 {
			t.Fatalf("want 409, got %v", err)
		}
	}
	if _, err := s.Scripts().Put(ctx, "- reply: {text: changed}\n- reply: {text: more}\n"); err != nil {
		t.Fatal(err)
	}
	if _, err := s.Snapshots().Restore(ctx, "base"); err != nil {
		t.Fatal(err)
	}
	if rules, _ := s.Scripts().List(ctx); len(rules) != 1 {
		t.Fatalf("rules after restore: %d", len(rules))
	}
	auto, err := s.Snapshots().Create(ctx, "")
	if err != nil || auto.Name == "" {
		t.Fatalf("auto %+v %v", auto, err)
	}
	if list, _ := s.Snapshots().List(ctx); len(list) != 2 {
		t.Fatalf("list %+v", list)
	}
	if err := s.Reset(ctx, fakellm.ResetOptions{}); err != nil {
		t.Fatal(err)
	}
	if list, _ := s.Snapshots().List(ctx); len(list) != 2 {
		t.Fatal("reset must keep snapshots")
	}
	if err := s.Reset(ctx, fakellm.ResetOptions{DropSnapshots: true}); err != nil {
		t.Fatal(err)
	}
	if list, _ := s.Snapshots().List(ctx); len(list) != 0 {
		t.Fatalf("drop: %+v", list)
	}
	if _, err := s.Snapshots().Get(ctx, "base"); !fakellm.IsNotFound(err) {
		t.Fatalf("get missing: %v", err)
	}
}
