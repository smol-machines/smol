package smol

import (
	"time"
)

// Target selects the backend a Machine runs on.
type Target string

const (
	// TargetAuto picks the cloud when an API key is given (ConnectOptions.APIKey
	// or SMOL_CLOUD_TOKEN), else the local engine. A library embedding the SDK
	// should set a target explicitly.
	TargetAuto Target = ""
	// TargetLocal runs machines in-process on this host's hypervisor.
	TargetLocal Target = "local"
	// TargetCloud runs machines on Smol Cloud (smolfleet).
	TargetCloud Target = "cloud"
)

// ConnectOptions selects and configures the backend. The zero value is the
// local engine (unless SMOL_CLOUD_TOKEN is set).
type ConnectOptions struct {
	Target Target
	// APIKey is the cloud API key, smk_… (cloud only). Falls back to
	// SMOL_CLOUD_TOKEN, then the CLI session `smol auth login` stored.
	APIKey string
	// BaseURL is the cloud API base URL (cloud only). Falls back to
	// SMOL_CLOUD_URL, the CLI session, then https://api.smolmachines.com.
	BaseURL string
	// NoSignalHandling stops the SDK from stopping this process's local
	// machines on SIGINT/SIGTERM. Set it when the program owns shutdown; it
	// must then stop or delete its machines itself (the engine still reaps a
	// VM whose process died). Local only; branches inherit it.
	NoSignalHandling bool
	// EgressInterceptor binds a stopped local machine that requires
	// intercepted egress. Local only.
	EgressInterceptor *EgressInterceptor
}

// EgressInterceptor is a per-launch trusted host service that sees a local
// machine's outbound traffic. Keep the token out of logs and stored config.
type EgressInterceptor struct {
	// Address is a loopback socket address, for example 127.0.0.1:9000.
	Address string
	// Token is the 64 hexadecimal digits the interceptor service issued.
	Token string
}

// ResourceSpec is a machine's CPU, memory, disk and network allocation. Zero
// values mean "the default".
type ResourceSpec struct {
	// CPUs is the number of vCPUs.
	CPUs int
	// MemoryMB is guest memory in MB.
	MemoryMB int
	// Network enables unrestricted outbound access. Nil means off locally and
	// the control plane's default (open) on the cloud; false blocks egress on
	// both. To allow only some destinations, set AllowHosts or AllowCIDRs
	// instead: either one enables networking scoped to its list.
	Network *bool
	// AllowCIDRs scopes egress to these CIDR ranges.
	AllowCIDRs []string
	// AllowHosts scopes egress to these hostnames and their subdomains, for
	// example api.anthropic.com. Names are enforced at DNS inside the
	// machine, so a host served from another domain (a CDN) needs its own entry.
	AllowHosts []string
	// StorageGB is the storage disk size in GB (default 20).
	StorageGB int
	// OverlayGB is the overlay disk size in GB.
	OverlayGB int
	// GPU enables GPU acceleration (virtio-gpu/Venus). Local only.
	GPU bool
	// GPUVRAMMiB is the GPU shared-memory size in MiB. Local only.
	GPUVRAMMiB int
	// CUDA remotes guest CUDA Driver-API calls to the host NVIDIA GPU. Local only.
	CUDA bool
	// NetworkBackend is NetworkBackendTSI or NetworkBackendVirtioNet. Choose
	// virtio-net for a machine you will checkpoint and restore with a
	// different network policy. Local only.
	NetworkBackend NetworkBackend
}

// NetworkBackend picks how a local machine's network is provided.
type NetworkBackend string

const (
	// NetworkBackendTSI is outbound-only; the default for plain network access.
	NetworkBackendTSI NetworkBackend = "tsi"
	// NetworkBackendVirtioNet is the default when a port, allow list or
	// credential needs it.
	NetworkBackendVirtioNet NetworkBackend = "virtio-net"
)

// Bool returns a pointer to b, for the optional booleans in ResourceSpec.
func Bool(b bool) *bool { return &b }

// MountSpec mounts a host directory into a machine. Local only.
type MountSpec struct {
	// Source is an absolute host path, or an s3:// URL mounted in the guest.
	Source string
	// Target is an absolute path inside the machine.
	Target string
	// ReadOnly mounts it read-only. Default writable, like `smol -v`.
	ReadOnly bool
	// Staged runs from a guest-local working copy and copies changes back on
	// Sync, graceful stop or delete. Cannot be combined with ReadOnly.
	Staged bool
}

