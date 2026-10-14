package smol

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"iter"
	"net"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"time"
)

// localTransport drives a machine in the embedded engine. The engine keeps
// every machine keyed by name, so the handle is the name plus how this
// process should treat it on a signal.
type localTransport struct {
	n string
	// owned: this process stops the machine on SIGINT/SIGTERM. False for a
	// machine borrowed through Connect, or a detached one.
	owned bool
	// handleSignals is passed on to branches of this machine.
	handleSignals bool
	interceptor   *EgressInterceptor
}

func newLocal(name string, owned bool, interceptor *EgressInterceptor, handleSignals bool) *localTransport {
	t := &localTransport{n: name, owned: owned, interceptor: interceptor, handleSignals: handleSignals}
	if owned {
		trackLocal(t)
	}
	return t
}

// localCall runs a native op on the local engine, loading it on first use.
// The engine's calls block; ctx is checked before the call, and only exec and
// exec streams can be cut short while they run.
func localCall(ctx context.Context, op string, args, out any) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if err := loadNative(); err != nil {
		return err
	}
	return callInto(op, args, out)
}

type nameArgs struct {
	Name string `json:"name"`
}

func (t *localTransport) name() string      { return t.n }
func (t *localTransport) machineID() string { return t.n }

func (t *localTransport) state(ctx context.Context) (string, error) {
	var s string
	err := localCall(ctx, "state", nameArgs{t.n}, &s)
	return s, err
}

func (t *localTransport) guestPorts(ctx context.Context) ([]int, error) {
	var ports []int
	err := localCall(ctx, "guest_ports", nameArgs{t.n}, &ports)
	return ports, err
}

func (t *localTransport) hostPort(ctx context.Context, guest int) (int, bool, error) {
	var host *int
	err := localCall(ctx, "host_port", map[string]any{"name": t.n, "guest_port": guest}, &host)
	if err != nil || host == nil {
		return 0, false, err
	}
	return *host, true, nil
}

// ready: running, and every published port's guest listener accepts.
func (t *localTransport) ready(ctx context.Context) (bool, error) {
	state, err := t.state(ctx)
	if err != nil || state != "running" {
		return false, err
	}
	guests, err := t.guestPorts(ctx)
	if err != nil {
		return false, err
	}
	for _, guest := range guests {
		host, ok, err := t.hostPort(ctx, guest)
		if err != nil || !ok {
			return false, err
		}
		if !localTCPReady(host) {
			return false, nil
		}
	}
	return true, nil
}

// localTCPReady: the host forwarder listens before the guest service does, so
// a completed connect alone is a false positive (the relay resets it when
// nothing is bound in the guest). A connection still open after a short
// window proves the guest listener is accepting.
func localTCPReady(port int) bool {
	c, err := net.DialTimeout("tcp", fmt.Sprintf("127.0.0.1:%d", port), 250*time.Millisecond)
	if err != nil {
		return false
	}
	defer c.Close()
	_ = c.SetReadDeadline(time.Now().Add(100 * time.Millisecond))
	var b [1]byte
	_, err = c.Read(b[:])
	var ne net.Error
	// Timing out with the connection open means nothing reset it.
	return err == nil || (errors.As(err, &ne) && ne.Timeout())
}

func (t *localTransport) readyAt(context.Context) (time.Time, error) {
	// The embedded engine keeps no readiness timestamp.
	return time.Time{}, nil
}

func (t *localTransport) waitUntilReady(ctx context.Context, opts WaitReadyOptions) error {
	return pollReady(ctx, t.n, opts, t.ready)
}

func (t *localTransport) endpoint(port int, path string) (PortEndpoint, error) {
	host, ok, err := t.hostPort(context.Background(), port)
	if err != nil {
		return PortEndpoint{}, err
	}
	if !ok {
		return PortEndpoint{}, invalidConfig(fmt.Sprintf("guest port %d is not published by local machine '%s'", port, t.n))
	}
	suffix := ""
	if path != "" {
		suffix = "/" + strings.TrimLeft(path, "/")
	}
	return PortEndpoint{
		HTTPURL: fmt.Sprintf("http://127.0.0.1:%d%s", host, suffix),
		WSURL:   fmt.Sprintf("ws://127.0.0.1:%d%s", host, suffix),
		Headers: map[string]string{},
	}, nil
}

