// Package fakellm starts the fakellm binary as a child process and wraps its control plane
// (/_fakellm), the way testcontainers wraps a container but without Docker.
//
// The server is the Rust binary, not a reimplementation: Start finds it (Options.Bin, $FAKELLM_BIN,
// $PATH, or Download), runs it with --port 0 and reads the bound endpoint from the first line of
// its stdout. Stop kills the process; nothing is left behind.
package fakellm

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"sync"
	"time"
)

// Options configures Start. The zero value runs `fakellm --port 0` on 127.0.0.1.
type Options struct {
	// Bin is the binary to run. Empty: $FAKELLM_BIN, then "fakellm" on $PATH.
	Bin string
	// Host to listen on; empty means the binary's default, 127.0.0.1.
	Host string
	// Port to listen on; 0 picks a free port.
	Port int
	// APIKey, when set, is required on API paths (the control plane stays open).
	APIKey string
	// Models listed by GET /v1/models.
	Models []string
	// Strict makes unmatched requests fail instead of getting the default reply.
	Strict bool
	// TTFT is the default time to first token, e.g. "300ms".
	TTFT string
	// TokensPerSec is the default streaming speed; 0 means no pacing.
	TokensPerSec float64
	// FallbackTokens is the length of the generated fallback text in tokens; 0 means the server default (32).
	FallbackTokens int
	// ChunkTokens is tokens per streamed content chunk; 0 means the server default (1).
	ChunkTokens int
	// ArgsChunkBytes is bytes of tool-call arguments per streamed delta; 0 means the server default (16).
	ArgsChunkBytes int
	// NoRequestLog turns request recording off.
	NoRequestLog bool
	// RequestLogCapacity is how many requests the log keeps; 0 means the server default.
	RequestLogCapacity int
	// RedactAuth stores "<redacted>" instead of credential header values in the request log.
	RedactAuth bool
	// Scripts loaded at start: a JSON or YAML document (string, []byte), or a []Rule / Rule.
	Scripts any
	// Trajectories registered right after start: a JSON/YAML/JSONL document (string, []byte), a Trajectory or a []Trajectory.
	Trajectories any
	// Cassettes registered right after start: a JSON/YAML/JSONL document (string, []byte), a Cassette or a []Cassette.
	Cassettes any
	// Upstream is --upstream: an OpenAI-compatible or Anthropic server that gets the requests no rule or cassette answers.
	Upstream string
	// UpstreamAPIKey is --upstream-api-key, sent to the upstream instead of the client's credentials.
	UpstreamAPIKey string
	// Record is --record: a file that every exchange recorded from the upstream is appended to. Needs Upstream.
	Record string
	// Watch reloads script, trajectory, policy and cassette files given in Args when they change.
	// nil keeps the server default (on); false passes --no-watch.
	Watch *bool
	// Policies registered right after start. Needs a policy runtime (tjs, node or bun) on the machine.
	Policies []Policy
	// PolicyRuntime is --policy-runtime: tjs, node, bun or a path; empty means the server default.
	PolicyRuntime string
	// PathPrefix also serves the API under this prefix, e.g. "/anthropic"; BaseURL and AnthropicBaseURL include it.
	PathPrefix string
	// Args are extra command-line arguments, appended last.
	Args []string
	// Env is extra environment, "KEY=value", added to the current process environment.
	Env []string
	// StartTimeout bounds the wait for the endpoint line; default 10s.
	StartTimeout time.Duration
}

// Server is a running fakellm process.
type Server struct {
	url string
	// prefix is Options.PathPrefix, normalised to a leading slash and none trailing.
	prefix string
	bin    string
	cmd    *exec.Cmd
	// scriptFile is the temp file holding Options.Scripts, removed once the binary has read it.
	scriptFile string
	rmOnce     sync.Once
	client     *http.Client

	done chan struct{} // closed when the process has been reaped
	once sync.Once
	werr error
	logs *tailBuffer
}