// PortSpec forwards a host port to a guest port.
type PortSpec struct {
	Host  int
	Guest int
}

// PublishedPort is one published port mapping. On the cloud the host port is
// the node's, and a port not placed on a node yet reports Host 0.
type PublishedPort struct {
	Host     int
	Guest    int
	Protocol string // "tcp" or "udp"
}

// PublishOptions selects a port for Machine.Publish. Host 0 picks a free host
// port; Protocol "" means TCP.
type PublishOptions struct {
	Guest    int
	Host     int
	Protocol string
}

// UnpublishOptions matches the ports Machine.Unpublish removes: by Guest port
// (every host port forwarding to it), by Host port, or both. Protocol "" means
// TCP.
type UnpublishOptions struct {
	Guest    int
	Host     int
	Protocol string
}

// MachineConfig configures a new machine. Every field is optional locally.
type MachineConfig struct {
	// Name of the machine; generated when empty.
	Name string
	// Image is the base OCI image. Required on the cloud; optional locally,
	// where Run(image, ...) per command is the alternative.
	Image string
	// Mounts are host directories to mount. Local only.
	Mounts []MountSpec
	// Ports to publish. On the cloud the guest port is published and the
	// control plane allocates the host port.
	Ports []PortSpec
	// Resources is the machine's allocation.
	Resources ResourceSpec
	// Network enables outbound access; an alias for Resources.Network, which
	// wins when both are set.
	Network *bool
	// Persistent keeps the machine record after this process exits. Local only.
	Persistent bool
	// Detach lets the machine outlive this process: it survives an exit, a
	// crash or a restart, and a later process picks it up with Connect(name).
	// Implies Persistent and is remembered by the machine. Local only.
	Detach bool
	// Labels are caller metadata stored with the machine and returned by
	// List, so a process can find its own machines after a restart. Local only.
	Labels map[string]string
	// AutoStop stops the machine after this long idle. Cloud only.
	AutoStop time.Duration
	// TTL deletes the machine after this long. Cloud only.
	TTL time.Duration
	// Branchable starts the machine as a live-RAM branch source, so Branch
	// can produce independent copy-on-write children from it.
	Branchable bool
	// Env is the environment for the image workload launched at create.
	Env map[string]string
	// Workdir is the image workload's working directory.
	Workdir string
	// User runs the workload as this user: a name from the image or a
	// numeric uid[:gid]. Local only.
	User string
	// EgressInterceptor binds intercepted egress for this local machine.
	EgressInterceptor *EgressInterceptor
}

// MachineSummary is one machine as List reports it.
type MachineSummary struct {
	Name string
	// ID is the cloud mach-… id, or the name locally.
	ID    string
	State string
	// Image is the base image, when the machine boots one.
	Image string
	// Labels given at create (local; empty on the cloud).
	Labels map[string]string
	// PID of the VM process while it runs (local; 0 otherwise).
	PID int
	// Persistent: the record outlives the process that made it.
	Persistent bool
	// Detached: the VM outlives the process that starts it. Always true on
	// the cloud.
	Detached bool
	// Branchable: it starts as a live branch source.
	Branchable bool
	CreatedAt  time.Time
}

// ListOptions filters List.
type ListOptions struct {
	// Labels keeps only machines carrying every one of these labels.
	Labels map[string]string
}

// NetworkPolicy is an outbound network policy that can replace a stopped
// machine's own. Build one with AllowAll, DenyAll or AllowList.
type NetworkPolicy struct {
	mode  string
	hosts []string
	cidrs []string
}

// AllowAll permits all outbound traffic.
func AllowAll() NetworkPolicy { return NetworkPolicy{mode: "allow-all"} }

// DenyAll blocks all outbound traffic.
func DenyAll() NetworkPolicy { return NetworkPolicy{mode: "deny-all"} }

// AllowList permits only these hosts and CIDR ranges. Host entries are exact
// names (api.github.com) or subdomain wildcards (*.github.com, which does not
// match github.com itself). An allow list naming nothing allows nothing.
func AllowList(hosts, cidrs []string) NetworkPolicy {
	return NetworkPolicy{mode: "allow-list", hosts: hosts, cidrs: cidrs}
}

