package smol

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"iter"
	"os"
	"path/filepath"
	"time"
)

// Machine is an isolated microVM, local (embedded engine) or on Smol Cloud.
// The backend is picked by ConnectOptions; calling code is the same for both,
// and an operation a backend lacks returns ErrNotSupported.
//
// The local engine's calls block their goroutine until the engine answers;
// ctx is checked before each call, and Exec and ExecStream are killed when it
// ends.
type Machine struct {
	t transport
}

// Create creates and starts a machine, returning once it is ready for work.
// conn may be nil for the default target.
func Create(ctx context.Context, cfg MachineConfig, conn *ConnectOptions) (*Machine, error) {
	t, err := makeTransport(ctx, cfg, connOrZero(conn))
	if err != nil {
		return nil, err
	}
	return &Machine{t}, nil
}

// Connect attaches to an existing machine without creating one. Locally it
// re-opens a persisted machine by name, starting it if stopped. On the cloud
// it looks the machine up by id or name; it does not wait for readiness, so
// call WaitUntilReady before Exec.
func Connect(ctx context.Context, id string, conn *ConnectOptions) (*Machine, error) {
	t, err := connectTransport(ctx, id, connOrZero(conn))
	if err != nil {
		return nil, err
	}
	return &Machine{t}, nil
}

// List lists every machine the target knows about, including ones other
// processes made: locally the engine's shared database (the one `smol machine
// ls` reads), on the cloud the account's machines. Filter by Labels to find
// your own, then Connect to pick one up.
func List(ctx context.Context, conn *ConnectOptions, opts ListOptions) ([]MachineSummary, error) {
	return listMachines(ctx, connOrZero(conn), opts)
}

// RestoreCheckpoint restores a local .smolcheckpoint path or a durable cloud
// checkpoint id into a new machine named name, returning once it is ready.
// With a nil conn, a path that exists here (or looks like one) restores
// locally and anything else restores on the cloud; a local file restored on
// the cloud is uploaded first.
func RestoreCheckpoint(ctx context.Context, checkpoint, name string, conn *ConnectOptions, opts RestoreCheckpointOptions) (*Machine, error) {
	t, err := restoreCheckpointTransport(ctx, checkpoint, name, conn, opts)
	if err != nil {
		return nil, err
	}
	return &Machine{t}, nil
}

// UploadCheckpoint uploads a checkpoint file taken on this host to the cloud,
// so it can be restored there by the returned id. progress, if non-nil, is
// called as bytes are sent.
func UploadCheckpoint(ctx context.Context, path string, conn *ConnectOptions, progress func(sent, total int64)) (*CloudCheckpointInfo, error) {
	return uploadCheckpointCloud(ctx, path, connOrZero(conn), progress)
}

// ExportCheckpoint exports a local stored checkpoint directory as one
// portable file and returns its size in bytes.
func ExportCheckpoint(source, output string) (int64, error) {
	src, err := filepath.Abs(source)
	if err != nil {
		return 0, err
	}
	out, err := filepath.Abs(output)
	if err != nil {
		return 0, err
	}
	var n int64
	err = localCall(context.Background(), "export_checkpoint", map[string]any{"source": src, "output": out}, &n)
	return n, err
}

// PruneCheckpointStore removes objects no retained checkpoint in a local
// store references and returns the bytes freed.
func PruneCheckpointStore(store string) (int64, error) {
	dir, err := filepath.Abs(store)
	if err != nil {
		return 0, err
	}
	var n int64
	err = localCall(context.Background(), "prune_checkpoint_store", map[string]any{"store": dir}, &n)
	return n, err
}

// CheckLocal reports whether this host can run local machines, answered
// without booting one: a runtime exists for the platform, the engine loads
// (downloading its bundle on first use), and the engine's host checks pass
// (/dev/kvm on Linux, a locatable libkrun everywhere). Unavailable hosts get
// the code a failing local Create would return. Use it to choose between a
// local machine and another sandbox up front.
func CheckLocal() LocalAvailability { return localAvailability() }

func connOrZero(c *ConnectOptions) ConnectOptions {
	if c == nil {
		return ConnectOptions{}
	}
	return *c
}

// Name is the machine's name.
func (m *Machine) Name() string { return m.t.name() }

// ID is the cloud mach-… id, or the name locally.
func (m *Machine) ID() string { return m.t.machineID() }

// State is the lifecycle state. On the cloud, "started" means only that the
// VM process launched; use Ready or WaitUntilReady before doing work.
func (m *Machine) State(ctx context.Context) (string, error) { return m.t.state(ctx) }

// Ready reports whether the machine can do work: its agent is reachable and
// every published port accepts connections.
func (m *Machine) Ready(ctx context.Context) (bool, error) { return m.t.ready(ctx) }

