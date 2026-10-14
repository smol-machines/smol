package smol

import (
	"context"
	"crypto/rand"
	"fmt"
	"iter"
	"os"
	"strconv"
	"strings"
	"time"
)

// transport is the seam that lets one Machine API drive either the embedded
// local engine or the Smol Cloud control plane, chosen at connect time.
// Features a backend lacks return ErrNotSupported.
type transport interface {
	name() string
	machineID() string
	state(ctx context.Context) (string, error)
	ready(ctx context.Context) (bool, error)
	readyAt(ctx context.Context) (time.Time, error)
	waitUntilReady(ctx context.Context, opts WaitReadyOptions) error
	endpoint(port int, path string) (PortEndpoint, error)
	url(ctx context.Context) (string, error)
	exec(ctx context.Context, command []string, opts *ExecOptions) (*ExecResult, error)
	run(ctx context.Context, image string, command []string, opts *ExecOptions) (*ExecResult, error)
	execStream(ctx context.Context, command []string, opts *ExecOptions) iter.Seq2[ExecEvent, error]
	readFile(ctx context.Context, path string) ([]byte, error)
	writeFile(ctx context.Context, path string, data []byte, mode os.FileMode) error
	pullImage(ctx context.Context, image string) (*ImageInfo, error)
	listImages(ctx context.Context) ([]ImageInfo, error)
	sync(ctx context.Context) error
	setNetworkPolicy(ctx context.Context, policy NetworkPolicy) error
	stop(ctx context.Context) error
	pause(ctx context.Context) error
	resume(ctx context.Context) error
	start(ctx context.Context, interceptor *EgressInterceptor) error
	delete(ctx context.Context) error
	deleteWithUsage(ctx context.Context) (*MachineUsageReport, error)
	usage(ctx context.Context) (*MachineUsageReport, error)
	share(ctx context.Context) (*ShareLink, error)
	unshare(ctx context.Context) error
	checkpoint(ctx context.Context, output string, opts CheckpointOptions) (*PortableCheckpointInfo, error)
	checkpoints(ctx context.Context) ([]PortableCheckpointInfo, error)
	resize(ctx context.Context, opts ResizeOptions) (*MachineResources, error)
	fork(ctx context.Context, name string, opts BranchOptions) (transport, error)
	forkBatch(ctx context.Context, opts BranchBatchOptions) ([]transport, error)
	assign(ctx context.Context, opts AssignOptions) (t transport, leaseID, ownerToken string, err error)
	heartbeatLease(ctx context.Context, leaseID, ownerToken string) error
	completeLease(ctx context.Context, leaseID, ownerToken, reason string, score *float64, result any) error
	getLease(ctx context.Context, leaseID string) (map[string]any, error)
}

const (
	defaultReadyTimeout  = 120 * time.Second
	defaultReadyInterval = time.Second
)

func (o WaitReadyOptions) withDefaults() WaitReadyOptions {
	if o.Timeout <= 0 {
		o.Timeout = defaultReadyTimeout
	}
	if o.Interval <= 0 {
		o.Interval = defaultReadyInterval
	}
	return o
}

// pollReady polls ready until it reports true, the timeout passes, or ctx ends.
func pollReady(ctx context.Context, name string, opts WaitReadyOptions, ready func(context.Context) (bool, error)) error {
	opts = opts.withDefaults()
	deadline := time.Now().Add(opts.Timeout)
	for {
		ok, err := ready(ctx)
		if err != nil {
			return err
		}
		if ok {
			return nil
		}
		if time.Now().Add(opts.Interval).After(deadline) {
			return &Error{Code: CodeTimeout, Message: fmt.Sprintf("machine '%s' did not become ready within %s", name, opts.Timeout)}
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(opts.Interval):
		}
	}
}

// selectsCloud reports whether a request targets the cloud. Only an explicit
// credential may select it: ConnectOptions.APIKey and SMOL_CLOUD_TOKEN are
// deliberate acts by the caller, whereas a CLI session on disk is a side
// effect of `smol auth login` and must never flip the default away from local.
func selectsCloud(c ConnectOptions) bool {
	switch c.Target {
	case TargetCloud:
		return true
	case TargetLocal:
		return false
	}
	return c.APIKey != "" || os.Getenv("SMOL_CLOUD_TOKEN") != ""
}

func generateName() string {
	var b [3]byte
	_, _ = rand.Read(b[:])
	return fmt.Sprintf("smol-%s-%x", strconv.FormatInt(time.Now().UnixMilli(), 36), b)
}

