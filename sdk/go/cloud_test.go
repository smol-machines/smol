package smol

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"testing"
	"time"
)

// mockCloud is an in-memory mock of the smolfleet /v1 API, ported from the
// Node SDK's test/cloud-mock.ts.
type mockCloud struct {
	mu        sync.Mutex
	auth      string
	echoUser  bool
	files     map[string][]byte
	readiness map[string]int
	bodies    map[string]map[string]any
	execs     []map[string]any
	flags     map[string]bool
	strs      map[string]string
}

func newMockCloud() *mockCloud {
	return &mockCloud{
		echoUser:  true,
		files:     map[string][]byte{},
		readiness: map[string]int{},
		bodies:    map[string]map[string]any{},
		flags:     map[string]bool{},
		strs:      map[string]string{},
	}
}

func (m *mockCloud) body(key string) map[string]any {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.bodies[key]
}

func (m *mockCloud) flag(key string) bool {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.flags[key]
}

func (m *mockCloud) str(key string) string {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.strs[key]
}

func (m *mockCloud) gets(id string) int {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.readiness[id]
}

func (m *mockCloud) readinessResponse(id string, readyAfter int, state string) map[string]any {
	m.readiness[id]++
	ready := readyAfter >= 0 && m.readiness[id] >= readyAfter
	var readyAt any
	if ready {
		readyAt = "2026-07-22T20:01:41.152Z"
	}
	return map[string]any{"id": id, "state": state, "ready": ready, "readyAt": readyAt}
}

var (
	leaseHeartbeat = regexp.MustCompile(`^/v1/leases/[^/]+/heartbeat$`)
	leaseComplete  = regexp.MustCompile(`^/v1/leases/[^/]+/complete$`)
	leaseGet       = regexp.MustCompile(`^/v1/leases/[^/]+$`)
	cloneGet       = regexp.MustCompile(`^/v1/machines/(b\d+|ep\d+)$`)
)

func cloneInfo(id, name string, ports any) map[string]any {
	if ports == nil {
		ports = []any{}
	}
	return map[string]any{
		"id": id, "name": name, "state": "started",
		"source":    map[string]any{"type": "image", "reference": "alpine"},
		"resources": map[string]any{"cpus": 2, "memoryMb": 1024},
		"network":   map[string]any{"mode": "open"}, "env": map[string]any{},
		"ephemeral": false, "ports": ports,
	}
}

