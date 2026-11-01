package fakellm

import (
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"net/url"
)

// Trajectory is a recorded agent run the server replays one step per request (docs/agent.md).
// Name is unique; the zero values of the other fields are the server defaults (on_divergence
// strict, cursor by_turn, unknown_tool error).
type Trajectory struct {
	Kind         string           `json:"kind,omitempty"`
	Name         string           `json:"name"`
	Steps        []TrajectoryStep `json:"steps"`
	OnDivergence string           `json:"on_divergence,omitempty"` // strict | lenient | stop
	StopText     *string          `json:"stop_text,omitempty"`
	Cursor       string           `json:"cursor,omitempty"` // by_turn | by_hits
	// Dedupe (by_hits only): nil or true replays the step for a request identical to the previous one; false counts every call.
	Dedupe       *bool                  `json:"dedupe,omitempty"`
	ToolMap      map[string]ToolMapping `json:"tool_map,omitempty"`
	UnknownTool  string                 `json:"unknown_tool,omitempty"` // stop | error | pass
	CheckPairing bool                   `json:"check_pairing,omitempty"`
	Meta         map[string]any         `json:"meta,omitempty"`
}

// TrajectoryStep is one model turn: the reply and what the request asking for it must contain.
type TrajectoryStep struct {
	Reply  ReplySpec `json:"reply"`
	Expect *Expect   `json:"expect,omitempty"`
}

// Expect lists what a step's request must look like; unset fields are not checked.
type Expect struct {
	ToolResults []ToolResultExpect `json:"tool_results,omitempty"`
	AnyUser     string             `json:"any_user,omitempty"`
}

// ToolResultExpect is a tool result the client must have sent back. Contains is a regex.
type ToolResultExpect struct {
	Name     string  `json:"name,omitempty"`
	ID       string  `json:"id,omitempty"`
	Contains string  `json:"contains,omitempty"`
	Equals   *string `json:"equals,omitempty"`
	IsError  *bool   `json:"is_error,omitempty"`
}

// ToolMapping renames a trajectory tool to the client's tool and rebuilds its arguments from
// templates such as "$cmd" or "run ${cmd}".
type ToolMapping struct {
	Name     string         `json:"name"`
	Args     map[string]any `json:"args,omitempty"`
	KeepRest bool           `json:"keep_rest,omitempty"`
}

// TrajectoryInfo is a registered trajectory with its counters.
type TrajectoryInfo struct {
	Name         string `json:"name"`
	Steps        int    `json:"steps"`
	OnDivergence string `json:"on_divergence"`
	Cursor       string `json:"cursor"`
	UnknownTool  string `json:"unknown_tool"`
	Hits         uint64 `json:"hits"`
	LastStep     *int   `json:"last_step"`
}

// Trajectories returns the trajectory client.
func (s *Server) Trajectories() Trajectories { return Trajectories{s} }

// Trajectories manages the registered trajectories.
type Trajectories struct{ s *Server }

// Put replaces all trajectories. v is a Trajectory, a []Trajectory, or a JSON/YAML/JSONL document
// (string or []byte). It returns the number of trajectories now registered.
func (c Trajectories) Put(ctx context.Context, v any) (int, error) {
	return c.send(ctx, http.MethodPut, v)
}

// Add registers trajectories; one with the same name is replaced in place.
func (c Trajectories) Add(ctx context.Context, v any) (int, error) {
	return c.send(ctx, http.MethodPost, v)
}

func (c Trajectories) send(ctx context.Context, method string, v any) (int, error) {
	body, ct, err := encodeTrajectories(v)
	if err != nil {
		return 0, err
	}
	var out struct {
		Count int `json:"count"`
	}
	err = c.s.do(ctx, method, "/trajectories", ct, body, &out)
	return out.Count, err
}

func encodeTrajectories(v any) ([]byte, string, error) {
	var text string
	switch t := v.(type) {
	case string:
		text = t
	case []byte:
		text = string(t)
	default:
		b, err := json.Marshal(v)
		return b, "application/json", err
	}
	data, ext, err := encodeDoc(text)
	if err != nil {
		return nil, "", err
	}
	switch {
	case ext == ".yaml":
		return data, "application/yaml", nil
	case isJSONL(data):
		return data, "application/x-ndjson", nil
	}
	return data, "application/json", nil
}

// isJSONL reports a document of several JSON objects, one per line.
func isJSONL(b []byte) bool {
	var first json.RawMessage
	dec := json.NewDecoder(bytes.NewReader(b))
	if dec.Decode(&first) != nil {
		return false
	}
	return dec.More()
}

