package smol

import (
	"context"
	"errors"
	"fmt"
	"os"
	"strings"
	"testing"
	"time"
)

// End-to-end tests against the real local engine. They boot microVMs, so they
// run only with SMOL_E2E=1 on a host the engine supports (macOS Apple Silicon,
// or Linux with /dev/kvm), with network access to pull the test image. Point
// SMOLMACHINES_RUNTIME_DIR at an unpacked runtime bundle to test a local build.

func requireE2E(t *testing.T) {
	t.Helper()
	if os.Getenv("SMOL_E2E") != "1" {
		t.Skip("set SMOL_E2E=1 to boot real machines")
	}
	if a := CheckLocal(); !a.Available {
		t.Fatalf("local machines unavailable: [%s] %s", a.Code, a.Reason)
	}
}

func newE2EMachine(t *testing.T, cfg MachineConfig) *Machine {
	t.Helper()
	ctx := context.Background()
	start := time.Now()
	m, err := Create(ctx, cfg, &ConnectOptions{Target: TargetLocal})
	if err != nil {
		t.Fatalf("Create: %v", err)
	}
	t.Logf("created %q in %s", m.Name(), time.Since(start).Round(time.Millisecond))
	t.Cleanup(func() { _ = m.Delete(context.Background()) })
	return m
}

func TestE2ELifecycle(t *testing.T) {
	requireE2E(t)
	ctx := context.Background()
	m := newE2EMachine(t, MachineConfig{Resources: ResourceSpec{CPUs: 2, MemoryMB: 1024, Network: Bool(true)}})

	if s, err := m.State(ctx); err != nil || s != "running" {
		t.Fatalf("State = %q, %v; want running", s, err)
	}

	echo, err := m.Exec(ctx, []string{"echo", "hello-from-vm"}, nil)
	if err != nil {
		t.Fatalf("Exec: %v", err)
	}
	if !echo.Success() || strings.TrimSpace(echo.Stdout) != "hello-from-vm" {
		t.Fatalf("Exec = %d %q (stderr %q)", echo.ExitCode, echo.Stdout, echo.Stderr)
	}

	bad, err := m.Exec(ctx, []string{"sh", "-c", "exit 7"}, nil)
	if err != nil {
		t.Fatalf("Exec exit 7: %v", err)
	}
	var execErr *ExecutionError
	if err := bad.AssertSuccess(); !errors.As(err, &execErr) || execErr.ExitCode != 7 || !errors.Is(err, ErrCommandFailed) {
		t.Fatalf("AssertSuccess = %v; want ExecutionError(7)", err)
	}

	env, err := m.Exec(ctx, []string{"sh", "-c", "echo $GREETING; pwd"}, &ExecOptions{Env: map[string]string{"GREETING": "hi"}, Workdir: "/tmp"})
	if err != nil || env.Stdout != "hi\n/tmp\n" {
		t.Fatalf("Exec with env/workdir = %q, %v", env.Stdout, err)
	}

	// Binary-safe file round trip, including a NUL and invalid UTF-8.
	payload := []byte(fmt.Sprintf("roundtrip-%d\x00\xff\xfe", time.Now().UnixNano()))
	if err := m.WriteFile(ctx, "/tmp/smol-e2e.bin", payload, 0o600); err != nil {
		t.Fatalf("WriteFile: %v", err)
	}
	back, err := m.ReadFile(ctx, "/tmp/smol-e2e.bin")
	if err != nil || string(back) != string(payload) {
		t.Fatalf("ReadFile = %q, %v; want %q", back, err, payload)
	}
	mode, err := m.Exec(ctx, []string{"stat", "-c", "%a", "/tmp/smol-e2e.bin"}, nil)
	if err != nil || strings.TrimSpace(mode.Stdout) != "600" {
		t.Fatalf("file mode = %q (exit %d, stderr %q), %v; want 600", mode.Stdout, mode.ExitCode, mode.Stderr, err)
	}

	py, err := m.Run(ctx, "python:3.12-alpine", []string{"python", "-c", "print(2 ** 10)"}, nil)
	if err != nil {
		t.Fatalf("Run: %v", err)
	}
	if py.ExitCode != 0 || strings.TrimSpace(py.Stdout) != "1024" {
		t.Fatalf("Run = %d %q (stderr %q)", py.ExitCode, py.Stdout, py.Stderr)
	}

	var out strings.Builder
	sawExit := false
	for ev, err := range m.ExecStream(ctx, []string{"sh", "-c", "echo line1; echo line2 >&2; exit 3"}, nil) {
		if err != nil {
			t.Fatalf("ExecStream: %v", err)
		}
		switch ev.Kind {
		case EventStdout, EventStderr:
			out.Write(ev.Data)
		case EventExit:
			sawExit = ev.ExitCode == 3
		}
	}
	if !sawExit || !strings.Contains(out.String(), "line1") || !strings.Contains(out.String(), "line2") {
		t.Fatalf("stream: exit3=%v output=%q", sawExit, out.String())
	}

	images, err := m.ListImages(ctx)
	if err != nil {
		t.Fatalf("ListImages: %v", err)
	}
	found := false
	for _, i := range images {
		found = found || strings.Contains(i.Reference, "python")
	}
	if !found {
		t.Fatalf("ListImages = %+v; want the pulled python image", images)
	}

	all, err := List(ctx, &ConnectOptions{Target: TargetLocal}, ListOptions{})
	if err != nil {
		t.Fatalf("List: %v", err)
	}
	listed := false
	for _, s := range all {
		listed = listed || (s.Name == m.Name() && s.State == "running" && s.PID > 0)
	}
	if !listed {
		t.Fatalf("List does not show %q running", m.Name())
	}

	if err := m.Delete(ctx); err != nil {
		t.Fatalf("Delete: %v", err)
	}
	if s, _ := m.State(ctx); s != "stopped" {
		t.Fatalf("State after Delete = %q; want stopped", s)
	}
}