func (m *mockCloud) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	m.mu.Lock()
	defer m.mu.Unlock()
	url := r.URL.RequestURI()
	method := r.Method
	if a := r.Header.Get("authorization"); a != "" {
		m.auth = a
	}
	w.Header().Set("x-request-id", "req-test-abc")
	raw, _ := io.ReadAll(r.Body)
	readJSON := func() map[string]any {
		out := map[string]any{}
		_ = json.Unmarshal(raw, &out)
		return out
	}
	reply := func(code int, obj any) {
		w.Header().Set("content-type", "application/json")
		w.WriteHeader(code)
		_ = json.NewEncoder(w).Encode(obj)
	}
	noContent := func() { w.WriteHeader(http.StatusNoContent) }

	switch {
	case method == "POST" && url == "/v1/machines":
		m.bodies["create"] = readJSON()
		reply(200, map[string]any{"id": "m1", "name": "cloud-test", "state": "created"})
	case method == "POST" && strings.HasPrefix(url, "/v1/machines/m1/start"):
		m.strs["startUrl"] = url
		reply(200, map[string]any{"state": "started", "ready": false})
	case method == "POST" && url == "/v1/machines/m1/branches":
		b := readJSON()
		m.bodies["fork"] = b
		if b["name"] == "legacy-branch" {
			m.flags["newBranch404"] = true
			reply(404, map[string]any{"code": "NOT_FOUND", "error": "route not found"})
			return
		}
		reply(201, cloneInfo("m2", b["name"].(string), b["ports"]))
	case method == "POST" && url == "/v1/machines/m1/fork":
		b := readJSON()
		m.bodies["legacyFork"] = b
		reply(201, cloneInfo("m2", b["name"].(string), b["ports"]))
	case method == "POST" && url == "/v1/machines/m1/checkpoints":
		m.flags["checkpointCreate"] = true
		reply(201, map[string]any{"id": "ckpt-1", "machineId": "m1", "status": "available", "sizeBytes": 4096,
			"arch": "amd64", "createdAt": "2026-08-26T00:00:00Z", "downloadUrl": "/v1/checkpoints/ckpt-1/download"})
	case method == "GET" && url == "/v1/machines/m1/checkpoints":
		reply(200, []any{map[string]any{"id": "ckpt-1", "machineId": "m1", "status": "available", "sizeBytes": 4096,
			"arch": "amd64", "createdAt": "2026-08-26T00:00:00Z", "downloadUrl": "/v1/checkpoints/ckpt-1/download"}})
	case method == "POST" && url == "/v1/checkpoints":
		b := readJSON()
		m.bodies["upload"] = b
		base := "http://" + r.Host
		reply(201, map[string]any{
			"checkpoint":    map[string]any{"id": "ckpt-up", "machineId": "", "status": "uploading", "sizeBytes": b["sizeBytes"], "arch": "", "createdAt": "2026-10-04T00:00:00Z"},
			"partSizeBytes": 6,
			"uploadUrls":    []string{base + "/upload/1", base + "/upload/2"},
			"expiresAt":     "2026-10-04T06:00:00Z",
		})
	case method == "PUT" && strings.HasPrefix(url, "/upload/"):
		m.strs["uploadAuth"] = r.Header.Get("authorization")
		m.files[url] = raw
		w.WriteHeader(200)
	case method == "POST" && url == "/v1/checkpoints/ckpt-up/complete":
		up := append(append([]byte{}, m.files["/upload/1"]...), m.files["/upload/2"]...)
		m.strs["uploaded"] = string(up)
		reply(200, map[string]any{"id": "ckpt-up", "machineId": "", "status": "available", "sizeBytes": len(up), "arch": "arm64", "createdAt": "2026-10-04T00:00:00Z"})
	case method == "POST" && url == "/v1/checkpoints/ckpt-up/restore":
		b := readJSON()
		m.bodies["uploadRestore"] = b
		reply(201, map[string]any{"id": "m-restored", "name": b["name"], "state": "stopped"})
	case method == "POST" && url == "/v1/checkpoints/ckpt-1/restore":
		b := readJSON()
		m.bodies["restore"] = b
		reply(201, map[string]any{"id": "m-restored", "name": b["name"], "state": "stopped"})
	case method == "POST" && url == "/v1/machines/m-restored/start":
		reply(200, map[string]any{"id": "m-restored", "state": "started"})
	case method == "POST" && url == "/v1/machines/m1/branches/batch":
		b := readJSON()
		m.bodies["forkBatch"] = b
		if b["namePrefix"] == "legacy" {
			m.flags["newBatch404"] = true
			reply(404, map[string]any{"code": "NOT_FOUND", "error": "route not found"})
			return
		}
		names, _ := b["names"].([]any)
		n := len(names)
		if c, ok := b["count"].(float64); ok {
			n = int(c)
		}
		prefix, _ := b["namePrefix"].(string)
		if prefix == "" {
			prefix = "golden"
		}
		clones := []any{}
		for i := range n {
			name := fmt.Sprintf("%s-%d", prefix, i+1)
			if i < len(names) {
				name = names[i].(string)
			}
			clones = append(clones, cloneInfo(fmt.Sprintf("b%d", i+1), name, b["ports"]))
		}
		reply(201, map[string]any{"clones": clones})
	case method == "POST" && url == "/v1/machines/m1/fork-batch":
		b := readJSON()
		m.bodies["legacyForkBatch"] = b
		n := int(b["count"].(float64))
		prefix, _ := b["namePrefix"].(string)
		if prefix == "" {
			prefix = "fork"
		}
		clones := []any{}
		for i := range n {
			clones = append(clones, cloneInfo(fmt.Sprintf("b%d", i+1), fmt.Sprintf("%s-%d", prefix, i+1), nil))
		}
		reply(201, map[string]any{"clones": clones})
	case method == "POST" && url == "/v1/machines/m1/assign":
		b := readJSON()
		m.bodies["assign"] = b
		reply(201, map[string]any{"leaseId": b["leaseId"], "machineId": "ep1", "ownerToken": "tok_secret", "state": "ready",
			"machine": cloneInfo("ep1", "ep-1", nil)})
	case method == "POST" && leaseHeartbeat.MatchString(url):
		m.bodies["heartbeat"] = readJSON()
		reply(200, map[string]any{"leaseId": "task-99", "machineId": "ep1", "state": "ready"})
	case method == "POST" && leaseComplete.MatchString(url):
		m.bodies["complete"] = readJSON()
		reply(200, map[string]any{"leaseId": "task-99", "machineId": "ep1", "state": "completed"})
	case method == "GET" && leaseGet.MatchString(url):
		reply(200, map[string]any{"leaseId": "task-100", "machineId": "ep1", "state": "completed", "reason": "done",
			"score": 0.9, "result": map[string]any{"passed": 3}})
	case method == "GET" && cloneGet.MatchString(url):
		reply(200, map[string]any{"id": url[strings.LastIndex(url, "/")+1:], "state": "running"})
	case method == "GET" && url == "/v1/machines/m1":
		reply(200, m.readinessResponse("m1", 2, "started"))
	case method == "GET" && url == "/v1/machines/m-restored":
		reply(200, map[string]any{"id": "m-restored", "state": "running", "ready": true})
	case method == "GET" && url == "/v1/machines/m-wait":
		reply(200, m.readinessResponse("m-wait", 5, "started"))
	case method == "GET" && url == "/v1/machines/m-timeout":
		reply(200, m.readinessResponse("m-timeout", -1, "started"))
	case method == "GET" && url == "/v1/machines/m-stopped":
		reply(200, m.readinessResponse("m-stopped", -1, "stopped"))
	case method == "GET" && strings.HasPrefix(url, "/v1/machines/m1/connect/"):
		m.strs["connectUrl"] = url
		reply(200, map[string]any{"ok": true, "path": url})
	case method == "GET" && url == "/v1/machines/m2":
		reply(200, map[string]any{"id": "m2", "state": "running"})
	case method == "POST" && url == "/v1/machines/m2/exec":
		m.bodies["rewardExec"] = readJSON()
		reply(200, map[string]any{"exitCode": 0, "stdout": "reward-clone-exec-ok\n", "stderr": "", "stdoutTruncated": false, "stderrTruncated": false})
	case method == "DELETE" && url == "/v1/machines/m2":
		m.flags["cloneDeleted"] = true
		noContent()
	case method == "POST" && url == "/v1/machines/m1/exec":
		b := readJSON()
		m.bodies["exec"] = b
		m.execs = append(m.execs, b)
		resp := map[string]any{"exitCode": 0, "stdout": "cloud-exec-ok\n", "stderr": "", "stdoutTruncated": true,
			"stderrTruncated": false, "stdoutB64": base64.StdEncoding.EncodeToString([]byte{0x68, 0x69, 0xff})}
		if u, ok := b["user"]; ok && m.echoUser {
			resp["user"] = u
		}
		reply(200, resp)
	case method == "PUT" && strings.HasPrefix(url, "/v1/machines/m1/files/"):
		m.files[url] = raw
		noContent()
	case method == "GET" && strings.HasPrefix(url, "/v1/machines/m1/files/"):
		b, ok := m.files[url]
		if !ok {
			w.WriteHeader(404)
			return
		}
		w.Header().Set("content-type", "application/octet-stream")
		w.WriteHeader(200)
		_, _ = w.Write(b)
	case method == "POST" && url == "/v1/machines/m1/share":
		reply(200, map[string]any{"token": "msh_tok123", "url": "https://app-abc.apps.smolmachines.com?t=msh_tok123"})
	case method == "POST" && url == "/v1/machines/m2/share":
		reply(200, map[string]any{"token": "msh_tok456"})
	case method == "DELETE" && url == "/v1/machines/m1/share":
		m.flags["unshared"] = true
		noContent()
	case method == "POST" && url == "/v1/machines/m1/stop":
		reply(200, map[string]any{"state": "stopped"})
	case method == "POST" && url == "/v1/machines/m1/pause":
		reply(200, map[string]any{"state": "paused"})
	case method == "POST" && url == "/v1/machines/m1/resume":
		reply(200, map[string]any{"state": "started"})
	case method == "DELETE" && url == "/v1/machines/m1":
		noContent()
	default:
		w.WriteHeader(404)
		_, _ = w.Write([]byte("no route"))
	}
}