func (t *localTransport) url(context.Context) (string, error) {
	// Public ingress is a cloud feature.
	return "", nil
}

type nativeEnv struct {
	Key   string `json:"key"`
	Value string `json:"value"`
}

func envList(env map[string]string) []nativeEnv {
	out := make([]nativeEnv, 0, len(env))
	for k, v := range env {
		out = append(out, nativeEnv{k, v})
	}
	return out
}

type nativeExecOptions struct {
	Env       []nativeEnv `json:"env,omitempty"`
	Workdir   string      `json:"workdir,omitempty"`
	TimeoutMS int64       `json:"timeout_ms,omitempty"`
	User      string      `json:"user,omitempty"`
}

func toNativeExec(o *ExecOptions) nativeExecOptions {
	if o == nil {
		return nativeExecOptions{}
	}
	n := nativeExecOptions{Env: envList(o.Env), Workdir: o.Workdir, User: o.User}
	if o.Timeout > 0 {
		n.TimeoutMS = max(1, o.Timeout.Milliseconds())
	}
	return n
}

type nativeExecResult struct {
	ExitCode int    `json:"exit_code"`
	Stdout   []byte `json:"stdout"`
	Stderr   []byte `json:"stderr"`
}

func (r nativeExecResult) result() *ExecResult {
	return &ExecResult{
		ExitCode:    r.ExitCode,
		Stdout:      string(r.Stdout),
		Stderr:      string(r.Stderr),
		StdoutBytes: r.Stdout,
		StderrBytes: r.Stderr,
	}
}

// exec runs a command. A context that can end makes it killable: only the
// engine's streaming path can kill a running command, so the call then goes
// through a canceller the context fires.
func (t *localTransport) exec(ctx context.Context, command []string, opts *ExecOptions) (*ExecResult, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if err := loadNative(); err != nil {
		return nil, err
	}
	args := map[string]any{"name": t.n, "command": command, "options": toNativeExec(opts)}
	var r nativeExecResult
	if ctx.Done() == nil {
		if err := callInto("exec", args, &r); err != nil {
			return nil, err
		}
		return r.result(), nil
	}
	var cancelID uint64
	if err := callInto("cancel_new", nil, &cancelID); err != nil {
		return nil, err
	}
	defer call("cancel_close", map[string]any{"id": cancelID})
	stop := context.AfterFunc(ctx, func() { call("cancel", map[string]any{"id": cancelID}) })
	defer stop()
	args["cancel"] = cancelID
	if err := callInto("exec", args, &r); err != nil {
		// A kill the context caused is reported as the context's error.
		if ctxErr := ctx.Err(); ctxErr != nil && errors.Is(err, ErrCancelled) {
			return nil, ctxErr
		}
		return nil, err
	}
	return r.result(), nil
}

func (t *localTransport) run(ctx context.Context, image string, command []string, opts *ExecOptions) (*ExecResult, error) {
	var r nativeExecResult
	args := map[string]any{"name": t.n, "image": image, "command": command, "options": toNativeExec(opts)}
	if err := localCall(ctx, "run", args, &r); err != nil {
		return nil, err
	}
	return r.result(), nil
}

type nativeEvent struct {
	Kind     string `json:"kind"`
	Data     []byte `json:"data"`
	ExitCode int    `json:"exit_code"`
	Message  string `json:"message"`
}

