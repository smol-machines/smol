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
	"strconv"
	"sync"
	"testing"
	"time"
)

// parityCloud mocks the job, log, export, cache-disk, file and connect routes
// for one machine, mj.
type parityCloud struct {
	mu        sync.Mutex
	jobBody   map[string]any
	infoCalls int
	stuck     bool
	cancelled bool
	file      []byte
	connAuth  string
	connPath  string
}

const stdoutText = "line one\nline two\n"

func (c *parityCloud) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	c.mu.Lock()
	defer c.mu.Unlock()
	reply := func(obj any) {
		w.Header().Set("content-type", "application/json")
		_ = json.NewEncoder(w).Encode(obj)
	}
	job := func(state string) map[string]any {
		j := map[string]any{"id": "job-1", "machineId": "mj", "command": []string{"sh", "-c", "work"}, "state": state, "createdAt": "2026-10-15T00:00:00Z"}
		if state == "exited" {
			j["exitCode"] = 0
		}
		return j
	}
	p := r.URL.Path
	switch {
	case r.Method == "GET" && p == "/v1/machines/mj":
		reply(map[string]any{"id": "mj", "name": "parity", "state": "started", "ready": true, "cpus": 2})
	case r.Method == "POST" && p == "/v1/machines/mj/jobs":
		raw, _ := io.ReadAll(r.Body)
		c.jobBody = map[string]any{}
		_ = json.Unmarshal(raw, &c.jobBody)
		reply(job("running"))
	case r.Method == "GET" && p == "/v1/machines/mj/jobs":
		reply(map[string]any{"jobs": []any{job("running")}})
	case r.Method == "GET" && p == "/v1/machines/mj/jobs/job-1":
		c.infoCalls++
		if c.stuck || c.infoCalls < 3 {
			reply(job("running"))
		} else {
			reply(job("exited"))
		}
	case r.Method == "POST" && p == "/v1/machines/mj/jobs/job-1/cancel":
		c.cancelled = true
		j := job("exited")
		j["exitCode"] = 143
		reply(j)
	case r.Method == "GET" && p == "/v1/machines/mj/jobs/job-1/output":
		stream := r.URL.Query().Get("stream")
		offset, _ := strconv.Atoi(r.URL.Query().Get("offset"))
		if stream == "stderr" {
			// Binary output arrives base64-encoded.
			reply(map[string]any{"stream": "stderr", "offset": 0, "nextOffset": 3, "data": "�", "dataB64": base64.StdEncoding.EncodeToString([]byte{0xff, 0x00, 0x7f}), "eof": true})
			return
		}
		// Serve stdout five bytes at a time to exercise ReadAll's paging.
		end := min(offset+5, len(stdoutText))
		reply(map[string]any{"stream": "stdout", "offset": offset, "nextOffset": end, "data": stdoutText[offset:end], "eof": end == len(stdoutText)})
	case r.Method == "GET" && p == "/v1/machines/mj/logs":
		w.Header().Set("content-type", "text/plain")
		_, _ = fmt.Fprintf(w, "tail=%s\nworkload says hi\n", r.URL.Query().Get("tail"))
	case r.Method == "POST" && p == "/v1/machines/mj/export":
		reply(map[string]any{"id": "art-1", "status": "available"})
	case r.Method == "POST" && p == "/v1/machines/mj/cache-disk/publish":
		reply(map[string]any{
			"cacheDisk": map[string]any{"id": "cd-1", "name": "deps", "sizeGb": 10, "mountPath": "/cache", "latestVersion": 2, "createdAt": "2026-10-01T00:00:00Z",
				"versions": []any{map[string]any{"version": 2, "sizeBytes": 4096, "sha256": "abc", "createdAt": "2026-10-15T00:00:00Z", "sourceMachineId": "mj"}}},
			"version": map[string]any{"version": 2, "sizeBytes": 4096, "sha256": "abc", "createdAt": "2026-10-15T00:00:00Z", "sourceMachineId": "mj"},
		})
	case r.Method == "GET" && p == "/v1/machines/mj/files//data/big.bin":
		w.Header().Set("content-type", "application/octet-stream")
		flusher, _ := w.(http.Flusher)
		for off := 0; off < len(c.file); off += 100_000 {
			_, _ = w.Write(c.file[off:min(off+100_000, len(c.file))])
			if flusher != nil {
				flusher.Flush()
			}
		}
	case r.Method == "GET" && p == "/v1/machines/mj/connect/8080/health":
		c.connAuth = r.Header.Get("authorization")
		c.connPath = p
		_, _ = io.WriteString(w, "ok")
	default:
		w.WriteHeader(http.StatusNotFound)
		reply(map[string]any{"code": "NOT_FOUND", "error": r.Method + " " + p})
	}
}

