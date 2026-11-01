package fakellm

import (
	"bytes"
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/url"
	"strconv"
	"time"
)

const prefix = "/_fakellm"

// do sends a control-plane request and decodes a JSON answer into out (when not nil).
func (s *Server) do(ctx context.Context, method, path, contentType string, body []byte, out any) error {
	var rd io.Reader
	if body != nil {
		rd = bytes.NewReader(body)
	}
	req, err := http.NewRequestWithContext(ctx, method, s.url+prefix+path, rd)
	if err != nil {
		return err
	}
	if contentType != "" {
		req.Header.Set("content-type", contentType)
	}
	res, err := s.client.Do(req)
	if err != nil {
		return err
	}
	defer res.Body.Close()
	data, err := io.ReadAll(res.Body)
	if err != nil {
		return err
	}
	if res.StatusCode/100 != 2 {
		var e struct {
			Error struct{ Code, Message string } `json:"error"`
		}
		_ = json.Unmarshal(data, &e)
		if e.Error.Message == "" {
			e.Error.Message = string(data)
		}
		return &Error{Status: res.StatusCode, Code: e.Error.Code, Message: e.Error.Message}
	}
	if r, ok := out.(rawString); ok {
		*r.p = string(data)
		return nil
	}
	if out == nil || len(data) == 0 {
		return nil
	}
	return json.Unmarshal(data, out)
}

func (s *Server) doJSON(ctx context.Context, method, path string, in, out any) error {
	var body []byte
	if in != nil {
		b, err := json.Marshal(in)
		if err != nil {
			return err
		}
		body = b
	}
	return s.do(ctx, method, path, "application/json", body, out)
}

// Status returns the server's version, clock, script and request-log state.
func (s *Server) Status(ctx context.Context) (Status, error) {
	var st Status
	err := s.doJSON(ctx, http.MethodGet, "", nil, &st)
	return st, err
}

// ResetOptions selects what Reset keeps. The zero value clears everything.
type ResetOptions struct {
	KeepClock, KeepRequests, KeepScripts, KeepResponses bool
}

// Reset clears scripts, the request log, the clock and stored responses, except what opts keeps.
// Call it before each test. It also releases requests held by hang.
func (s *Server) Reset(ctx context.Context, opts ResetOptions) error {
	q := url.Values{}
	for k, keep := range map[string]bool{
		"clock": opts.KeepClock, "requests": opts.KeepRequests, "scripts": opts.KeepScripts, "responses": opts.KeepResponses,
	} {
		if keep {
			q.Set(k, "false")
		}
	}
	p := "/reset"
	if len(q) > 0 {
		p += "?" + q.Encode()
	}
	return s.doJSON(ctx, http.MethodPost, p, nil, nil)
}

// Scripts returns the script client.
func (s *Server) Scripts() Scripts { return Scripts{s} }

// Scripts manages the rules.
type Scripts struct{ s *Server }

// Put replaces all rules. rules is a []Rule or a JSON/YAML document (string or []byte). The
// hit counters restart; use PutKeepHits to carry them over by position. It returns the number of
// rules now loaded (the HTTP API also reports how many were added; here that is len of your input).
func (c Scripts) Put(ctx context.Context, rules any) (int, error) {
	return c.send(ctx, http.MethodPut, "", rules)
}

// PutKeepHits is Put but keeps the hit counters of rules at the same position.
func (c Scripts) PutKeepHits(ctx context.Context, rules any) (int, error) {
	return c.send(ctx, http.MethodPut, "?keep_hits=true", rules)
}

// Add appends rules after the existing ones and returns the number of rules now loaded.
func (c Scripts) Add(ctx context.Context, rules any) (int, error) {
	return c.send(ctx, http.MethodPost, "", rules)
}

func (c Scripts) send(ctx context.Context, method, query string, rules any) (int, error) {
	body, ext, err := encodeDoc(rules)
	if err != nil {
		return 0, err
	}
	ct := "application/json"
	if ext == ".yaml" {
		ct = "application/yaml"
	}
	var out struct {
		Count int `json:"count"`
	}
	err = c.s.do(ctx, method, "/scripts"+query, ct, body, &out)
	return out.Count, err
}

// List returns the rules in matching order with their counters.
func (c Scripts) List(ctx context.Context) ([]RuleInfo, error) {
	var out struct {
		Rules []RuleInfo `json:"rules"`
	}
	err := c.s.doJSON(ctx, http.MethodGet, "/scripts", nil, &out)
	return out.Rules, err
}

// Clear removes all rules.
func (c Scripts) Clear(ctx context.Context) error {
	return c.s.doJSON(ctx, http.MethodDelete, "/scripts", nil, nil)
}

// Clock returns the clock client.
func (s *Server) Clock() Clock { return Clock{s} }

// Clock controls the server clock that gates streams and delays (docs/control-plane.md#时钟).
type Clock struct{ s *Server }

// Now reads the clock.
func (c Clock) Now(ctx context.Context) (ClockState, error) {
	return c.call(ctx, http.MethodGet, "", nil)
}

// Freeze stops the clock at the first of at, or at the current server time when none is given.
func (c Clock) Freeze(ctx context.Context, at ...time.Time) (ClockState, error) {
	body := map[string]string{}
	if len(at) > 0 {
		body["at"] = at[0].UTC().Format(time.RFC3339Nano)
	}
	return c.call(ctx, http.MethodPost, "/freeze", body)
}

