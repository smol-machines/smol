package smol

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
)

// credentialsCloud records the credential calls a cloud machine makes.
type credentialsCloud struct {
	mu       sync.Mutex
	stored   []string // "PUT name body"
	bindings []string
	create   map[string]any
}

func (c *credentialsCloud) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	c.mu.Lock()
	defer c.mu.Unlock()
	reply := func(obj any) {
		w.Header().Set("content-type", "application/json")
		_ = json.NewEncoder(w).Encode(obj)
	}
	raw, _ := io.ReadAll(r.Body)
	switch {
	case r.Method == "GET" && r.URL.Path == "/v1/machines/mc":
		reply(map[string]any{"id": "mc", "name": "creds", "state": "started", "ready": true})
	case r.Method == "POST" && r.URL.Path == "/v1/machines":
		_ = json.Unmarshal(raw, &c.create)
		reply(map[string]any{"id": "mc", "name": "creds", "state": "created"})
	case r.Method == "POST" && r.URL.Path == "/v1/machines/mc/start":
		reply(map[string]any{"id": "mc", "state": "started", "ready": true})
	case r.Method == "PUT" && strings.HasPrefix(r.URL.Path, "/v1/credentials/"):
		c.stored = append(c.stored, "PUT "+strings.TrimPrefix(r.URL.Path, "/v1/credentials/")+" "+strings.TrimSpace(string(raw)))
		reply(map[string]any{"name": "x", "envVar": "X", "hosts": []string{}})
	case r.Method == "GET" && r.URL.Path == "/v1/credentials":
		reply([]map[string]any{{"name": "gh", "envVar": "GH_TOKEN", "hosts": []string{"api.github.com"}, "createdAt": "", "updatedAt": ""}})
	case r.Method == "PUT" && r.URL.Path == "/v1/machines/mc/credentials":
		c.bindings = append(c.bindings, strings.TrimSpace(string(raw)))
		reply(map[string]any{"id": "mc", "state": "started"})
	default:
		http.Error(w, "no route "+r.Method+" "+r.URL.Path, http.StatusNotFound)
	}
}

// A cloud machine's credentials are stored for the account when they carry a
// value, bound by name at create, changed by name later, and their values
// re-stored with the variable and hosts the account already has.
func TestCloudCredentials(t *testing.T) {
	isolateCloudEnv(t)
	mock := &credentialsCloud{}
	srv := httptest.NewServer(mock)
	defer srv.Close()
	ctx := context.Background()
	opts := &ConnectOptions{Target: TargetCloud, BaseURL: srv.URL, APIKey: "smk_test"}

	_, err := Create(ctx, MachineConfig{Image: "alpine", Credentials: []CredentialSpec{
		{Name: "gh", EnvVar: "GH", Hosts: []string{"api.github.com"}, Methods: []string{"GET"}, Value: "v"},
	}}, opts)
	var smolErr *Error
	if !errors.As(err, &smolErr) || smolErr.Code != CodeNotSupported || len(mock.stored) != 0 {
		t.Fatalf("a method restriction must be refused before anything is stored: %v %v", err, mock.stored)
	}

	m, err := Create(ctx, MachineConfig{Image: "alpine", Credentials: []CredentialSpec{
		{Name: "notion", EnvVar: "NOTION_API_KEY", Hosts: []string{"api.notion.com"}, Value: "secret_x"},
		{Name: "github", EnvVar: "GITHUB_TOKEN", Hosts: []string{"api.github.com"}},
	}}, opts)
	if err != nil {
		t.Fatal(err)
	}
	if got := mock.stored; len(got) != 1 || got[0] != `PUT notion {"envVar":"NOTION_API_KEY","hosts":["api.notion.com"],"value":"secret_x"}` {
		t.Fatalf("create stores only the credential with a value: %v", got)
	}
	if got, _ := json.Marshal(mock.create["credentials"]); string(got) != `["notion","github"]` {
		t.Fatalf("create binds every credential by name: %s", got)
	}

	if err := m.SetCredentials(ctx, []CredentialSpec{
		{Name: "gh", EnvVar: "GH_TOKEN", Hosts: []string{"api.github.com"}, Value: "ghp_new"},
		{Name: "notion", EnvVar: "NOTION_API_KEY", Hosts: []string{"api.notion.com"}},
	}); err != nil {
		t.Fatal(err)
	}
	if got := mock.bindings[len(mock.bindings)-1]; got != `{"credentials":["gh","notion"]}` {
		t.Fatalf("SetCredentials binds the names: %s", got)
	}
	if got := mock.stored[len(mock.stored)-1]; !strings.HasPrefix(got, "PUT gh ") {
		t.Fatalf("SetCredentials stores the given value first: %s", got)
	}

	if err := m.UpdateCredentialValues(ctx, map[string]string{"gh": "ghp_rotated"}); err != nil {
		t.Fatal(err)
	}
	if got := mock.stored[len(mock.stored)-1]; got != `PUT gh {"envVar":"GH_TOKEN","hosts":["api.github.com"],"value":"ghp_rotated"}` {
		t.Fatalf("UpdateCredentialValues re-stores the credential: %s", got)
	}
	err = m.UpdateCredentialValues(ctx, map[string]string{"stripe": "sk"})
	if err == nil || !strings.Contains(err.Error(), `no stored credential "stripe"`) {
		t.Fatalf("a credential not stored is refused: %v", err)
	}
}