// ReadyAt is when the machine first became ready; zero if not yet ready or
// on the local target, which keeps no timestamp.
func (m *Machine) ReadyAt(ctx context.Context) (time.Time, error) { return m.t.readyAt(ctx) }

// WaitUntilReady blocks until Ready, a failed or stopped state, the timeout
// (default 120s), or the end of ctx. Create already waits; this is for
// machines attached with Connect.
func (m *Machine) WaitUntilReady(ctx context.Context, opts WaitReadyOptions) error {
	return m.t.waitUntilReady(ctx, opts)
}

// Endpoint is how to reach a published guest port: localhost locally, the
// authenticated connect bridge on the cloud.
func (m *Machine) Endpoint(port int, path string) (PortEndpoint, error) {
	return m.t.endpoint(port, path)
}

// URL is the public ingress URL of the machine's first published port on the
// cloud; empty until it has one, and always empty locally.
func (m *Machine) URL(ctx context.Context) (string, error) { return m.t.url(ctx) }

// Exec runs a command directly in the machine. opts may be nil. Ending ctx
// kills the command and returns ctx's error.
func (m *Machine) Exec(ctx context.Context, command []string, opts *ExecOptions) (*ExecResult, error) {
	return m.t.exec(ctx, command, opts)
}

// Run pulls image if needed and runs a command in a container of it. Local only.
func (m *Machine) Run(ctx context.Context, image string, command []string, opts *ExecOptions) (*ExecResult, error) {
	return m.t.run(ctx, image, command, opts)
}

// ExecStream runs a command and yields its output as it arrives, ending with
// an exit event. Stopping the loop early, or ending ctx, kills the command.
//
//	for ev, err := range m.ExecStream(ctx, []string{"make"}, nil) {
//		if err != nil { return err }
//		if ev.Kind == smol.EventStdout { os.Stdout.Write(ev.Data) }
//	}
func (m *Machine) ExecStream(ctx context.Context, command []string, opts *ExecOptions) iter.Seq2[ExecEvent, error] {
	return m.t.execStream(ctx, command, opts)
}

// ReadFile reads a file from the machine.
func (m *Machine) ReadFile(ctx context.Context, path string) ([]byte, error) {
	return m.t.readFile(ctx, path)
}

// WriteFile writes a file into the machine. A zero mode keeps the default.
func (m *Machine) WriteFile(ctx context.Context, path string, data []byte, mode os.FileMode) error {
	return m.t.writeFile(ctx, path, data, mode)
}

// PullImage pulls an OCI image into the machine's storage. Local only.
func (m *Machine) PullImage(ctx context.Context, image string) (*ImageInfo, error) {
	return m.t.pullImage(ctx, image)
}

// ListImages lists cached OCI images. Local only.
func (m *Machine) ListImages(ctx context.Context) ([]ImageInfo, error) { return m.t.listImages(ctx) }

// Sync copies staged mounts back to their host directories without stopping.
func (m *Machine) Sync(ctx context.Context) error { return m.t.sync(ctx) }

// SetNetworkPolicy replaces this stopped machine's outbound network policy. A
// machine that resumes saved memory keeps its network backend, so a policy
// needing a different one is refused. Local only.
func (m *Machine) SetNetworkPolicy(ctx context.Context, policy NetworkPolicy) error {
	return m.t.setNetworkPolicy(ctx, policy)
}

// Start boots a stopped machine with its disk state, not its previous RAM.
// interceptor rebinds intercepted egress for a local machine; nil reuses the
// previous binding.
func (m *Machine) Start(ctx context.Context, interceptor *EgressInterceptor) error {
	return m.t.start(ctx, interceptor)
}

// Stop stops the machine.
func (m *Machine) Stop(ctx context.Context) error { return m.t.stop(ctx) }

// Pause saves RAM and disk durably, then stops at that execution boundary.
func (m *Machine) Pause(ctx context.Context) error { return m.t.pause(ctx) }

// Resume resumes saved execution.
func (m *Machine) Resume(ctx context.Context) error { return m.t.resume(ctx) }

// Delete stops the machine and deletes its storage.
func (m *Machine) Delete(ctx context.Context) error { return m.t.delete(ctx) }

// DeleteWithUsage deletes the machine and returns its settled usage and cost;
// the control plane samples metering once more before teardown. Cloud only.
func (m *Machine) DeleteWithUsage(ctx context.Context) (*MachineUsageReport, error) {
	return m.t.deleteWithUsage(ctx)
}

// Usage is the machine's metered usage and cost, readable up to 30 days after
// delete. CPU and memory accrue through a periodic rollup, so a mid-life read
// is a lower bound. Cloud only.
func (m *Machine) Usage(ctx context.Context) (*MachineUsageReport, error) { return m.t.usage(ctx) }

// Share mints an anonymous link to the machine's published app, replacing any
// previous one. Treat the URL as a credential. Cloud only.
func (m *Machine) Share(ctx context.Context) (*ShareLink, error) { return m.t.share(ctx) }