// resolveNetwork is whether a machine wants outbound network, from either
// place it can be asked. Resources.Network wins when both are set.
func resolveNetwork(cfg MachineConfig) *bool {
	if cfg.Resources.Network != nil {
		return cfg.Resources.Network
	}
	return cfg.Network
}

// egressPolicy is the engine's form of a NetworkPolicy.
func egressPolicy(p NetworkPolicy) (network bool, cidrs, hosts []string, err error) {
	// Loopback only keeps the network device a checkpointed machine expects,
	// unlike turning networking off.
	denyAll := []string{"127.0.0.0/8", "::1/128"}
	switch p.mode {
	case "allow-all":
		return true, []string{}, []string{}, nil
	case "deny-all":
		return true, denyAll, []string{}, nil
	case "allow-list":
		// An allow list that names nothing allows nothing; empty lists alone
		// would read as unrestricted egress.
		if len(p.cidrs) == 0 && len(p.hosts) == 0 {
			return true, denyAll, []string{}, nil
		}
		return true, append([]string{}, p.cidrs...), append([]string{}, p.hosts...), nil
	}
	return false, nil, nil, invalidConfig("network policy must be AllowAll(), DenyAll() or AllowList(hosts, cidrs)")
}

// resolveBatchNames: explicit Names win, otherwise Count children are named
// {prefix}-{n}.
func resolveBatchNames(opts BranchBatchOptions, defaultPrefix string) ([]string, error) {
	if len(opts.Names) > 0 {
		return opts.Names, nil
	}
	if opts.Count < 1 {
		return nil, invalidConfig("BranchBatch requires Count >= 1 or non-empty Names")
	}
	prefix := opts.NamePrefix
	if prefix == "" {
		prefix = defaultPrefix
	}
	names := make([]string, opts.Count)
	for i := range names {
		names[i] = fmt.Sprintf("%s-%d", prefix, i+1)
	}
	return names, nil
}

// looksLikeLocalCheckpoint: a restore source that names a file or directory on
// this host, rather than a cloud checkpoint id.
func looksLikeLocalCheckpoint(id string) bool {
	if _, err := os.Stat(id); err == nil {
		return true
	}
	return strings.HasSuffix(id, ".smolcheckpoint") || strings.HasPrefix(id, ".") ||
		strings.ContainsAny(id, `/\`)
}

// isCheckpointFile: a restore source that is a checkpoint file on this host,
// which a cloud restore uploads first.
func isCheckpointFile(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular()
}

func makeTransport(ctx context.Context, cfg MachineConfig, c ConnectOptions) (transport, error) {
	if selectsCloud(c) {
		return createCloud(ctx, cfg, c)
	}
	return createLocal(ctx, cfg, c)
}

func connectTransport(ctx context.Context, id string, c ConnectOptions) (transport, error) {
	if selectsCloud(c) {
		return connectCloud(ctx, id, c)
	}
	return connectLocal(ctx, id, c)
}

func listMachines(ctx context.Context, c ConnectOptions, opts ListOptions) ([]MachineSummary, error) {
	var all []MachineSummary
	var err error
	if selectsCloud(c) {
		all, err = listCloud(ctx, c)
	} else {
		all, err = listLocal()
	}
	if err != nil || len(opts.Labels) == 0 {
		return all, err
	}
	kept := all[:0]
	for _, m := range all {
		match := true
		for k, v := range opts.Labels {
			if m.Labels[k] != v {
				match = false
				break
			}
		}
		if match {
			kept = append(kept, m)
		}
	}
	return kept, nil
}

func restoreCheckpointTransport(ctx context.Context, checkpointID, name string, c *ConnectOptions, opts RestoreCheckpointOptions) (transport, error) {
	if checkpointID == "" || name == "" {
		return nil, invalidConfig("checkpoint id and restored machine name are required.")
	}
	var resolved ConnectOptions
	if c == nil {
		resolved.Target = TargetCloud
		if looksLikeLocalCheckpoint(checkpointID) {
			resolved.Target = TargetLocal
		}
	} else {
		resolved = *c
	}
	if !selectsCloud(resolved) {
		return restoreLocal(ctx, checkpointID, name, resolved, opts)
	}
	if opts.KeepIdentity {
		return nil, invalidConfig("KeepIdentity is local-only: a cloud restore always gives the new machine its own identity.")
	}
	return restoreCloud(ctx, checkpointID, name, resolved, opts)
}