// RestoreCheckpointOptions configures RestoreCheckpoint.
type RestoreCheckpointOptions struct {
	// NetworkPolicy, when set, is applied to the restored machine before it
	// boots. Locally the checkpoint must have been taken on the virtio-net
	// backend for an allow list or DenyAll. A cloud restore without one has
	// networking blocked.
	NetworkPolicy *NetworkPolicy
	// KeepIdentity restores the machine as itself going back in time rather
	// than as a clone, keeping its hostname and machine ID. Do not run two
	// machines from one checkpoint with it. Local only.
	KeepIdentity bool
}

// ExecOptions configures one command.
type ExecOptions struct {
	// Env adds environment variables.
	Env map[string]string
	// Workdir is the working directory inside the machine or container.
	Workdir string
	// Timeout kills the command after this long; it then exits 124, as under
	// timeout(1). To abandon a command and get an error instead, end ctx.
	Timeout time.Duration
	// User runs the command as this user: a name from the image or a numeric
	// uid[:gid]. Locally this needs an image machine. On the cloud the SDK
	// first confirms the control plane honours per-command users and returns
	// ErrNotSupported, before running anything, if it does not.
	User string
	// Output picks which encodings the cloud returns: "text", "b64" or the
	// default both. Ignored locally.
	Output string
}

// ExecResult is a finished command.
type ExecResult struct {
	ExitCode int
	// Stdout is the captured stdout. Locally it is byte-exact; on the cloud it
	// is capped at about 1 MiB of UTF-8 text (see StdoutTruncated), so prefer
	// StdoutBytes for binary or large output.
	Stdout string
	Stderr string
	// StdoutTruncated is set when the cloud capped Stdout. Always false locally.
	StdoutTruncated bool
	StderrTruncated bool
	// StdoutBytes is byte-exact, untruncated stdout.
	StdoutBytes []byte
	StderrBytes []byte
}

// Success reports whether the command exited 0.
func (r *ExecResult) Success() bool { return r.ExitCode == 0 }

// Output is stdout followed by stderr.
func (r *ExecResult) Output() string { return r.Stdout + r.Stderr }

// AssertSuccess returns an *ExecutionError when the command exited non-zero.
func (r *ExecResult) AssertSuccess() error {
	if r.ExitCode == 0 {
		return nil
	}
	return &ExecutionError{ExitCode: r.ExitCode, Stdout: r.Stdout, Stderr: r.Stderr}
}

// EventKind is the kind of an ExecEvent.
type EventKind string

const (
	EventStdout EventKind = "stdout"
	EventStderr EventKind = "stderr"
	EventExit   EventKind = "exit"
	EventError  EventKind = "error"
)

// ExecEvent is one event of a streaming command.
type ExecEvent struct {
	Kind EventKind
	// Data is the output chunk for stdout and stderr events.
	Data []byte
	// ExitCode is set on the exit event.
	ExitCode int
	// Message is set on an error event.
	Message string
}

// ImageInfo is a cached OCI image.
type ImageInfo struct {
	Reference    string
	Digest       string
	Size         int64
	Architecture string
	OS           string
}

// WaitReadyOptions bounds WaitUntilReady. Zero values mean 120s and 1s.
type WaitReadyOptions struct {
	Timeout  time.Duration
	Interval time.Duration
}

// PortEndpoint reaches a published guest port.
type PortEndpoint struct {
	// HTTPURL is for HTTP requests.
	HTTPURL string
	// WSURL is for WebSocket upgrades.
	WSURL string
	// Headers must be sent with each request (the cloud bearer token).
	Headers map[string]string
}

// BranchOptions configures one live branch.
type BranchOptions struct {
	// Ports pins inbound port forwards for the child.
	Ports []PortSpec
	// Branchable makes the child a branch source itself. It pays one eager
	// guest-memory copy when the child boots.
	Branchable bool
}

// BranchBatchOptions branches one source into many children. Give Count
// (children named {NamePrefix}-{n}) or Names.
type BranchBatchOptions struct {
	// Count is the number of children (1..64). Ignored when Names is set.
	Count int
	// Names are explicit child names.
	Names []string
	// NamePrefix names counted children (default: the source name on the
	// cloud, else "branch").
	NamePrefix string
	// Ports applies to every child. Empty gives each child fresh host ports.
	Ports []PortSpec
}