// Unshare revokes the machine's share link. Cloud only.
func (m *Machine) Unshare(ctx context.Context) error { return m.t.unshare(ctx) }

// Checkpoint captures this running branchable machine. Locally output is the
// .smolcheckpoint path to write (or, with opts.Store, a self-contained
// directory); on the cloud output is ignored and the artifact is stored durably.
func (m *Machine) Checkpoint(ctx context.Context, output string, opts CheckpointOptions) (*PortableCheckpointInfo, error) {
	return m.t.checkpoint(ctx, output, opts)
}

// Checkpoints lists durable checkpoints captured from this machine. Cloud only.
func (m *Machine) Checkpoints(ctx context.Context) ([]PortableCheckpointInfo, error) {
	return m.t.checkpoints(ctx)
}

// Resize grows this running machine without rebooting it. Sizes are totals;
// every target is checked before anything changes, then RAM, CPUs and disks
// are applied in that order. Local only.
func (m *Machine) Resize(ctx context.Context, opts ResizeOptions) (*MachineResources, error) {
	return m.t.resize(ctx, opts)
}

// Branch makes an independent child of this running branchable source. The
// child inherits warm RAM and disk through copy-on-write, and the source stays
// available for more branches. Create the source with Branchable.
func (m *Machine) Branch(ctx context.Context, name string, opts BranchOptions) (*Machine, error) {
	t, err := m.t.fork(ctx, name, opts)
	if err != nil {
		return nil, err
	}
	return &Machine{t}, nil
}

// BranchBatch branches this source into many children in one call, returned
// in request order. On the cloud the batch is transactional.
func (m *Machine) BranchBatch(ctx context.Context, opts BranchBatchOptions) ([]*Machine, error) {
	ts, err := m.t.forkBatch(ctx, opts)
	if err != nil {
		return nil, err
	}
	out := make([]*Machine, len(ts))
	for i, t := range ts {
		out[i] = &Machine{t}
	}
	return out, nil
}

// Assign leases one provisioned branch of this branchable golden for an RL
// episode, returned only once fully installed. Idempotent on opts.LeaseID.
// Always Complete the episode so its branch is reclaimed. Cloud only.
func (m *Machine) Assign(ctx context.Context, opts AssignOptions) (*Episode, error) {
	t, leaseID, ownerToken, err := m.t.assign(ctx, opts)
	if err != nil {
		return nil, err
	}
	return &Episode{Machine: &Machine{t}, LeaseID: leaseID, ownerToken: ownerToken, lease: m.t}, nil
}

// RewardBranch scores this machine's state without changing it: it branches
// a throwaway child, runs command in it, and deletes the child whatever
// happens. name may be empty for a generated one. The machine must be
// branchable.
func (m *Machine) RewardBranch(ctx context.Context, command []string, opts *ExecOptions, name string) (*ExecResult, error) {
	if name == "" {
		var b [4]byte
		_, _ = rand.Read(b[:])
		name = m.Name() + "-rf-" + hex.EncodeToString(b[:])
	}
	child, err := m.Branch(ctx, name, BranchOptions{})
	if err != nil {
		return nil, err
	}
	defer child.Delete(context.WithoutCancel(ctx))
	return child.Exec(ctx, command, opts)
}

// Episode is a leased RL episode: a provisioned branch plus its lease, made
// by Machine.Assign. Cloud only.
type Episode struct {
	// Machine is the episode's provisioned branch.
	Machine    *Machine
	LeaseID    string
	ownerToken string
	lease      transport
	completed  bool
}

// Exec runs a command in the episode's machine.
func (e *Episode) Exec(ctx context.Context, command []string, opts *ExecOptions) (*ExecResult, error) {
	return e.Machine.Exec(ctx, command, opts)
}

// Heartbeat keeps the lease alive. Call it within the heartbeat cadence the
// episode was assigned with, or it is reclaimed.
func (e *Episode) Heartbeat(ctx context.Context) error {
	return e.lease.heartbeatLease(ctx, e.LeaseID, e.ownerToken)
}

// Complete finishes the episode with a termination reason (done,
// agent_failed, infra_failed, cancelled) and tears its branch down. score and
// result are optional. A second call is a no-op.
func (e *Episode) Complete(ctx context.Context, reason string, score *float64, result any) error {
	if e.completed {
		return nil
	}
	if reason == "" {
		reason = "done"
	}
	if err := e.lease.completeLease(ctx, e.LeaseID, e.ownerToken, reason, score, result); err != nil {
		return err
	}
	e.completed = true
	return nil
}

// Status is the lease's current status and, once completed, its outcome.
func (e *Episode) Status(ctx context.Context) (map[string]any, error) {
	return e.lease.getLease(ctx, e.LeaseID)
}