func jsonEq(a, b any) bool {
	ja, _ := json.Marshal(a)
	jb, _ := json.Marshal(b)
	return bytes.Equal(ja, jb)
}

// isolateCloudEnv keeps the real CLI session and environment out of a test.
func isolateCloudEnv(t *testing.T) {
	t.Helper()
	home := t.TempDir()
	t.Setenv("HOME", home)
	t.Setenv("XDG_CONFIG_HOME", filepath.Join(home, ".config"))
	t.Setenv("SMOL_CLOUD_TOKEN", "")
	t.Setenv("SMOL_CLOUD_URL", "")
}

func TestCloudMock(t *testing.T) {
	isolateCloudEnv(t)
	mock := newMockCloud()
	srv := httptest.NewServer(mock)
	defer srv.Close()
	ctx := context.Background()
	conn := &ConnectOptions{Target: TargetCloud, BaseURL: srv.URL, APIKey: "smk_test123"}
	check := func(label string, ok bool, detail ...any) {
		t.Helper()
		if !ok {
			t.Errorf("%s %v", label, detail)
		}
	}
	must := func(err error) {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
	}

	m, err := Create(ctx, MachineConfig{
		Image: "alpine", Branchable: true, Env: map[string]string{"FOO": "bar"}, Workdir: "/app",
		Resources: ResourceSpec{CPUs: 2, MemoryMB: 1024},
	}, conn)
	must(err)
	check("created via cloud (name from API)", m.Name() == "cloud-test", m.Name())
	check("Create waited past started/ready=false", mock.gets("m1") >= 2, mock.gets("m1"))
	cb := mock.body("create")
	check("create sends env as a plain map + workdir", jsonEq(cb["env"], map[string]string{"FOO": "bar"}) && cb["workdir"] == "/app", cb)
	check("sent Bearer auth", mock.auth == "Bearer smk_test123", mock.auth)
	st, err := m.State(ctx)
	must(err)
	check("State over REST", st == "started", st)
	ready, err := m.Ready(ctx)
	must(err)
	check("Ready reads the readiness flag", ready)
	at, err := m.ReadyAt(ctx)
	must(err)
	check("ReadyAt reads the readiness timestamp", at.Equal(time.Date(2026, 7, 22, 20, 1, 41, 152_000_000, time.UTC)), at)
	must(m.WaitUntilReady(ctx, WaitReadyOptions{Timeout: 2 * time.Second, Interval: 50 * time.Millisecond}))
	check("branchable start passes ?forkable=true", strings.Contains(mock.str("startUrl"), "forkable=true"), mock.str("startUrl"))

	// Connect only attaches; started with ready=false is not usable yet.
	connected, err := Connect(ctx, "m-wait", conn)
	must(err)
	cs, _ := connected.State(ctx)
	cr, _ := connected.Ready(ctx)
	check("connected started machine is not yet usable", cs == "started" && !cr)
	before := mock.gets("m-wait")
	must(connected.WaitUntilReady(ctx, WaitReadyOptions{Timeout: 500 * time.Millisecond, Interval: 10 * time.Millisecond}))
	check("WaitUntilReady polls through started/ready=false", mock.gets("m-wait")-before >= 2, mock.gets("m-wait")-before)

	neverReady, err := Connect(ctx, "m-timeout", conn)
	must(err)
	err = neverReady.WaitUntilReady(ctx, WaitReadyOptions{Timeout: 25 * time.Millisecond, Interval: 5 * time.Millisecond})
	var se *Error
	check("readiness timeout reports machine, state, and duration",
		errors.As(err, &se) && se.Code == CodeTimeout && strings.Contains(se.Message, "m-timeout") &&
			strings.Contains(se.Message, "25ms") && strings.Contains(se.Message, "state=started"), err)

	stopped, err := Connect(ctx, "m-stopped", conn)
	must(err)
	err = stopped.WaitUntilReady(ctx, WaitReadyOptions{Timeout: 100 * time.Millisecond, Interval: 5 * time.Millisecond})
	check("terminal state fails readiness with a useful error",
		errors.As(err, &se) && strings.Contains(se.Message, "m-stopped") && strings.Contains(se.Message, "stopped before becoming ready"), err)

	// Connect bridge.
	ep, err := m.Endpoint(80, "")
	must(err)
	check("Endpoint builds the connect-bridge HTTPURL", ep.HTTPURL == srv.URL+"/v1/machines/m1/connect/80", ep.HTTPURL)
	check("Endpoint derives a ws URL from the base", ep.WSURL == "ws"+strings.TrimPrefix(srv.URL, "http")+"/v1/machines/m1/connect/80", ep.WSURL)
	check("Endpoint carries the Bearer auth header", ep.Headers["authorization"] == "Bearer smk_test123", ep.Headers)
	sub, _ := m.Endpoint(80, "/healthz")
	check("Endpoint(port, path) appends the sub-path", sub.HTTPURL == srv.URL+"/v1/machines/m1/connect/80/healthz", sub.HTTPURL)
	req, _ := http.NewRequest("GET", sub.HTTPURL, nil)
	for k, v := range sub.Headers {
		req.Header.Set(k, v)
	}
	res, err := http.DefaultClient.Do(req)
	must(err)
	var bridged struct {
		OK bool `json:"ok"`
	}
	_ = json.NewDecoder(res.Body).Decode(&bridged)
	res.Body.Close()
	check("a request reaches the guest port through the authed bridge",
		res.StatusCode == 200 && bridged.OK && mock.str("connectUrl") == "/v1/machines/m1/connect/80/healthz", mock.str("connectUrl"))

	r, err := m.Exec(ctx, []string{"echo", "hi"}, &ExecOptions{Env: map[string]string{"A": "b"}, Timeout: 5 * time.Second})
	must(err)
	check("exec stdout mapped", strings.TrimSpace(r.Stdout) == "cloud-exec-ok", r.Stdout)
	check("exec surfaces truncation flags", r.StdoutTruncated && !r.StderrTruncated)
	check("exec exposes byte-exact StdoutBytes from base64", bytes.Equal(r.StdoutBytes, []byte{0x68, 0x69, 0xff}), r.StdoutBytes)
	eb := mock.body("exec")
	check("exec sent command array", jsonEq(eb["command"], []string{"echo", "hi"}), eb)
	env, _ := eb["env"].(map[string]any)
	check("exec sent env + timeoutSeconds", env["A"] == "b" && eb["timeoutSeconds"] == float64(5), eb)

	must(m.WriteFile(ctx, "/tmp/x", []byte("cloud-rt"), 0))
	back, err := m.ReadFile(ctx, "/tmp/x")
	must(err)
	check("file round-trip over REST", string(back) == "cloud-rt", string(back))

	_, err = m.Run(ctx, "alpine", []string{"echo", "x"}, nil)
	check("Run gated as NotSupported on cloud", errors.Is(err, ErrNotSupported), err)

	_, err = Create(ctx, MachineConfig{Image: "alpine", Mounts: []MountSpec{{Source: "/data", Target: "/data"}}}, conn)
	check("cloud create rejects host mounts as NotSupported", errors.Is(err, ErrNotSupported), err)

	check("cloud Sync is gated as NotSupported", errors.Is(m.Sync(ctx), ErrNotSupported))

	_, err = Create(ctx, MachineConfig{Image: "alpine", Ports: []PortSpec{{Host: 8080, Guest: 80}}}, conn)
	must(err)
	cb = mock.body("create")
	check("cloud create publishes ports (guest port only)", jsonEq(cb["ports"], []map[string]int{{"port": 80}}), cb["ports"])
	_, hasEnv := cb["env"]
	_, hasWorkdir := cb["workdir"]
	check("env/workdir omitted from the body when unset", !hasEnv && !hasWorkdir, cb)

	_, err = Create(ctx, MachineConfig{Image: "alpine", Resources: ResourceSpec{Network: Bool(false)}}, conn)
	must(err)
	check("Network false is sent as blocked", jsonEq(mock.body("create")["network"], map[string]string{"mode": "blocked"}), mock.body("create"))
	_, err = Create(ctx, MachineConfig{Image: "alpine", Network: Bool(true)}, conn)
	must(err)
	check("Network true is sent as open", jsonEq(mock.body("create")["network"], map[string]string{"mode": "open"}), mock.body("create"))
	_, err = Create(ctx, MachineConfig{Image: "alpine"}, conn)
	must(err)
	_, hasNetwork := mock.body("create")["network"]
	check("unset network is left to the control plane", !hasNetwork, mock.body("create"))

	// Branch: a live-RAM child over the cloud.
	clone, err := m.Branch(ctx, "rollout-1", BranchOptions{Ports: []PortSpec{{Host: 18080, Guest: 80}}, Branchable: true})
	must(err)

	ck, err := m.Checkpoint(ctx, "", CheckpointOptions{})
	must(err)
	check("checkpoint captures durable live state", mock.flag("checkpointCreate") && ck.ID == "ckpt-1" && ck.SizeBytes == 4096, ck)
	cks, err := m.Checkpoints(ctx)
	must(err)
	check("checkpoints lists captured state", len(cks) == 1 && cks[0].Arch == "amd64", cks)
	restored, err := RestoreCheckpoint(ctx, "ckpt-1", "restored", conn, RestoreCheckpointOptions{})
	must(err)
	rr, _ := restored.Ready(ctx)
	check("RestoreCheckpoint returns a ready machine", mock.body("restore")["name"] == "restored" && restored.ID() == "m-restored" && rr, mock.body("restore"))

	file := filepath.Join(t.TempDir(), "laptop.checkpoint")
	must(os.WriteFile(file, []byte("0123456789"), 0o644))
	var progressMu sync.Mutex
	var last [2]int64
	info, err := UploadCheckpoint(ctx, file, conn, func(sent, total int64) {
		progressMu.Lock()
		last = [2]int64{sent, total}
		progressMu.Unlock()
	})
	must(err)
	check("UploadCheckpoint sends each slice to its URL and completes",
		info.ID == "ckpt-up" && mock.str("uploaded") == "0123456789" && mock.body("upload")["sizeBytes"] == float64(10), mock.str("uploaded"), mock.body("upload"))
	check("...without sending the API key to storage", mock.str("uploadAuth") == "", mock.str("uploadAuth"))
	check("...and reports progress to the end", last == [2]int64{10, 10}, last)
	mock.mu.Lock()
	mock.files = map[string][]byte{}
	mock.mu.Unlock()
	allowAll := AllowAll()
	moved, err := RestoreCheckpoint(ctx, file, "moved", conn, RestoreCheckpointOptions{NetworkPolicy: &allowAll})
	must(err)
	urb := mock.body("uploadRestore")
	network, _ := urb["network"].(map[string]any)
	check("restoring a local file in the cloud uploads it and keeps the network policy",
		moved.ID() == "m-restored" && mock.str("uploaded") == "0123456789" && urb["name"] == "moved" && network["mode"] == "open", urb)

	fb := mock.body("fork")
	check("branch uses POST /branches with the child name", fb["name"] == "rollout-1", fb)
	check("branch ports mapped guest+hostPort", jsonEq(fb["ports"], []map[string]int{{"port": 80, "hostPort": 18080}}), fb["ports"])
	check("branch can promote the child to another branch source", fb["branchable"] == true, fb)
	cst, _ := clone.State(ctx)
	check("branch returns a running child handle", clone.Name() == "rollout-1" && cst == "running", clone.Name())
	legacy, err := m.Branch(ctx, "legacy-branch", BranchOptions{Branchable: true})
	must(err)
	lfb := mock.body("legacyFork")
	check("branch falls back to a legacy /fork control plane",
		mock.flag("newBranch404") && lfb["name"] == "legacy-branch" && lfb["forkable"] == true && legacy.Name() == "legacy-branch", lfb)

	batch, err := m.BranchBatch(ctx, BranchBatchOptions{Count: 3, NamePrefix: "rollout"})
	must(err)
	bb := mock.body("forkBatch")
	check("BranchBatch uses POST /branches/batch with the size spec", bb["count"] == float64(3) && bb["namePrefix"] == "rollout", bb)
	check("BranchBatch returns N child handles in request order", len(batch) == 3 && batch[0].Name() == "rollout-1" && batch[2].Name() == "rollout-3")
	legacyBatch, err := m.BranchBatch(ctx, BranchBatchOptions{Count: 2, NamePrefix: "legacy"})
	must(err)
	check("BranchBatch falls back to a legacy /fork-batch control plane",
		mock.flag("newBatch404") && mock.body("legacyForkBatch")["count"] == float64(2) && len(legacyBatch) == 2, mock.body("legacyForkBatch"))

	ep1, err := m.Assign(ctx, AssignOptions{LeaseID: "task-99", Task: map[string]any{"seed": 7}, Secrets: map[string]string{"KEY": "v"}})
	must(err)
	ab := mock.body("assign")
	check("assign hit POST /assign with leaseId + task + secrets",
		ab["leaseId"] == "task-99" && jsonEq(ab["task"], map[string]any{"seed": 7}) && jsonEq(ab["secrets"], map[string]string{"KEY": "v"}), ab)
	check("episode exposes the provisioned clone", ep1.Machine.Name() == "ep-1", ep1.Machine.Name())
	must(ep1.Heartbeat(ctx))
	check("heartbeat sent the owner token", mock.body("heartbeat")["ownerToken"] == "tok_secret", mock.body("heartbeat"))
	must(ep1.Complete(ctx, "done", nil, nil))
	check("complete sent owner token + reason", mock.body("complete")["ownerToken"] == "tok_secret" && mock.body("complete")["reason"] == "done", mock.body("complete"))

	check("machine exposes its id", m.ID() == "m1", m.ID())
	must(m.Start(ctx, nil))
	must(m.Pause(ctx))
	must(m.Resume(ctx))

	ep2, err := m.Assign(ctx, AssignOptions{LeaseID: "task-100"})
	must(err)
	score := 0.9
	must(ep2.Complete(ctx, "done", &score, map[string]any{"passed": 3}))
	comp := mock.body("complete")
	check("complete sent score + result", comp["score"] == 0.9 && jsonEq(comp["result"], map[string]any{"passed": 3}), comp)
	status, err := ep2.Status(ctx)
	must(err)
	check("Status reads the lease outcome (state + score)", status["state"] == "completed" && status["score"] == 0.9, status)

	mock.mu.Lock()
	mock.flags["cloneDeleted"] = false
	mock.mu.Unlock()
	reward, err := m.RewardBranch(ctx, []string{"python", "grade.py"}, nil, "")
	must(err)
	check("RewardBranch grades in the CLONE (not the parent)", strings.TrimSpace(reward.Stdout) == "reward-clone-exec-ok", reward.Stdout)
	check("RewardBranch sent the grader command to the clone", jsonEq(mock.body("rewardExec")["command"], []string{"python", "grade.py"}), mock.body("rewardExec"))
	check("RewardBranch destroys the throwaway clone", mock.flag("cloneDeleted"))

	_, err = m.ReadFile(ctx, "/does-not-exist")
	check("error message surfaces x-request-id", err != nil && strings.Contains(err.Error(), "[request id: req-test-abc]"), err)
	check("a missing file is ErrNotFound with its status", errors.Is(err, ErrNotFound) && errors.As(err, &se) && se.Status == 404, err)

	link, err := m.Share(ctx)
	must(err)
	check("Share returns the token and the ready-to-use URL",
		link.Token == "msh_tok123" && link.URL == "https://app-abc.apps.smolmachines.com?t=msh_tok123", link)
	m2, err := Connect(ctx, "m2", conn)
	must(err)
	bare, err := m2.Share(ctx)
	must(err)
	check("Share reports an empty URL when the control plane omits it", bare.Token == "msh_tok456" && bare.URL == "", bare)
	must(m.Unshare(ctx))
	check("Unshare issues DELETE on the share route", mock.flag("unshared"))

	// Per-command user: probe the control plane before running as a user.
	resetExecs := func() {
		mock.mu.Lock()
		mock.execs = nil
		mock.mu.Unlock()
	}
	execLog := func() [][2]any {
		mock.mu.Lock()
		defer mock.mu.Unlock()
		out := [][2]any{}
		for _, b := range mock.execs {
			cmd := []string{}
			for _, c := range b["command"].([]any) {
				cmd = append(cmd, c.(string))
			}
			out = append(out, [2]any{strings.Join(cmd, " "), b["user"]})
		}
		return out
	}
	resetExecs()
	asUser, err := m.Exec(ctx, []string{"id", "-u"}, &ExecOptions{User: "nobody"})
	must(err)
	first := execLog()
	check("a user-bearing exec probes with `true` first, then runs as the user",
		jsonEq(first, [][2]any{{"true", "nobody"}, {"id -u", "nobody"}}) && asUser.ExitCode == 0, first)
	resetExecs()
	_, err = m.Exec(ctx, []string{"id", "-u"}, &ExecOptions{User: "nobody"})
	must(err)
	again := execLog()
	check("a proven user is not probed again on the same handle", len(again) == 1 && again[0][0] == "id -u", again)

	mock.mu.Lock()
	mock.echoUser = false
	mock.mu.Unlock()
	old, err := Connect(ctx, "m1", conn)
	must(err)
	resetExecs()
	_, err = old.Exec(ctx, []string{"rm", "-rf", "/data"}, &ExecOptions{User: "app"})
	check("an old control plane is refused with ErrNotSupported", errors.Is(err, ErrNotSupported), err)
	refused := execLog()
	check("...and the real command never reaches it", len(refused) == 1 && refused[0][0] == "true", refused)
	var streamErr error
	for _, err := range old.ExecStream(ctx, []string{"id"}, &ExecOptions{User: "app"}) {
		if err != nil {
			streamErr = err
			break
		}
	}
	check("ExecStream is refused the same way", errors.Is(streamErr, ErrNotSupported), streamErr)
	mock.mu.Lock()
	mock.echoUser = true
	mock.mu.Unlock()

	must(m.Stop(ctx))
	must(m.Delete(ctx))
}