// Start runs the binary and returns once it is listening. The process is killed when ctx is
// cancelled, so pass a context that lives as long as the test.
func Start(ctx context.Context, opts Options) (*Server, error) {
	bin, err := resolveBin(opts.Bin)
	if err != nil {
		return nil, err
	}
	args := []string{"--port", strconv.Itoa(opts.Port)}
	if opts.Host != "" {
		args = append(args, "--host", opts.Host)
	}
	if opts.APIKey != "" {
		args = append(args, "--api-key", opts.APIKey)
	}
	if len(opts.Models) > 0 {
		args = append(args, "--models", strings.Join(opts.Models, ","))
	}
	if opts.Strict {
		args = append(args, "--strict")
	}
	if opts.TTFT != "" {
		args = append(args, "--ttft", opts.TTFT)
	}
	if opts.TokensPerSec > 0 {
		args = append(args, "--tokens-per-sec", strconv.FormatFloat(opts.TokensPerSec, 'f', -1, 64))
	}
	for _, f := range []struct {
		flag string
		n    int
	}{{"--fallback-tokens", opts.FallbackTokens}, {"--chunk-tokens", opts.ChunkTokens}, {"--args-chunk-bytes", opts.ArgsChunkBytes}} {
		if f.n > 0 {
			args = append(args, f.flag, strconv.Itoa(f.n))
		}
	}
	if opts.NoRequestLog {
		args = append(args, "--no-request-log")
	}
	if opts.RequestLogCapacity > 0 {
		args = append(args, "--request-log-capacity", strconv.Itoa(opts.RequestLogCapacity))
	}
	if opts.RedactAuth {
		args = append(args, "--redact-auth")
	}
	if opts.PolicyRuntime != "" {
		args = append(args, "--policy-runtime", opts.PolicyRuntime)
	}
	for _, f := range []struct{ flag, v string }{{"--upstream", opts.Upstream}, {"--upstream-api-key", opts.UpstreamAPIKey}, {"--record", opts.Record}} {
		if f.v != "" {
			args = append(args, f.flag, f.v)
		}
	}
	if opts.Watch != nil && !*opts.Watch {
		args = append(args, "--no-watch")
	}
	prefix := ""
	if opts.PathPrefix != "" {
		prefix = "/" + strings.Trim(opts.PathPrefix, "/")
		args = append(args, "--path-prefix", prefix)
	}
	s := &Server{bin: bin, prefix: prefix, client: &http.Client{Timeout: 30 * time.Second}, done: make(chan struct{}), logs: &tailBuffer{max: 8 << 10}}
	if opts.Scripts != nil {
		file, err := writeScripts(opts.Scripts)
		if err != nil {
			return nil, err
		}
		s.scriptFile = file
		args = append(args, "--scripts", file)
	}
	args = append(args, opts.Args...)

	cmd := exec.CommandContext(ctx, bin, args...)
	cmd.Env = append(os.Environ(), opts.Env...)
	cmd.Stderr = s.logs
	cmd.WaitDelay = time.Second
	// A writer rather than StdoutPipe: Wait then finishes the copy itself, so it cannot race the reader.
	pr, pw := io.Pipe()
	cmd.Stdout = pw
	if err := cmd.Start(); err != nil {
		s.cleanup()
		_ = pw.Close()
		_ = pr.Close()
		return nil, fmt.Errorf("fakellm: start %s: %w", bin, err)
	}
	s.cmd = cmd

	lines := make(chan string, 1)
	go func() {
		// Only the first line matters, but the pipe is drained so the child never blocks on stdout.
		line, _ := bufio.NewReader(pr).ReadString('\n')
		if line != "" {
			lines <- strings.TrimSpace(line)
		}
		close(lines)
		_, _ = io.Copy(io.Discard, pr)
	}()
	go func() {
		s.werr = cmd.Wait()
		_ = pw.Close()
		s.cleanup()
		close(s.done)
	}()

	timeout := opts.StartTimeout
	if timeout == 0 {
		timeout = 10 * time.Second
	}
	timer := time.NewTimer(timeout)
	defer timer.Stop()
	select {
	case line, ok := <-lines:
		if !ok {
			_ = s.Stop()
			return nil, fmt.Errorf("fakellm: exited during start (%v): %s", s.werr, s.logs.String())
		}
		if !strings.HasPrefix(line, "http://") {
			_ = s.Stop()
			return nil, fmt.Errorf("fakellm: no endpoint on stdout (got %q): %s", line, s.logs.String())
		}
		s.url = reachable(line)
		s.cleanup() // the binary read --scripts before it printed the endpoint
		if err := s.register(ctx, opts); err != nil {
			_ = s.Stop()
			return nil, err
		}
	case <-s.done:
		return nil, fmt.Errorf("fakellm: exited during start (%v): %s", s.werr, s.logs.String())
	case <-timer.C:
		_ = s.Stop()
		return nil, fmt.Errorf("fakellm: no endpoint after %s: %s", timeout, s.logs.String())
	case <-ctx.Done():
		_ = s.Stop()
		return nil, ctx.Err()
	}
	return s, nil
}

// URL is the server origin, e.g. http://127.0.0.1:40123. Anthropic clients want this as base URL.
func (s *Server) URL() string { return s.url }

// AnthropicBaseURL is the base URL for the Anthropic SDK, which appends /v1/messages itself: URL plus the path prefix.
func (s *Server) AnthropicBaseURL() string { return s.url + s.prefix }

// BaseURL is URL plus the path prefix plus /v1, the base URL for OpenAI clients.
func (s *Server) BaseURL() string { return s.url + s.prefix + "/v1" }

