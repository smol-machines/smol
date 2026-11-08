package smol

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"sort"
	"strconv"
	"strings"
	"sync"
	"testing"
)

// resumableMock is a control plane with resumable execs that cuts off every
// third read without an answer, as a restarting control does, and refuses the
// first start as draining.
type resumableMock struct {
	mu       sync.Mutex
	output   []byte
	requests int
	starts   []map[string]any
	deletes  int
	plain    int
	gets     int
}

func (m *resumableMock) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	m.mu.Lock()
	defer m.mu.Unlock()
	path := r.URL.Path
	switch {
	case r.Method == "GET" && (path == "/v1/machines/m-new" || path == "/v1/machines/m-old"):
		writeJSON(w, 200, map[string]any{"id": strings.TrimPrefix(path, "/v1/machines/"), "state": "started", "ready": true})
	case strings.HasPrefix(path, "/v1/machines/m-old/execs"):
		writeJSON(w, 404, map[string]any{"code": "not_found"})
	case path == "/v1/machines/m-old/exec":
		m.plain++
		writeJSON(w, 200, map[string]any{"exitCode": 0, "stdout": "plain\n", "stderr": ""})
	case strings.HasPrefix(path, "/v1/machines/m-new/files/"):
		m.gets++
		if m.gets == 1 {
			// Half the file, then the connection is cut.
			w.Header().Set("content-length", "1000")
			w.WriteHeader(200)
			_, _ = w.Write(make([]byte, 100))
			if f, ok := w.(http.Flusher); ok {
				f.Flush()
			}
			conn, _, _ := w.(http.Hijacker).Hijack()
			_ = conn.Close()
			return
		}
		_, _ = w.Write(make([]byte, 1000))
	case r.Method == "POST" && path == "/v1/machines/m-new/execs":
		var body map[string]any
		_ = json.NewDecoder(r.Body).Decode(&body)
		m.starts = append(m.starts, body)
		if len(m.starts) == 1 {
			writeJSON(w, 503, map[string]any{"code": "control_draining", "retryable": true})
			return
		}
		m.answer(w, 0, 201)
	case r.Method == "DELETE":
		m.deletes++
		w.WriteHeader(204)
	case r.Method == "GET" && strings.HasPrefix(path, "/v1/machines/m-new/execs/"):
		m.requests++
		if m.requests%3 == 0 {
			conn, _, _ := w.(http.Hijacker).Hijack()
			_ = conn.Close()
			return
		}
		after, _ := strconv.Atoi(r.URL.Query().Get("after"))
		m.answer(w, after, 200)
	default:
		writeJSON(w, 404, map[string]any{"code": "not_found"})
	}
}

func (m *resumableMock) answer(w http.ResponseWriter, after, status int) {
	end := min(after+7, len(m.output))
	done := end == len(m.output)
	stream := "stdout"
	if after%2 == 1 {
		stream = "stderr"
	}
	a := map[string]any{
		"execId": "x", "done": done, "next": end, "start": after, "end": len(m.output),
		"output": []map[string]any{{"stream": stream, "offset": after, "data": base64.StdEncoding.EncodeToString(m.output[after:end])}},
	}
	if done {
		a["exitCode"] = 4
	}
	writeJSON(w, status, a)
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("content-type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func TestResumableExecRidesOutCutOffConnections(t *testing.T) {
	var out strings.Builder
	for i := range 30 {
		fmt.Fprintf(&out, "line %d\n", i)
	}
	mock := &resumableMock{output: []byte(out.String())}
	srv := httptest.NewServer(mock)
	defer srv.Close()
	ctx := context.Background()
	cloud := &ConnectOptions{Target: TargetCloud, BaseURL: srv.URL, APIKey: "smk_test"}

	m, err := Connect(ctx, "m-new", cloud)
	if err != nil {
		t.Fatal(err)
	}
	res, err := m.Exec(ctx, []string{"build"}, nil)
	if err != nil {
		t.Fatal(err)
	}
	got := append(append([]byte{}, res.StdoutBytes...), res.StderrBytes...)
	sort.Slice(got, func(i, j int) bool { return got[i] < got[j] })
	want := []byte(out.String())
	sort.Slice(want, func(i, j int) bool { return want[i] < want[j] })
	if string(got) != string(want) || res.ExitCode != 4 {
		t.Fatalf("every byte once and the exit code: got %d/%d bytes, exit %d", len(got), len(want), res.ExitCode)
	}
	if len(mock.starts) != 2 || mock.starts[0]["execId"] != mock.starts[1]["execId"] {
		t.Fatalf("a start refused while draining is retried under the same id: %v", mock.starts)
	}
	if mock.deletes != 1 {
		t.Fatalf("the collected exec is deleted: %d", mock.deletes)
	}

	var text strings.Builder
	exit := -1
	for ev, err := range m.ExecStream(ctx, []string{"build"}, nil) {
		if err != nil {
			t.Fatal(err)
		}
		switch ev.Kind {
		case EventStdout, EventStderr:
			text.Write(ev.Data)
		case EventExit:
			exit = ev.ExitCode
		}
	}
	if text.String() != out.String() || exit != 4 {
		t.Fatalf("a streamed exec delivers its output in order: %q exit %d", text.String()[:min(40, text.Len())], exit)
	}

	data, err := m.ReadFile(ctx, "/data.bin")
	if err != nil || len(data) != 1000 || mock.gets != 2 {
		t.Fatalf("a download cut off midway is read again whole: len=%d gets=%d err=%v", len(data), mock.gets, err)
	}

	old, err := Connect(ctx, "m-old", cloud)
	if err != nil {
		t.Fatal(err)
	}
	plain, err := old.Exec(ctx, []string{"true"}, nil)
	if err != nil || plain.Stdout != "plain\n" || mock.plain != 1 {
		t.Fatalf("a control plane without resumable execs gets the plain exec: %v %+v", err, plain)
	}
	_ = io.EOF
}

func TestResumableVerdict(t *testing.T) {
	cases := []struct {
		status   int
		code     string
		starting bool
		want     string
	}{
		{503, "control_draining", true, "retry"},
		{503, "unavailable", true, "fail"},
		{503, "unavailable", false, "retry"},
		{503, "machine_interrupted", false, "fail"},
		{502, "", true, "retry"},
		{404, "", true, "unsupported"},
		{404, "exec_not_found", false, "fail"},
	}
	for _, c := range cases {
		if got := resumableVerdict(c.status, c.code, c.starting); got != c.want {
			t.Errorf("%d %q starting=%v: %s, want %s", c.status, c.code, c.starting, got, c.want)
		}
	}
}