// The SSE parser: events, multi-line data, CRLF line endings and the exit
// event's JSON payload.
func TestCloudExecStreamSSE(t *testing.T) {
	isolateCloudEnv(t)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/machines/m1/exec/stream" || r.Header.Get("accept") != "text/event-stream" {
			w.WriteHeader(404)
			return
		}
		w.Header().Set("content-type", "text/event-stream")
		_, _ = io.WriteString(w, "event: stdout\ndata: line one\ndata: line two\n\n"+
			"event: stderr\r\ndata: oops\r\n\r\n"+
			"event: error\ndata: agent hiccup\n\n"+
			"event: exit\ndata: {\"exitCode\":3}\n")
	}))
	defer srv.Close()
	tr := newCloud(cloudConn{baseURL: srv.URL, apiKey: "k"}, "m1", "m1")
	var got []ExecEvent
	for ev, err := range tr.execStream(context.Background(), []string{"x"}, nil) {
		if err != nil {
			t.Fatal(err)
		}
		got = append(got, ev)
	}
	want := []ExecEvent{
		{Kind: EventStdout, Data: []byte("line one\nline two")},
		{Kind: EventStderr, Data: []byte("oops")},
		{Kind: EventError, Message: "agent hiccup"},
		{Kind: EventExit, ExitCode: 3},
	}
	if !jsonEq(got, want) {
		t.Fatalf("events = %+v, want %+v", got, want)
	}
}