func TestCloudParity(t *testing.T) {
	isolateCloudEnv(t)
	mock := &parityCloud{file: bytes.Repeat([]byte("0123456789abcdef"), 40_000)} // 640 KB
	srv := httptest.NewServer(mock)
	defer srv.Close()
	ctx := context.Background()
	m, err := Connect(ctx, "mj", &ConnectOptions{Target: TargetCloud, BaseURL: srv.URL, APIKey: "smk_parity"})
	if err != nil {
		t.Fatal(err)
	}

	job, err := m.StartJob(ctx, []string{"sh", "-c", "work"}, JobOptions{Env: map[string]string{"A": "1"}, Workdir: "/app", Timeout: 90 * time.Second, User: "nobody"})
	if err != nil {
		t.Fatal(err)
	}
	b, _ := json.Marshal(mock.jobBody)
	if string(b) != `{"command":["sh","-c","work"],"cwd":"/app","env":{"A":"1"},"timeoutSeconds":90,"user":"nobody"}` {
		t.Fatalf("StartJob body = %s", b)
	}
	if job.ID() != "job-1" {
		t.Fatalf("job id = %q", job.ID())
	}
	jobs, err := m.Jobs(ctx)
	if err != nil || len(jobs) != 1 || jobs[0].State != "running" {
		t.Fatalf("Jobs = %+v, %v", jobs, err)
	}
	out, err := job.ReadAll(ctx, "")
	if err != nil || string(out) != stdoutText {
		t.Fatalf("ReadAll stdout = %q, %v", out, err)
	}
	errOut, err := job.Output(ctx, JobOutputOptions{Stream: "stderr"})
	if err != nil || !bytes.Equal(errOut.Data, []byte{0xff, 0x00, 0x7f}) || !errOut.EOF {
		t.Fatalf("Output stderr = %+v, %v", errOut, err)
	}
	final, err := job.Wait(ctx, JobWaitOptions{Poll: 10 * time.Millisecond, Timeout: 5 * time.Second})
	if err != nil || final.State != "exited" || final.ExitCode == nil || *final.ExitCode != 0 {
		t.Fatalf("Wait = %+v, %v", final, err)
	}
	cancelled, err := m.Job("job-1").Cancel(ctx)
	if err != nil || !mock.cancelled || *cancelled.ExitCode != 143 {
		t.Fatalf("Cancel = %+v, %v", cancelled, err)
	}

	mock.mu.Lock()
	mock.stuck = true
	mock.mu.Unlock()
	if _, err := job.Wait(ctx, JobWaitOptions{Poll: 50 * time.Millisecond, Timeout: 60 * time.Millisecond}); !errors.Is(err, ErrTimeout) {
		t.Fatalf("Wait past its timeout = %v, want ErrTimeout", err)
	}

	logs, err := m.Logs(ctx, 0)
	if err != nil || logs != "tail=100\nworkload says hi\n" {
		t.Fatalf("Logs = %q, %v", logs, err)
	}
	info, err := m.Info(ctx)
	if err != nil || info["cpus"] != float64(2) || info["name"] != "parity" {
		t.Fatalf("Info = %v, %v", info, err)
	}
	art, err := m.ExportArtifact(ctx)
	if err != nil || art["id"] != "art-1" {
		t.Fatalf("ExportArtifact = %v, %v", art, err)
	}
	pub, err := m.PublishCacheDisk(ctx)
	if err != nil || pub.Version.Version != 2 || pub.CacheDisk.Versions[0].SourceMachineID != "mj" || pub.CacheDisk.MountPath != "/cache" {
		t.Fatalf("PublishCacheDisk = %+v, %v", pub, err)
	}

	var got bytes.Buffer
	chunks := 0
	for chunk, err := range m.ReadFileStream(ctx, "/data/big.bin") {
		if err != nil {
			t.Fatal(err)
		}
		chunks++
		got.Write(chunk)
	}
	if !bytes.Equal(got.Bytes(), mock.file) || chunks < 2 {
		t.Fatalf("ReadFileStream: %d bytes in %d chunks, want %d bytes in several", got.Len(), chunks, len(mock.file))
	}
	for _, err := range m.ReadFileStream(ctx, "/missing") {
		if !errors.Is(err, ErrNotFound) {
			t.Fatalf("ReadFileStream of a missing file = %v, want ErrNotFound", err)
		}
	}

	res, err := m.Fetch(ctx, 8080, "/health")
	if err != nil {
		t.Fatal(err)
	}
	body, _ := io.ReadAll(res.Body)
	res.Body.Close()
	if string(body) != "ok" || mock.connAuth != "Bearer smk_parity" || mock.connPath != "/v1/machines/mj/connect/8080/health" {
		t.Fatalf("Fetch body=%q auth=%q path=%q", body, mock.connAuth, mock.connPath)
	}

	if _, err := m.Pack(ctx, "/tmp/x.smolmachine"); !errors.Is(err, ErrNotSupported) {
		t.Fatalf("cloud Pack = %v, want ErrNotSupported", err)
	}
}

func TestLocalCloudOnlyMethodsNotSupported(t *testing.T) {
	ctx := context.Background()
	m := &Machine{t: &localTransport{n: "x"}}
	checks := map[string]error{}
	_, checks["StartJob"] = m.StartJob(ctx, []string{"true"}, JobOptions{})
	_, checks["Jobs"] = m.Jobs(ctx)
	_, checks["Job.Info"] = m.Job("j").Info(ctx)
	_, checks["Info"] = m.Info(ctx)
	_, checks["Logs"] = m.Logs(ctx, 10)
	_, checks["ExportArtifact"] = m.ExportArtifact(ctx)
	_, checks["PublishCacheDisk"] = m.PublishCacheDisk(ctx)
	for name, err := range checks {
		if !errors.Is(err, ErrNotSupported) {
			t.Errorf("local %s = %v, want ErrNotSupported", name, err)
		}
	}
	if _, err := m.Pack(ctx, ""); !errors.Is(err, ErrInvalidConfig) {
		t.Errorf("local Pack with no path = %v, want ErrInvalidConfig", err)
	}
}