// Logs returns the tail of the process's stderr, for failure messages.
func (s *Server) Logs() string { return s.logs.String() }

// Stop kills the process, waits for it and removes temporary files. It is safe to call twice.
func (s *Server) Stop() error {
	s.once.Do(func() {
		if s.cmd != nil && s.cmd.Process != nil {
			_ = s.cmd.Process.Kill()
			<-s.done
		}
		s.cleanup()
	})
	return nil
}

func (s *Server) cleanup() {
	s.rmOnce.Do(func() {
		if s.scriptFile != "" {
			_ = os.Remove(s.scriptFile)
		}
	})
}

// reachable turns the unspecified bind address in an endpoint into one a client can connect to.
func reachable(endpoint string) string {
	u, err := url.Parse(endpoint)
	if err != nil {
		return endpoint
	}
	switch u.Hostname() {
	case "0.0.0.0":
		u.Host = net.JoinHostPort("127.0.0.1", u.Port())
	case "::":
		u.Host = net.JoinHostPort("::1", u.Port())
	}
	return u.String()
}

// Wire returns the client configuration for a tool (claude, codex, opencode, pi, openai-sdk, ...) in
// the given format (env, json, toml or shell; empty picks the tool's natural one), by running
// `fakellm wire`.
func (s *Server) Wire(ctx context.Context, target, format string) (string, error) {
	args := []string{"wire", target, "--endpoint", s.url}
	if format != "" {
		args = append(args, "--format", format)
	}
	out, err := exec.CommandContext(ctx, s.bin, args...).Output()
	if err != nil {
		var ee *exec.ExitError
		if errors.As(err, &ee) {
			return "", fmt.Errorf("fakellm wire %s: %s", target, strings.TrimSpace(string(ee.Stderr)))
		}
		return "", err
	}
	return string(out), nil
}

// WireOutput is what `fakellm wire --format json` prints: environment variables, files to write
// (file name to content; this replaces the CLI's --file) and a ready-to-paste snippet.
type WireOutput struct {
	Env     map[string]string `json:"env"`
	Files   map[string]string `json:"files"`
	Snippet string            `json:"snippet"`
}

// WireJSON is Wire with the parsed json rendering.
func (s *Server) WireJSON(ctx context.Context, target string) (*WireOutput, error) {
	out, err := s.Wire(ctx, target, "json")
	if err != nil {
		return nil, err
	}
	var w WireOutput
	if err := json.Unmarshal([]byte(out), &w); err != nil {
		return nil, fmt.Errorf("fakellm wire %s: %w", target, err)
	}
	return &w, nil
}

func writeScripts(v any) (string, error) {
	data, ext, err := encodeDoc(v)
	if err != nil {
		return "", fmt.Errorf("fakellm: encode scripts: %w", err)
	}
	f, err := os.CreateTemp("", "fakellm-scripts-*"+ext)
	if err != nil {
		return "", err
	}
	defer f.Close()
	if _, err := f.Write(data); err != nil {
		_ = os.Remove(f.Name())
		return "", err
	}
	return f.Name(), nil
}

// encodeDoc turns a scripts value into a document: strings and bytes are taken as JSON or YAML
// text, anything else is marshalled to JSON. ext is the file extension that tells the server which.
func encodeDoc(v any) (data []byte, ext string, err error) {
	switch t := v.(type) {
	case string:
		data = []byte(t)
	case []byte:
		data = t
	default:
		data, err = json.Marshal(v)
		return data, ".json", err
	}
	if startsJSON(data) {
		return data, ".json", nil
	}
	return data, ".yaml", nil
}

func startsJSON(b []byte) bool {
	t := strings.TrimSpace(string(b))
	return strings.HasPrefix(t, "[") || strings.HasPrefix(t, "{")
}

// Error is a non-2xx answer from the control plane.
type Error struct {
	Status  int
	Code    string
	Message string
}

func (e *Error) Error() string { return fmt.Sprintf("fakellm: %d %s: %s", e.Status, e.Code, e.Message) }

// IsNotFound reports whether err is a 404 from the control plane.
func IsNotFound(err error) bool {
	var e *Error
	return errors.As(err, &e) && e.Status == http.StatusNotFound
}

// tailBuffer keeps the last max bytes written to it.
type tailBuffer struct {
	mu  sync.Mutex
	buf []byte
	max int
}

func (t *tailBuffer) Write(p []byte) (int, error) {
	t.mu.Lock()
	defer t.mu.Unlock()
	t.buf = append(t.buf, p...)
	if len(t.buf) > t.max {
		t.buf = t.buf[len(t.buf)-t.max:]
	}
	return len(p), nil
}

func (t *tailBuffer) String() string {
	t.mu.Lock()
	defer t.mu.Unlock()
	return strings.TrimSpace(string(t.buf))
}