func TestCloudErrorSentence(t *testing.T) {
	cases := map[string]string{
		`{"type":"about:blank","title":"Not Found","detail":"no machine m9"}`: "no machine m9",
		`{"error":"legacy shape"}`: "legacy shape",
		"  plain text  ":           "plain text",
		`["array"]`:                `["array"]`,
	}
	for in, want := range cases {
		if got := errorSentence(in); got != want {
			t.Errorf("errorSentence(%q) = %q, want %q", in, got, want)
		}
	}
	if got := encodePath("/tmp/a b?#%.txt"); got != "/tmp/a%20b%3F%23%25.txt" {
		t.Errorf("encodePath = %q", got)
	}
}

// Only an explicit credential selects the cloud; a stored CLI session supplies
// the key and endpoint once it is selected, unless it has expired.
func TestCloudCLISession(t *testing.T) {
	isolateCloudEnv(t)
	home := os.Getenv("HOME")
	cfg := filepath.Join(home, ".config", "smolvm", "config.toml")
	if err := os.MkdirAll(filepath.Dir(cfg), 0o755); err != nil {
		t.Fatal(err)
	}
	write := func(expires string) {
		t.Helper()
		text := "[other]\napi_key = \"nope\"\n[cloud]\napi_key = \"smk_cli\"\nendpoint = \"https://cli.example/\"\n"
		if expires != "" {
			text += "token_expires_at = \"" + expires + "\"\n"
		}
		if err := os.WriteFile(cfg, []byte(text), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	write("")
	if selectsCloud(ConnectOptions{}) {
		t.Fatal("a CLI session must not select the cloud")
	}
	c, err := cloudConnFor(ConnectOptions{Target: TargetCloud}, "op")
	if err != nil || c.apiKey != "smk_cli" || c.baseURL != "https://cli.example" {
		t.Fatalf("session conn = %+v, %v", c, err)
	}
	write(time.Now().Add(-time.Hour).UTC().Format(time.RFC3339))
	c, err = cloudConnFor(ConnectOptions{Target: TargetCloud}, "op")
	// An expired session yields no endpoint; the plain api_key fallback still applies.
	if err != nil || c.apiKey != "smk_cli" || c.baseURL != defaultCloudURL {
		t.Fatalf("expired session conn = %+v, %v", c, err)
	}
	if err := os.Remove(cfg); err != nil {
		t.Fatal(err)
	}
	if _, err := cloudConnFor(ConnectOptions{Target: TargetCloud}, "connect"); !errors.Is(err, ErrInvalidConfig) {
		t.Fatalf("no key: err = %v", err)
	}
	t.Setenv("SMOL_CLOUD_TOKEN", "smk_env")
	t.Setenv("SMOL_CLOUD_URL", "https://env.example//")
	c, err = cloudConnFor(ConnectOptions{}, "op")
	if err != nil || c.apiKey != "smk_env" || c.baseURL != "https://env.example" || !selectsCloud(ConnectOptions{}) {
		t.Fatalf("env conn = %+v, %v", c, err)
	}
}