func (t *localTransport) execStream(ctx context.Context, command []string, opts *ExecOptions) iter.Seq2[ExecEvent, error] {
	return func(yield func(ExecEvent, error) bool) {
		var id uint64
		args := map[string]any{"name": t.n, "command": command, "options": toNativeExec(opts)}
		if err := localCall(ctx, "stream_open", args, &id); err != nil {
			yield(ExecEvent{}, err)
			return
		}
		ref := map[string]any{"id": id}
		// Closing kills a command still running: the consumer stopped early
		// (break, return or panic), so nothing should keep running in the
		// machine. A no-op once it has exited.
		defer call("stream_close", ref)
		stop := context.AfterFunc(ctx, func() { call("stream_kill", ref) })
		defer stop()
		for {
			var ev *nativeEvent
			if err := callInto("stream_next", ref, &ev); err != nil {
				yield(ExecEvent{}, err)
				return
			}
			if ev == nil {
				// A kill ends the stream too; report it as the cancellation it was.
				if err := ctx.Err(); err != nil {
					yield(ExecEvent{}, err)
				}
				return
			}
			if !yield(ExecEvent{Kind: EventKind(ev.Kind), Data: ev.Data, ExitCode: ev.ExitCode, Message: ev.Message}, nil) {
				return
			}
		}
	}
}

func (t *localTransport) readFile(ctx context.Context, path string) ([]byte, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if err := loadNative(); err != nil {
		return nil, err
	}
	_, data, err := callBytes("read_file", map[string]any{"name": t.n, "path": path}, nil, true)
	if data == nil && err == nil {
		data = []byte{}
	}
	return data, err
}

func (t *localTransport) writeFile(ctx context.Context, path string, data []byte, mode os.FileMode) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if err := loadNative(); err != nil {
		return err
	}
	args := map[string]any{"name": t.n, "path": path}
	if mode != 0 {
		args["mode"] = uint32(mode.Perm())
	}
	_, _, err := callBytes("write_file", args, data, false)
	return err
}

type nativeImage struct {
	Reference    string `json:"reference"`
	Digest       string `json:"digest"`
	Size         int64  `json:"size"`
	Architecture string `json:"architecture"`
	OS           string `json:"os"`
}

func (i nativeImage) info() ImageInfo { return ImageInfo(i) }

func (t *localTransport) pullImage(ctx context.Context, image string) (*ImageInfo, error) {
	var i nativeImage
	if err := localCall(ctx, "pull_image", map[string]any{"name": t.n, "image": image}, &i); err != nil {
		return nil, err
	}
	info := i.info()
	return &info, nil
}

func (t *localTransport) listImages(ctx context.Context) ([]ImageInfo, error) {
	var images []nativeImage
	if err := localCall(ctx, "list_images", nameArgs{t.n}, &images); err != nil {
		return nil, err
	}
	out := make([]ImageInfo, len(images))
	for i, img := range images {
		out[i] = img.info()
	}
	return out, nil
}

func (t *localTransport) sync(ctx context.Context) error {
	return localCall(ctx, "sync", nameArgs{t.n}, nil)
}

func (t *localTransport) setNetworkPolicy(ctx context.Context, policy NetworkPolicy) error {
	network, cidrs, hosts, err := egressPolicy(policy)
	if err != nil {
		return err
	}
	return localCall(ctx, "set_egress_policy", map[string]any{
		"name": t.n, "network": network, "cidrs": cidrs, "hosts": hosts,
	}, nil)
}

func (t *localTransport) stop(ctx context.Context) error {
	untrackLocal(t)
	return localCall(ctx, "stop", nameArgs{t.n}, nil)
}

func (t *localTransport) pause(ctx context.Context) error {
	if err := localCall(ctx, "pause", nameArgs{t.n}, nil); err != nil {
		return err
	}
	untrackLocal(t)
	return nil
}

func (t *localTransport) resume(ctx context.Context) error {
	if err := localCall(ctx, "resume", nameArgs{t.n}, nil); err != nil {
		return err
	}
	if t.owned {
		trackLocal(t)
	}
	return t.waitUntilReady(ctx, WaitReadyOptions{})
}