// ResizeOptions are totals to grow a running machine to. Zero fields stay
// unchanged. RAM and disks only grow; CPUs can also shrink on Linux x86_64.
type ResizeOptions struct {
	CPUs      int
	MemoryMB  int
	StorageGB int
	OverlayGB int
}

// MachineResources is a machine's allocation after a resize.
type MachineResources struct {
	CPUs     int
	MemoryMB int
	// StorageGB is 0 while the disk has its default size.
	StorageGB int
	OverlayGB int
}

// CheckpointOptions configures a local capture.
type CheckpointOptions struct {
	// Store is a local content-addressed checkpoint store directory; periodic
	// captures into it reuse unchanged chunks.
	Store string
}

// PortableCheckpointInfo is a captured live checkpoint.
type PortableCheckpointInfo struct {
	ID          string
	MachineID   string
	Status      string
	SizeBytes   int64
	ReusedBytes int64
	Arch        string
	CreatedAt   time.Time
	DownloadURL string
	// Path is the local artifact path when capture wrote to disk.
	Path string
	// SourcePause is how long the source paused at the consistency boundary.
	SourcePause time.Duration
	// Elapsed is the local end-to-end capture and compression time.
	Elapsed time.Duration
}

// AssignOptions leases one provisioned clone of a branchable golden for an RL
// episode. Cloud only.
type AssignOptions struct {
	// LeaseID is the caller-chosen idempotency key; a retried Assign returns
	// the same episode.
	LeaseID string
	// Task is a JSON-serializable payload staged at /run/smol-task.json.
	Task any
	// Files to stage into the clone, by guest path.
	Files map[string][]byte
	// Secrets are staged at /run/smol-secrets.json and never persisted.
	Secrets map[string]string
	// Ports pins inbound port forwards.
	Ports []PortSpec
	// TTL reclaims the clone past it.
	TTL time.Duration
	// Heartbeat is the expected heartbeat cadence; a missed one reclaims.
	Heartbeat time.Duration
}

// MachineUsageReport is a machine's metered usage and cost. Cloud only.
type MachineUsageReport struct {
	MachineID string               `json:"machineId"`
	From      string               `json:"from"`
	To        string               `json:"to"`
	Usage     MachineUsageTotals   `json:"usage"`
	Cost      MachineCostBreakdown `json:"cost"`
}

// MachineUsageTotals are usage totals over a report window.
type MachineUsageTotals struct {
	TotalUptimeSeconds float64 `json:"totalUptimeSeconds"`
	CPUHours           float64 `json:"cpuHours"`
	MemoryGBHours      float64 `json:"memoryGbHours"`
	DiskGBHours        float64 `json:"diskGbHours"`
	EgressGB           float64 `json:"egressGb"`
}

// MachineCostBreakdown is a machine's cost in micro-dollars (1e-6 USD).
type MachineCostBreakdown struct {
	CPUMicros       int64 `json:"cpuMicros"`
	MemoryMicros    int64 `json:"memoryMicros"`
	DiskMicros      int64 `json:"diskMicros"`
	EgressMicros    int64 `json:"egressMicros"`
	BaseMicros      int64 `json:"baseMicros"`
	TotalMicros     int64 `json:"totalMicros"`
	AmountDueMicros int64 `json:"amountDueMicros"`
}

// ShareLink is an anonymous link to a machine's published app. Anyone with
// the URL reaches the app, so treat it as a credential. Cloud only.
type ShareLink struct {
	Token string `json:"token"`
	// URL is empty when the tenant has no apps domain or the machine name is
	// not DNS-safe; attach ?t=<token> yourself then.
	URL string `json:"url"`
}

// CloudCheckpointInfo is a checkpoint uploaded to the cloud.
type CloudCheckpointInfo struct {
	ID        string `json:"id"`
	Status    string `json:"status"`
	SizeBytes int64  `json:"sizeBytes"`
	Arch      string `json:"arch"`
	CreatedAt string `json:"createdAt"`
}

// LocalAvailability says whether this host can run local machines.
type LocalAvailability struct {
	Available bool
	// Code is the error code a local Create would fail with, when unavailable.
	Code string
	// Reason explains Code.
	Reason string
}
