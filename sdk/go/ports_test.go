package smol

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"sync"
	"testing"
)

// portsCloud mocks the control plane's published-port and port-link routes
// for one machine, mp. The "node" gives each guest port host port 40000+port,
// the way a real node picks the host side itself.
type portsCloud struct {
	mu        sync.Mutex
	ports     []int
	puts      []map[string]any
	linkCalls int
	revoked   bool
}

func (c *portsCloud) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	c.mu.Lock()
	defer c.mu.Unlock()
	reply := func(obj any) {
		w.Header().Set("content-type", "application/json")
		_ = json.NewEncoder(w).Encode(obj)
	}
	switch {
	case r.Method == "GET" && r.URL.Path == "/v1/machines/mp":
		ports := []map[string]any{}
		for _, p := range c.ports {
			ports = append(ports, map[string]any{"port": p, "hostPort": 40000 + p})
		}
		// A port the node has not placed yet has no host port.
		ports = append(ports, map[string]any{"port": 9999, "hostPort": nil})
		reply(map[string]any{"id": "mp", "name": "ports-test", "state": "started", "ready": true, "ports": ports})
	case r.Method == "PUT" && r.URL.Path == "/v1/machines/mp/ports":
		raw, _ := io.ReadAll(r.Body)
		body := map[string]any{}
		_ = json.Unmarshal(raw, &body)
		c.puts = append(c.puts, body)
		c.ports = nil
		for _, p := range body["ports"].([]any) {
			port := int(p.(map[string]any)["port"].(float64))
			if port != 9999 {
				c.ports = append(c.ports, port)
			}
		}
		reply(map[string]any{"id": "mp", "state": "started"})
	case r.Method == "POST" && r.URL.Path == "/v1/machines/mp/port-links":
		c.linkCalls++
		reply(map[string]any{"urls": map[string]string{"8080": "https://8080-tok.example/", "3000": "https://3000-tok.example/"}})
	case r.Method == "DELETE" && r.URL.Path == "/v1/machines/mp/port-links":
		c.revoked = true
		w.WriteHeader(http.StatusNoContent)
	default:
		w.WriteHeader(http.StatusNotFound)
		reply(map[string]any{"code": "NOT_FOUND", "error": r.Method + " " + r.URL.Path})
	}
}

func TestCloudPorts(t *testing.T) {
	isolateCloudEnv(t)
	mock := &portsCloud{ports: []int{3000}}
	srv := httptest.NewServer(mock)
	defer srv.Close()
	ctx := context.Background()
	m, err := Connect(ctx, "mp", &ConnectOptions{Target: TargetCloud, BaseURL: srv.URL, APIKey: "smk_test"})
	if err != nil {
		t.Fatal(err)
	}

	ports, err := m.Ports(ctx)
	if err != nil {
		t.Fatal(err)
	}
	want := []PublishedPort{{Host: 43000, Guest: 3000, Protocol: "tcp"}, {Host: 0, Guest: 9999, Protocol: "tcp"}}
	if len(ports) != 2 || ports[0] != want[0] || ports[1] != want[1] {
		t.Fatalf("Ports = %+v, want %+v", ports, want)
	}

	// The node picks the host port, so Publish reports what took effect, not
	// the free port it proposed.
	got, err := m.Publish(ctx, PublishOptions{Guest: 8080})
	if err != nil {
		t.Fatal(err)
	}
	if got != (PublishedPort{Host: 48080, Guest: 8080, Protocol: "tcp"}) {
		t.Fatalf("Publish = %+v", got)
	}
	last := mock.puts[len(mock.puts)-1]
	b, _ := json.Marshal(last["ports"])
	if string(b) != `[{"port":3000},{"port":9999},{"port":8080}]` {
		t.Fatalf("PUT /ports sends guest ports only, deduplicated: %s", b)
	}

	if _, err := m.Publish(ctx, PublishOptions{Guest: 5353, Protocol: "udp"}); !errors.Is(err, ErrNotSupported) {
		t.Fatalf("UDP on the cloud = %v, want ErrNotSupported", err)
	}

	if err := m.Unpublish(ctx, UnpublishOptions{Guest: 3000}); err != nil {
		t.Fatal(err)
	}
	remaining := map[int]bool{}
	for _, p := range mock.puts[len(mock.puts)-1]["ports"].([]any) {
		remaining[int(p.(map[string]any)["port"].(float64))] = true
	}
	if len(remaining) != 2 || !remaining[8080] || !remaining[9999] {
		t.Fatalf("Unpublish by guest port left %v, want 8080 and 9999", remaining)
	}
	before := len(mock.puts)
	if err := m.Unpublish(ctx, UnpublishOptions{Guest: 1234}); err != nil || len(mock.puts) != before {
		t.Fatalf("Unpublish of an unpublished port must not PUT: err=%v puts=%d->%d", err, before, len(mock.puts))
	}
	if err := m.Unpublish(ctx, UnpublishOptions{}); !errors.Is(err, ErrInvalidConfig) {
		t.Fatalf("Unpublish with no port = %v, want ErrInvalidConfig", err)
	}

	links, err := m.PortLinks(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if links[8080] != "https://8080-tok.example/" || links[3000] != "https://3000-tok.example/" || len(links) != 2 {
		t.Fatalf("PortLinks = %v", links)
	}
	if err := m.RevokePortLinks(ctx); err != nil || !mock.revoked {
		t.Fatalf("RevokePortLinks err=%v revoked=%v", err, mock.revoked)
	}
}

func TestLocalPortLinksNotSupported(t *testing.T) {
	m := &Machine{t: &localTransport{n: "x"}}
	if _, err := m.PortLinks(context.Background()); !errors.Is(err, ErrNotSupported) {
		t.Fatalf("local PortLinks = %v", err)
	}
	if err := m.RevokePortLinks(context.Background()); !errors.Is(err, ErrNotSupported) {
		t.Fatalf("local RevokePortLinks = %v", err)
	}
}