func interceptorArgs(name string, i *EgressInterceptor) map[string]any {
	args := map[string]any{"name": name}
	if i != nil {
		args["interceptor_address"] = i.Address
		args["interceptor_token"] = i.Token
	}
	return args
}

func (t *localTransport) start(ctx context.Context, interceptor *EgressInterceptor) error {
	binding := interceptor
	if binding == nil {
		binding = t.interceptor
	}
	if err := localCall(ctx, "start", interceptorArgs(t.n, binding), nil); err != nil {
		return err
	}
	if interceptor != nil {
		t.interceptor = interceptor
	}
	if t.owned {
		trackLocal(t)
	}
	return t.waitUntilReady(ctx, WaitReadyOptions{})
}

func (t *localTransport) delete(ctx context.Context) error {
	untrackLocal(t)
	return localCall(ctx, "delete", nameArgs{t.n}, nil)
}

func (t *localTransport) deleteWithUsage(context.Context) (*MachineUsageReport, error) {
	return nil, notSupported("DeleteWithUsage is a cloud-only metering feature; the local target has no billing.")
}

func (t *localTransport) usage(context.Context) (*MachineUsageReport, error) {
	return nil, notSupported("Usage is a cloud-only metering feature; the local target has no billing.")
}

func (t *localTransport) share(context.Context) (*ShareLink, error) {
	return nil, notSupported("Share is cloud-only; a local machine has no published app URL to hand out.")
}

func (t *localTransport) unshare(context.Context) error {
	return notSupported("Unshare is cloud-only; a local machine has no share link to revoke.")
}

func (t *localTransport) checkpoint(ctx context.Context, output string, opts CheckpointOptions) (*PortableCheckpointInfo, error) {
	if output == "" {
		return nil, invalidConfig("local checkpoint capture requires an output .smolcheckpoint path.")
	}
	path, err := filepath.Abs(output)
	if err != nil {
		return nil, err
	}
	args := map[string]any{"name": t.n, "output": path}
	if opts.Store != "" {
		store, err := filepath.Abs(opts.Store)
		if err != nil {
			return nil, err
		}
		args["store"] = store
	}
	var r struct {
		SizeBytes     int64   `json:"size_bytes"`
		ReusedBytes   int64   `json:"reused_bytes"`
		SourcePauseMS float64 `json:"source_pause_ms"`
		ElapsedMS     float64 `json:"elapsed_ms"`
	}
	if err := localCall(ctx, "checkpoint", args, &r); err != nil {
		return nil, err
	}
	return &PortableCheckpointInfo{
		ID:          path,
		MachineID:   t.n,
		Status:      "available",
		SizeBytes:   r.SizeBytes,
		ReusedBytes: r.ReusedBytes,
		Arch:        runtime.GOARCH,
		CreatedAt:   time.Now().UTC(),
		Path:        path,
		SourcePause: time.Duration(r.SourcePauseMS * float64(time.Millisecond)),
		Elapsed:     time.Duration(r.ElapsedMS * float64(time.Millisecond)),
	}, nil
}

func (t *localTransport) checkpoints(context.Context) ([]PortableCheckpointInfo, error) {
	return nil, notSupported("durable portable checkpoint listing is currently available on the cloud target.")
}

func (t *localTransport) resize(ctx context.Context, opts ResizeOptions) (*MachineResources, error) {
	fields := map[string]int{"CPUs": opts.CPUs, "MemoryMB": opts.MemoryMB, "StorageGB": opts.StorageGB, "OverlayGB": opts.OverlayGB}
	for field, v := range fields {
		if v < 0 {
			return nil, invalidConfig(fmt.Sprintf("resize %s must be a positive whole number", field))
		}
	}
	args := map[string]any{"name": t.n}
	if opts.CPUs > 0 {
		args["cpus"] = opts.CPUs
	}
	if opts.MemoryMB > 0 {
		args["memory_mib"] = opts.MemoryMB
	}
	if opts.StorageGB > 0 {
		args["storage_gib"] = opts.StorageGB
	}
	if opts.OverlayGB > 0 {
		args["overlay_gib"] = opts.OverlayGB
	}
	var r struct {
		CPUs       int  `json:"cpus"`
		MemoryMiB  int  `json:"memory_mib"`
		StorageGiB *int `json:"storage_gib"`
		OverlayGiB *int `json:"overlay_gib"`
	}
	if err := localCall(ctx, "resize", args, &r); err != nil {
		return nil, err
	}
	res := &MachineResources{CPUs: r.CPUs, MemoryMB: r.MemoryMiB}
	if r.StorageGiB != nil {
		res.StorageGB = *r.StorageGiB
	}
	if r.OverlayGiB != nil {
		res.OverlayGB = *r.OverlayGiB
	}
	return res, nil
}