// List returns the trajectories in registration order.
func (c Trajectories) List(ctx context.Context) ([]TrajectoryInfo, error) {
	var out struct {
		Trajectories []TrajectoryInfo `json:"trajectories"`
	}
	err := c.s.doJSON(ctx, http.MethodGet, "/trajectories", nil, &out)
	return out.Trajectories, err
}

// Get returns one trajectory as the server stores it.
func (c Trajectories) Get(ctx context.Context, name string) (Trajectory, error) {
	var t Trajectory
	err := c.s.doJSON(ctx, http.MethodGet, "/trajectories/"+url.PathEscape(name), nil, &t)
	return t, err
}

// JSONL returns one trajectory as JSON lines.
func (c Trajectories) JSONL(ctx context.Context, name string) (string, error) {
	var out string
	err := c.s.do(ctx, http.MethodGet, "/trajectories/"+url.PathEscape(name)+"?format=jsonl", "", nil, rawString{&out})
	return out, err
}

// Delete removes one trajectory.
func (c Trajectories) Delete(ctx context.Context, name string) error {
	return c.s.doJSON(ctx, http.MethodDelete, "/trajectories/"+url.PathEscape(name), nil, nil)
}

// Clear removes all trajectories.
func (c Trajectories) Clear(ctx context.Context) error {
	return c.s.doJSON(ctx, http.MethodDelete, "/trajectories", nil, nil)
}

// Policy is a script the server asks for each reply: an ES module whose default export is
// decide(request, context). Runtime overrides Options.PolicyRuntime for this policy.
type Policy struct {
	Name    string `json:"name"`
	Source  string `json:"source"`
	Runtime string `json:"runtime,omitempty"`
	// Files are extra modules written next to the entry module (relative paths), importable from Source.
	Files map[string]string `json:"files,omitempty"`
}

// PolicyInfo is a registered policy with its process state.
type PolicyInfo struct {
	Name     string          `json:"name"`
	Runtime  string          `json:"runtime"`
	Running  bool            `json:"running"`
	Hits     uint64          `json:"hits"`
	Restarts uint64          `json:"restarts"`
	State    json.RawMessage `json:"state"`
	Log      []string        `json:"log"`
}

// Policies returns the policy client.
func (s *Server) Policies() Policies { return Policies{s} }

// Policies manages the registered policies.
type Policies struct{ s *Server }

// Put replaces all policies. Every module is loaded once first; a module that fails to load is an
// *Error with status 400 and nothing changes.
func (c Policies) Put(ctx context.Context, p ...Policy) (int, error) {
	return c.send(ctx, http.MethodPut, p)
}

// Add registers policies; one with the same name is replaced.
func (c Policies) Add(ctx context.Context, p ...Policy) (int, error) {
	return c.send(ctx, http.MethodPost, p)
}

func (c Policies) send(ctx context.Context, method string, p []Policy) (int, error) {
	var out struct {
		Count int `json:"count"`
	}
	err := c.s.doJSON(ctx, method, "/policies", struct {
		Policies []Policy `json:"policies"`
	}{p}, &out)
	return out.Count, err
}

// List returns the policies in registration order.
func (c Policies) List(ctx context.Context) ([]PolicyInfo, error) {
	var out struct {
		Policies []PolicyInfo `json:"policies"`
	}
	err := c.s.doJSON(ctx, http.MethodGet, "/policies", nil, &out)
	return out.Policies, err
}

// Get returns one policy's state.
func (c Policies) Get(ctx context.Context, name string) (PolicyInfo, error) {
	var p PolicyInfo
	err := c.s.doJSON(ctx, http.MethodGet, "/policies/"+url.PathEscape(name), nil, &p)
	return p, err
}

// Delete stops and removes one policy.
func (c Policies) Delete(ctx context.Context, name string) error {
	return c.s.doJSON(ctx, http.MethodDelete, "/policies/"+url.PathEscape(name), nil, nil)
}

// Clear stops and removes all policies.
func (c Policies) Clear(ctx context.Context) error {
	return c.s.doJSON(ctx, http.MethodDelete, "/policies", nil, nil)
}

// register loads Options.Trajectories and Options.Policies through the control plane.
func (s *Server) register(ctx context.Context, opts Options) error {
	if opts.Trajectories != nil {
		if _, err := s.Trajectories().Put(ctx, opts.Trajectories); err != nil {
			return err
		}
	}
	if opts.Cassettes != nil {
		if _, err := s.Cassettes().Put(ctx, opts.Cassettes); err != nil {
			return err
		}
	}
	if len(opts.Policies) > 0 {
		if _, err := s.Policies().Put(ctx, opts.Policies...); err != nil {
			return err
		}
	}
	return nil
}

// rawString makes do store the response body as is instead of decoding JSON.
type rawString struct{ p *string }