// heartbeat rewrites a file every 200ms until killed, so a test can tell
// whether the command is still running in the machine.
func heartbeat(file string) []string {
	return []string{"sh", "-c", fmt.Sprintf("while true; do date +%%s%%N > /tmp/%s; echo tick; sleep 0.2; done", file)}
}

func beating(t *testing.T, m *Machine, file string) bool {
	t.Helper()
	ctx := context.Background()
	read := func() string {
		r, err := m.Exec(ctx, []string{"sh", "-c", "cat /tmp/" + file + " 2>/dev/null || echo absent"}, nil)
		if err != nil {
			t.Fatalf("read heartbeat: %v", err)
		}
		return strings.TrimSpace(r.Stdout)
	}
	first := read()
	time.Sleep(700 * time.Millisecond)
	return first != "absent" && read() != first
}

func TestE2EExecControl(t *testing.T) {
	requireE2E(t)
	m := newE2EMachine(t, MachineConfig{Resources: ResourceSpec{CPUs: 1, MemoryMB: 512}})

	// Ending the context kills a streaming command.
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		var last error
		for _, err := range m.ExecStream(ctx, heartbeat("hb-cancel"), nil) {
			last = err
		}
		done <- last
	}()
	time.Sleep(time.Second)
	if !beating(t, m, "hb-cancel") {
		t.Fatal("streamed command is not running before cancel")
	}
	cancel()
	select {
	case err := <-done:
		if !errors.Is(err, context.Canceled) {
			t.Fatalf("cancelled stream ended with %v; want context.Canceled", err)
		}
	case <-time.After(15 * time.Second):
		t.Fatal("cancelled stream did not end")
	}
	if beating(t, m, "hb-cancel") {
		t.Fatal("cancelled stream's command is still running")
	}

	// Breaking out of the loop kills the command.
	for range m.ExecStream(context.Background(), heartbeat("hb-break"), nil) {
		time.Sleep(time.Second)
		break
	}
	if beating(t, m, "hb-break") {
		t.Fatal("breaking out of the loop left the command running")
	}

	// A deadline kills a plain Exec.
	ctx, cancel = context.WithTimeout(context.Background(), 1500*time.Millisecond)
	defer cancel()
	start := time.Now()
	if _, err := m.Exec(ctx, heartbeat("hb-exec"), nil); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("Exec past its deadline = %v; want context.DeadlineExceeded", err)
	}
	if took := time.Since(start); took > 10*time.Second {
		t.Fatalf("cancelled Exec took %s to return", took)
	}
	if beating(t, m, "hb-exec") {
		t.Fatal("timed-out Exec's command is still running")
	}

	// An already-ended context starts nothing.
	pre, preCancel := context.WithCancel(context.Background())
	preCancel()
	if _, err := m.Exec(pre, heartbeat("hb-pre"), nil); !errors.Is(err, context.Canceled) {
		t.Fatalf("Exec with a cancelled context = %v; want context.Canceled", err)
	}
	never, err := m.Exec(context.Background(), []string{"sh", "-c", "cat /tmp/hb-pre 2>/dev/null || echo absent"}, nil)
	if err != nil || strings.TrimSpace(never.Stdout) != "absent" {
		t.Fatalf("pre-cancelled command ran: %q, %v", never.Stdout, err)
	}

	// ExecOptions.Timeout kills the command, which exits 124 like timeout(1).
	start = time.Now()
	timedOut, err := m.Exec(context.Background(), []string{"sleep", "30"}, &ExecOptions{Timeout: time.Second})
	if err != nil || timedOut.ExitCode != 124 || time.Since(start) > 10*time.Second {
		t.Fatalf("Exec of sleep 30 with a 1s timeout = %+v, %v after %s; want exit 124 within 10s", timedOut, err, time.Since(start))
	}
}