// A running local machine's bindings and values change without a restart:
// a binding added live is substituted for a new host, a rotated value is used
// on the next request, and a removed binding is not substituted.
// postman-echo checks Basic auth, so its status shows which value reached it.
func TestE2ECredentialsChangeLive(t *testing.T) {
	requireE2E(t)
	ctx := context.Background()
	const good = "cG9zdG1hbjpwYXNzd29yZA==" // base64("postman:password")
	github := CredentialSpec{Name: "gh", EnvVar: "GH_TOKEN", Hosts: []string{"api.github.com"}, Value: "unused"}
	postman := CredentialSpec{Name: "pm", EnvVar: "PM_AUTH", Hosts: []string{"postman-echo.com"}}
	m := newE2EMachine(t, MachineConfig{Image: "alpine", Network: Bool(true), Credentials: []CredentialSpec{github}})
	sh := func(script string) string {
		t.Helper()
		r, err := m.Exec(ctx, []string{"sh", "-c", script}, nil)
		if err != nil {
			t.Fatal(err)
		}
		return strings.TrimSpace(r.Stdout)
	}
	status := func(auth string) string {
		return sh(`curl -s -o /dev/null -m 15 -w '%{http_code}' -H "Authorization: Basic ` + auth + `" https://postman-echo.com/basic-auth`)
	}
	sh("apk add -q curl")

	withValue := postman
	withValue.Value = good
	if err := m.SetCredentials(ctx, []CredentialSpec{github, withValue}); err != nil {
		t.Fatal(err)
	}
	if got := status("$PM_AUTH"); got != "200" {
		t.Fatalf("a binding added while running is substituted: %s", got)
	}
	if err := m.UpdateCredentialValues(ctx, map[string]string{"pm": "d3Jvbmc6d3Jvbmc="}); err != nil {
		t.Fatal(err)
	}
	if got := status("$PM_AUTH"); got != "401" {
		t.Fatalf("a rotated value is used on the next request: %s", got)
	}
	if err := m.UpdateCredentialValues(ctx, map[string]string{"pm": good}); err != nil {
		t.Fatal(err)
	}
	if err := m.SetCredentials(ctx, []CredentialSpec{github, postman}); err != nil {
		t.Fatal(err)
	}
	if got := status("$PM_AUTH"); got != "200" {
		t.Fatalf("a binding that stays keeps its held value: %s", got)
	}
	placeholder := sh("echo $PM_AUTH")
	if err := m.SetCredentials(ctx, []CredentialSpec{github}); err != nil {
		t.Fatal(err)
	}
	if got := sh("echo $PM_AUTH"); got != "" {
		t.Fatalf("a removed binding leaves the guest environment: %q", got)
	}
	// An unsubstituted placeholder is refused (4xx).
	if got := status(placeholder); !strings.HasPrefix(got, "4") {
		t.Fatalf("a removed binding is no longer substituted: %s", got)
	}
	err := m.UpdateCredentialValues(ctx, map[string]string{"pm": good})
	if err == nil || !strings.Contains(err.Error(), "no credential binding 'pm'") {
		t.Fatalf("a value for a removed binding is refused: %v", err)
	}
}