type nativePort struct {
	Host  int `json:"host"`
	Guest int `json:"guest"`
}

func nativePorts(ports []PortSpec) []nativePort {
	out := make([]nativePort, len(ports))
	for i, p := range ports {
		out[i] = nativePort(p)
	}
	return out
}

// fork makes a live-RAM copy-on-write branch of this branchable machine.
func (t *localTransport) fork(ctx context.Context, name string, opts BranchOptions) (transport, error) {
	args := map[string]any{"name": t.n, "clone": name, "ports": nativePorts(opts.Ports), "checkpointable": opts.Branchable}
	if err := localCall(ctx, "fork", args, nil); err != nil {
		return nil, err
	}
	clone := newLocal(name, t.handleSignals, nil, t.handleSignals)
	if err := clone.waitUntilReady(ctx, WaitReadyOptions{}); err != nil {
		_ = clone.delete(context.Background())
		return nil, err
	}
	return clone, nil
}

func (t *localTransport) forkBatch(ctx context.Context, opts BranchBatchOptions) ([]transport, error) {
	names, err := resolveBatchNames(opts, "branch")
	if err != nil {
		return nil, err
	}
	args := map[string]any{"name": t.n, "clones": names, "ports": nativePorts(opts.Ports), "parallel": min(8, len(names))}
	if err := localCall(ctx, "fork_batch", args, nil); err != nil {
		return nil, err
	}
	branches := make([]transport, len(names))
	for i, name := range names {
		branches[i] = newLocal(name, t.handleSignals, nil, t.handleSignals)
	}
	for _, b := range branches {
		if err := b.waitUntilReady(ctx, WaitReadyOptions{}); err != nil {
			for _, b := range branches {
				_ = b.delete(context.Background())
			}
			return nil, err
		}
	}
	return branches, nil
}

func (t *localTransport) assign(context.Context, AssignOptions) (transport, string, string, error) {
	return nil, "", "", notSupported("Assign is a cloud feature: episode leases (idempotent assignment, " +
		"heartbeat, TTL reclaim) live in the control plane. On the local target use Branch/BranchBatch directly.")
}

func (t *localTransport) heartbeatLease(context.Context, string, string) error {
	return notSupported("HeartbeatLease is a cloud-only lease feature.")
}

func (t *localTransport) completeLease(context.Context, string, string, string, *float64, any) error {
	return notSupported("CompleteLease is a cloud-only lease feature.")
}

func (t *localTransport) getLease(context.Context, string) (map[string]any, error) {
	return nil, notSupported("GetLease is a cloud-only lease feature.")
}

// ---------------------------------------------------------------------------
// Local factories
// ---------------------------------------------------------------------------

type nativeMount struct {
	Source   string `json:"source"`
	Target   string `json:"target"`
	ReadOnly bool   `json:"read_only"`
	Staged   bool   `json:"staged"`
}