// Unfreeze lets the clock run on from where it stands.
func (c Clock) Unfreeze(ctx context.Context) (ClockState, error) {
	return c.call(ctx, http.MethodPost, "/unfreeze", nil)
}

// Set jumps the clock to at, keeping it frozen or running.
func (c Clock) Set(ctx context.Context, at time.Time) (ClockState, error) {
	return c.call(ctx, http.MethodPost, "/set", map[string]string{"at": at.UTC().Format(time.RFC3339Nano)})
}

// Advance moves the clock by d (negative goes back) and releases everything that became due.
func (c Clock) Advance(ctx context.Context, d time.Duration) (ClockState, error) {
	return c.call(ctx, http.MethodPost, "/advance", map[string]string{"by": strconv.FormatInt(int64(d), 10) + "ns"})
}

// Reset returns to the host clock.
func (c Clock) Reset(ctx context.Context) (ClockState, error) {
	return c.call(ctx, http.MethodPost, "/reset", nil)
}

func (c Clock) call(ctx context.Context, method, path string, in any) (ClockState, error) {
	var st ClockState
	err := c.s.doJSON(ctx, method, "/clock"+path, in, &st)
	return st, err
}

// Responses returns the client for stored Responses API objects.
func (s *Server) Responses() Responses { return Responses{s} }

// Responses inspects the objects POST /v1/responses stores.
type Responses struct{ s *Server }

// List returns the stored responses, oldest first.
func (c Responses) List(ctx context.Context) ([]StoredResponse, error) {
	var out struct {
		Responses []StoredResponse `json:"responses"`
	}
	err := c.s.doJSON(ctx, http.MethodGet, "/responses", nil, &out)
	return out.Responses, err
}

// Clear forgets all stored responses and returns how many there were.
func (c Responses) Clear(ctx context.Context) (int, error) {
	var out struct {
		Deleted int `json:"deleted"`
	}
	err := c.s.doJSON(ctx, http.MethodDelete, "/responses", nil, &out)
	return out.Deleted, err
}

// Requests returns the request-log client.
func (s *Server) Requests() Requests { return Requests{s} }

// Requests reads the log of API requests the server received.
type Requests struct{ s *Server }

// Filter narrows a request-log query; the zero value matches everything.
type Filter struct {
	Model    string // glob
	HasTool  string
	Stream   *bool
	Protocol string // openai.chat, openai.responses, anthropic.messages
	Status   string // "200", "4xx", "error" or "0"
	Source   string // script, fallback, strict, rejected
	Rule     string
	Path     string // prefix
	Since    time.Time
	After    uint64 // only records with seq greater than this
	Limit    int    // 0: no limit
	// Full adds Headers and Raw to each record.
	Full bool

	countOnly bool
}

func (f Filter) query() url.Values {
	q := url.Values{}
	set := func(k, v string) {
		if v != "" {
			q.Set(k, v)
		}
	}
	set("model", f.Model)
	set("has_tool", f.HasTool)
	set("protocol", f.Protocol)
	set("status", f.Status)
	set("source", f.Source)
	set("rule", f.Rule)
	set("path", f.Path)
	if f.Stream != nil {
		q.Set("stream", strconv.FormatBool(*f.Stream))
	}
	if !f.Since.IsZero() {
		q.Set("since", f.Since.UTC().Format(time.RFC3339Nano))
	}
	if f.After > 0 {
		q.Set("after", strconv.FormatUint(f.After, 10))
	}
	if f.countOnly {
		q.Set("limit", "0")
	} else if f.Limit > 0 {
		q.Set("limit", strconv.Itoa(f.Limit))
	}
	if f.Full {
		q.Set("full", "true")
	}
	return q
}

func (c Requests) list(ctx context.Context, f Filter) (RequestList, error) {
	p := "/requests"
	if q := f.query(); len(q) > 0 {
		p += "?" + q.Encode()
	}
	var out RequestList
	err := c.s.doJSON(ctx, http.MethodGet, p, nil, &out)
	return out, err
}

// List returns the matching records, oldest first.
func (c Requests) List(ctx context.Context, f Filter) ([]RequestRecord, error) {
	out, err := c.list(ctx, f)
	return out.Requests, err
}

// Count returns how many records match f (ignoring Limit).
func (c Requests) Count(ctx context.Context, f Filter) (int, error) {
	f.countOnly = true
	out, err := c.list(ctx, f)
	return out.Total, err
}

// Clear empties the log. Sequence numbers keep growing.
func (c Requests) Clear(ctx context.Context) error {
	return c.s.doJSON(ctx, http.MethodDelete, "/requests", nil, nil)
}

// Config reads the log settings and occupancy.
func (c Requests) Config(ctx context.Context) (LogConfig, error) {
	var out LogConfig
	err := c.s.doJSON(ctx, http.MethodGet, "/requests/config", nil, &out)
	return out, err
}

// LogUpdate selects the log settings SetConfig changes; nil fields stay as they are.
type LogUpdate struct {
	Enabled *bool `json:"enabled,omitempty"`
	// Capacity must be at least 1. A smaller capacity drops the oldest records at once.
	Capacity *int `json:"capacity,omitempty"`
}

// SetConfig switches the log on or off and/or resizes it, and returns the settings now in force.
func (c Requests) SetConfig(ctx context.Context, u LogUpdate) (LogConfig, error) {
	var out LogConfig
	err := c.s.doJSON(ctx, http.MethodPut, "/requests/config", u, &out)
	return out, err
}
