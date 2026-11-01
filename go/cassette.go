package fakellm

import (
	"context"
	"net/http"
	"net/url"
)

// Cassette is a set of recorded exchanges the server replays by meaning (docs/m4-spec.md). Each
// exchange is {request, outcome, protocol?, meta?, ignore?} in the server's IR; it is kept as
// generic JSON because recordings are normally produced by the server itself.
type Cassette struct {
	Kind              string           `json:"kind,omitempty"`
	Name              string           `json:"name"`
	RecordedAt        string           `json:"recorded_at,omitempty"`
	Upstream          string           `json:"upstream,omitempty"`
	Ignore            []string         `json:"ignore,omitempty"`
	Normalize         []map[string]any `json:"normalize,omitempty"`
	NormalizeDefaults *bool            `json:"normalize_defaults,omitempty"`
	MatchToolSchemas  bool             `json:"match_tool_schemas,omitempty"`
	ModelGlob         string           `json:"model_glob,omitempty"`
	Fuzzy             bool             `json:"fuzzy,omitempty"`
	Once              bool             `json:"once,omitempty"`
	Meta              map[string]any   `json:"meta,omitempty"`
	Exchanges         []map[string]any `json:"exchanges"`
}

// CassetteInfo is a registered cassette with its counters. Recording is true for the in-memory
// recording of the upstream proxy, which is listed last under the name "recording".
type CassetteInfo struct {
	Name      string `json:"name"`
	Exchanges int    `json:"exchanges"`
	Once      bool   `json:"once"`
	Fuzzy     bool   `json:"fuzzy"`
	Hits      uint64 `json:"hits"`
	Consumed  int    `json:"consumed"`
	Recording bool   `json:"recording"`
}

// Cassettes returns the cassette client.
func (s *Server) Cassettes() Cassettes { return Cassettes{s} }

// Cassettes manages the registered cassettes.
type Cassettes struct{ s *Server }

// Put replaces all cassettes. v is a Cassette, a []Cassette, or a JSON/YAML/JSONL document (string
// or []byte). It returns the number of cassettes now registered.
func (c Cassettes) Put(ctx context.Context, v any) (int, error) {
	return c.send(ctx, http.MethodPut, v)
}

// Add registers cassettes; one with the same name is replaced.
func (c Cassettes) Add(ctx context.Context, v any) (int, error) {
	return c.send(ctx, http.MethodPost, v)
}

func (c Cassettes) send(ctx context.Context, method string, v any) (int, error) {
	body, ct, err := encodeTrajectories(v) // same document kinds: object, array, YAML, JSONL
	if err != nil {
		return 0, err
	}
	var out struct {
		Count int `json:"count"`
	}
	err = c.s.do(ctx, method, "/cassettes", ct, body, &out)
	return out.Count, err
}

// List returns the cassettes in registration order, the recording last when there is one.
func (c Cassettes) List(ctx context.Context) ([]CassetteInfo, error) {
	var out struct {
		Cassettes []CassetteInfo `json:"cassettes"`
	}
	err := c.s.doJSON(ctx, http.MethodGet, "/cassettes", nil, &out)
	return out.Cassettes, err
}

// Get returns one cassette; the name "recording" is what the upstream proxy has recorded so far.
func (c Cassettes) Get(ctx context.Context, name string) (Cassette, error) {
	var v Cassette
	err := c.s.doJSON(ctx, http.MethodGet, "/cassettes/"+url.PathEscape(name), nil, &v)
	return v, err
}

// JSONL returns one cassette as JSON lines (a header line, then one exchange per line), the
// format of --record files and of Put.
func (c Cassettes) JSONL(ctx context.Context, name string) (string, error) {
	var out string
	err := c.s.do(ctx, http.MethodGet, "/cassettes/"+url.PathEscape(name)+"?format=jsonl", "", nil, rawString{&out})
	return out, err
}

// Delete removes one cassette; "recording" empties the recording.
func (c Cassettes) Delete(ctx context.Context, name string) error {
	return c.s.doJSON(ctx, http.MethodDelete, "/cassettes/"+url.PathEscape(name), nil, nil)
}

// Clear removes all registered cassettes (not the recording).
func (c Cassettes) Clear(ctx context.Context) error {
	return c.s.doJSON(ctx, http.MethodDelete, "/cassettes", nil, nil)
}

// SnapshotInfo says what a snapshot holds.
type SnapshotInfo struct {
	Name         string `json:"name"`
	Created      string `json:"created"`
	Scripts      int    `json:"scripts"`
	Trajectories int    `json:"trajectories"`
	Policies     int    `json:"policies"`
	Cassettes    int    `json:"cassettes"`
	Responses    int    `json:"responses"`
}

// Snapshots returns the snapshot client.
func (s *Server) Snapshots() Snapshots { return Snapshots{s} }

// Snapshots saves and restores the control-plane state: scripts, trajectories, policies,
// cassettes, stored responses and the clock. Reset keeps snapshots unless DropSnapshots is set,
// so take one as the baseline and Restore it before each test.
type Snapshots struct{ s *Server }

// Create takes a snapshot. name is 1 to 64 characters of A-Za-z0-9._-; empty picks snapshot-N.
// A taken name is a 409 *Error.
func (c Snapshots) Create(ctx context.Context, name string) (SnapshotInfo, error) {
	body := map[string]any{}
	if name != "" {
		body["name"] = name
	}
	var out SnapshotInfo
	err := c.s.doJSON(ctx, http.MethodPost, "/snapshots", body, &out)
	return out, err
}

// List returns the snapshots.
func (c Snapshots) List(ctx context.Context) ([]SnapshotInfo, error) {
	var out struct {
		Snapshots []SnapshotInfo `json:"snapshots"`
	}
	err := c.s.doJSON(ctx, http.MethodGet, "/snapshots", nil, &out)
	return out.Snapshots, err
}

// Get returns one snapshot's summary.
func (c Snapshots) Get(ctx context.Context, name string) (SnapshotInfo, error) {
	var out SnapshotInfo
	err := c.s.doJSON(ctx, http.MethodGet, "/snapshots/"+url.PathEscape(name), nil, &out)
	return out, err
}

// Restore puts a snapshot's state back; the snapshot stays.
func (c Snapshots) Restore(ctx context.Context, name string) (SnapshotInfo, error) {
	var out SnapshotInfo
	err := c.s.doJSON(ctx, http.MethodPost, "/snapshots/"+url.PathEscape(name)+"/restore", nil, &out)
	return out, err
}

// Delete removes one snapshot.
func (c Snapshots) Delete(ctx context.Context, name string) error {
	return c.s.doJSON(ctx, http.MethodDelete, "/snapshots/"+url.PathEscape(name), nil, nil)
}

// Clear removes all snapshots.
func (c Snapshots) Clear(ctx context.Context) error {
	return c.s.doJSON(ctx, http.MethodDelete, "/snapshots", nil, nil)
}