type nativeResources struct {
	CPUs           int      `json:"cpus,omitempty"`
	MemoryMiB      int      `json:"memory_mib,omitempty"`
	Network        *bool    `json:"network,omitempty"`
	AllowedCIDRs   []string `json:"allowed_cidrs,omitempty"`
	AllowedHosts   []string `json:"allowed_hosts,omitempty"`
	StorageGiB     int      `json:"storage_gib,omitempty"`
	OverlayGiB     int      `json:"overlay_gib,omitempty"`
	GPU            bool     `json:"gpu,omitempty"`
	GPUVRAMMiB     int      `json:"gpu_vram_mib,omitempty"`
	CUDA           bool     `json:"cuda,omitempty"`
	NetworkBackend string   `json:"network_backend,omitempty"`
}

type nativeMachineConfig struct {
	Name       string            `json:"name"`
	Image      string            `json:"image,omitempty"`
	Env        []nativeEnv       `json:"env,omitempty"`
	Workdir    string            `json:"workdir,omitempty"`
	User       string            `json:"user,omitempty"`
	Mounts     []nativeMount     `json:"mounts,omitempty"`
	Ports      []nativePort      `json:"ports,omitempty"`
	Resources  nativeResources   `json:"resources"`
	Persistent bool              `json:"persistent"`
	Forkable   bool              `json:"forkable"`
	Detached   bool              `json:"detached"`
	Labels     map[string]string `json:"labels,omitempty"`
}

func toNativeConfig(name string, cfg MachineConfig) nativeMachineConfig {
	r := cfg.Resources
	mounts := make([]nativeMount, len(cfg.Mounts))
	for i, m := range cfg.Mounts {
		mounts[i] = nativeMount(m)
	}
	return nativeMachineConfig{
		Name:    name,
		Image:   cfg.Image,
		Env:     envList(cfg.Env),
		Workdir: cfg.Workdir,
		User:    cfg.User,
		Mounts:  mounts,
		Ports:   nativePorts(cfg.Ports),
		Resources: nativeResources{
			CPUs:      r.CPUs,
			MemoryMiB: r.MemoryMB,
			Network:   resolveNetwork(cfg),
			// Either list enables scoped networking on its own; the engine
			// enforces both (CIDRs directly, hostnames via its DNS filter).
			AllowedCIDRs:   r.AllowCIDRs,
			AllowedHosts:   r.AllowHosts,
			StorageGiB:     r.StorageGB,
			OverlayGiB:     r.OverlayGB,
			GPU:            r.GPU,
			GPUVRAMMiB:     r.GPUVRAMMiB,
			CUDA:           r.CUDA,
			NetworkBackend: string(r.NetworkBackend),
		},
		// A machine meant to outlive this process has to outlive its record too.
		Persistent: cfg.Persistent || cfg.Detach,
		Forkable:   cfg.Branchable,
		Detached:   cfg.Detach,
		Labels:     cfg.Labels,
	}
}

func createLocal(ctx context.Context, cfg MachineConfig, c ConnectOptions) (transport, error) {
	name := cfg.Name
	if name == "" {
		name = generateName()
	}
	interceptor := cfg.EgressInterceptor
	if interceptor == nil {
		interceptor = c.EgressInterceptor
	}
	if interceptor != nil && cfg.Branchable {
		return nil, invalidConfig("EgressInterceptor cannot be combined with branchable machines.")
	}
	if cfg.AutoStop > 0 || cfg.TTL > 0 {
		return nil, notSupported("AutoStop and TTL are cloud-only; stop or delete a local machine yourself.")
	}
	if err := localCall(ctx, "create", toNativeConfig(name, cfg), nil); err != nil {
		return nil, err
	}
	// A detached machine is nobody's to stop on a signal: it was made to
	// survive this process, and its branches inherit that.
	owned := !cfg.Detach && !c.NoSignalHandling
	t := newLocal(name, owned, interceptor, owned)
	var err error
	if cfg.Branchable {
		// A branch source boots with memfd-backed guest RAM and a control
		// socket so Branch can clone it live.
		if err = localCall(ctx, "start_forkable", nameArgs{name}, nil); err == nil {
			err = t.waitUntilReady(ctx, WaitReadyOptions{})
		}
	} else {
		err = t.start(ctx, nil)
	}
	if err != nil {
		_ = t.delete(context.Background())
		return nil, err
	}
	return t, nil
}