func TestE2EBranch(t *testing.T) {
	requireE2E(t)
	ctx := context.Background()
	source := newE2EMachine(t, MachineConfig{Branchable: true, Resources: ResourceSpec{CPUs: 1, MemoryMB: 512}})
	if _, err := source.Exec(ctx, []string{"sh", "-c", "echo warm > /root/state"}, nil); err != nil {
		t.Fatalf("seed source: %v", err)
	}
	child, err := source.Branch(ctx, source.Name()+"-child", BranchOptions{})
	if err != nil {
		t.Fatalf("Branch: %v", err)
	}
	t.Cleanup(func() { _ = child.Delete(context.Background()) })
	got, err := child.Exec(ctx, []string{"cat", "/root/state"}, nil)
	if err != nil || strings.TrimSpace(got.Stdout) != "warm" {
		t.Fatalf("child state = %q, %v; want warm", got.Stdout, err)
	}
	if _, err := child.Exec(ctx, []string{"sh", "-c", "echo child > /root/state"}, nil); err != nil {
		t.Fatalf("write child: %v", err)
	}
	src, err := source.Exec(ctx, []string{"cat", "/root/state"}, nil)
	if err != nil || strings.TrimSpace(src.Stdout) != "warm" {
		t.Fatalf("source state after child write = %q, %v; want warm", src.Stdout, err)
	}

	reward, err := source.RewardBranch(ctx, []string{"cat", "/root/state"}, nil, "")
	if err != nil || strings.TrimSpace(reward.Stdout) != "warm" {
		t.Fatalf("RewardBranch = %q, %v", reward.Stdout, err)
	}
}

func TestE2ENotSupportedLocally(t *testing.T) {
	requireE2E(t)
	m := newE2EMachine(t, MachineConfig{Resources: ResourceSpec{CPUs: 1, MemoryMB: 512}})
	ctx := context.Background()
	if _, err := m.Usage(ctx); !errors.Is(err, ErrNotSupported) {
		t.Fatalf("Usage = %v; want ErrNotSupported", err)
	}
	if _, err := Connect(ctx, "smol-go-e2e-does-not-exist", &ConnectOptions{Target: TargetLocal}); !errors.Is(err, ErrNotFound) {
		t.Fatalf("Connect to a missing machine = %v; want ErrNotFound", err)
	}
}