func connectLocal(ctx context.Context, name string, c ConnectOptions) (transport, error) {
	// Connecting borrows an existing machine: a signal in this client must not
	// stop a machine another process or controller owns.
	if err := localCall(ctx, "connect", interceptorArgs(name, c.EgressInterceptor), nil); err != nil {
		return nil, err
	}
	t := newLocal(name, false, c.EgressInterceptor, !c.NoSignalHandling)
	// A frozen checkpoint is intentionally not agent-ready; it stays
	// connectable so callers can branch its retained snapshot.
	state, err := t.state(ctx)
	if err != nil {
		return nil, err
	}
	if state != "frozen" {
		if err := t.waitUntilReady(ctx, WaitReadyOptions{}); err != nil {
			return nil, err
		}
	}
	return t, nil
}

func listLocal() ([]MachineSummary, error) {
	var rows []struct {
		Name       string            `json:"name"`
		State      string            `json:"state"`
		Image      string            `json:"image"`
		Labels     map[string]string `json:"labels"`
		PID        int               `json:"pid"`
		Persistent bool              `json:"persistent"`
		Detached   bool              `json:"detached"`
		Branchable bool              `json:"branchable"`
		CreatedAt  json.Number       `json:"created_at"`
	}
	if err := localCall(context.Background(), "list", nil, &rows); err != nil {
		return nil, err
	}
	out := make([]MachineSummary, len(rows))
	for i, r := range rows {
		secs, _ := r.CreatedAt.Int64()
		labels := r.Labels
		if labels == nil {
			labels = map[string]string{}
		}
		out[i] = MachineSummary{
			Name: r.Name, ID: r.Name, State: r.State, Image: r.Image, Labels: labels, PID: r.PID,
			Persistent: r.Persistent, Detached: r.Detached, Branchable: r.Branchable,
			CreatedAt: time.Unix(secs, 0).UTC(),
		}
	}
	return out, nil
}

func restoreLocal(ctx context.Context, checkpoint, name string, c ConnectOptions, opts RestoreCheckpointOptions) (transport, error) {
	path, err := filepath.Abs(checkpoint)
	if err != nil {
		return nil, err
	}
	args := map[string]any{"name": name, "artifact": path, "keep_identity": opts.KeepIdentity}
	if err := localCall(ctx, "restore_checkpoint", args, nil); err != nil {
		return nil, err
	}
	t := newLocal(name, !c.NoSignalHandling, nil, !c.NoSignalHandling)
	// Between install and first boot: the one moment a restored machine's
	// policy can be its own rather than the checkpoint's.
	if opts.NetworkPolicy != nil {
		err = t.setNetworkPolicy(ctx, *opts.NetworkPolicy)
	}
	if err == nil {
		err = t.start(ctx, nil)
	}
	if err != nil {
		_ = t.delete(context.Background())
		return nil, err
	}
	return t, nil
}

func localAvailability() LocalAvailability {
	if !localPlatformSupported() {
		return LocalAvailability{Code: "UNSUPPORTED_PLATFORM", Reason: fmt.Sprintf(
			"No prebuilt local engine for %s/%s (supported: macOS Apple Silicon, Linux amd64/arm64 with glibc). "+
				"The cloud target works on any platform.", runtime.GOOS, runtime.GOARCH)}
	}
	if err := loadNative(); err != nil {
		return LocalAvailability{Code: "RUNTIME_NOT_INSTALLED", Reason: "The local engine could not be loaded: " + err.Error()}
	}
	var host struct {
		Available bool   `json:"available"`
		Code      string `json:"code"`
		Reason    string `json:"reason"`
	}
	if err := callInto("check_host", nil, &host); err != nil {
		return LocalAvailability{Code: CodeSmolvm, Reason: err.Error()}
	}
	return LocalAvailability{Available: host.Available, Code: host.Code, Reason: host.Reason}
}
