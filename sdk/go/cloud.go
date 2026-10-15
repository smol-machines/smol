package smol

import (
	"bufio"
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"iter"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"
)

const defaultCloudURL = "https://api.smolmachines.com"

const (
	// cloudTimeout bounds an ordinary cloud call.
	cloudTimeout = 30 * time.Second
	// cloudStartTimeout: starting can include a cold image pull, so this
	// long-running operation gets the same window as checkpoint restore.
	cloudStartTimeout = 30 * time.Minute
	// noPortReadyProbeGrace: before probing the guest agent of a machine with
	// no published port, give `ready` time to flip first, so the probe stays a
	// last resort.
	noPortReadyProbeGrace = 2 * time.Second
	// cloudExecTimeoutHeadroom is added to a command's own timeout when sizing
	// the request timeout, so the client never gives up before the server.
	cloudExecTimeoutHeadroom = 30 * time.Second
)

const noKeyHint = "pass ConnectOptions.APIKey, set SMOL_CLOUD_TOKEN, or run `smol auth login` to create a CLI session the SDK reuses"

// cloudConn is the control plane and key a cloud call uses.
type cloudConn struct {
	baseURL string
	apiKey  string
}

// errorSentence is the sentence a person should read from a failed response's
// body. The control plane answers errors as RFC 9457 problem+json, whose
// `detail` is the sentence; older builds sent {"error": …} or plain text. A
// JSON object yields its first string among detail, error, message and title.
func errorSentence(body string) string {
	trimmed := strings.TrimSpace(body)
	var fields map[string]any
	if json.Unmarshal([]byte(trimmed), &fields) == nil && fields != nil {
		for _, key := range []string{"detail", "error", "message", "title"} {
			if s, ok := fields[key].(string); ok {
				return strings.TrimSpace(s)
			}
		}
	}
	return trimmed
}

// encodePath percent-encodes each path segment but keeps the `/` separators:
// the files route is a wildcard, so slashes are meaningful while spaces, ?, #
// and % in a filename must be escaped.
func encodePath(p string) string {
	parts := strings.Split(p, "/")
	for i, s := range parts {
		parts[i] = encodeURIComponent(s)
	}
	return strings.Join(parts, "/")
}

// encodeURIComponent matches JavaScript's encodeURIComponent.
func encodeURIComponent(s string) string {
	const keep = "-_.!~*'()"
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		c := s[i]
		if c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c >= '0' && c <= '9' || strings.IndexByte(keep, c) >= 0 {
			b.WriteByte(c)
		} else {
			fmt.Fprintf(&b, "%%%02X", c)
		}
	}
	return b.String()
}

type fetchOpts struct {
	json    any
	body    []byte
	bytes   bool
	timeout time.Duration
}

// httpError is the *Error for a non-2xx response. The server's correlation id
// goes in the message: callers see the error, not the response headers.
func httpError(method, path string, res *http.Response) error {
	b, _ := io.ReadAll(res.Body)
	text := errorSentence(string(b))
	code := CodeSmolvm
	switch res.StatusCode {
	case http.StatusNotFound:
		code = CodeNotFound
	case http.StatusUnauthorized:
		code = CodeUnauthorized
	}
	msg := fmt.Sprintf("cloud %s %s → %d", method, path, res.StatusCode)
	if text != "" {
		msg += ": " + text
	}
	if rid := res.Header.Get("x-request-id"); rid != "" {
		msg += " [request id: " + rid + "]"
	}
	return &Error{Code: code, Message: msg, Status: res.StatusCode}
}

// cloudDo sends one request and returns the response body: raw bytes when
// opts.bytes is set, otherwise the JSON body (nil for 204 or a non-JSON body).
func cloudDo(ctx context.Context, c cloudConn, method, path string, opts fetchOpts) ([]byte, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	timeout := opts.timeout
	if timeout <= 0 {
		timeout = cloudTimeout
	}
	var body io.Reader
	contentType := ""
	if opts.json != nil {
		b, err := json.Marshal(opts.json)
		if err != nil {
			return nil, invalidConfig("encode request: " + err.Error())
		}
		body = bytes.NewReader(b)
		contentType = "application/json"
	} else if opts.body != nil {
		body = bytes.NewReader(opts.body)
		contentType = "application/octet-stream"
	}
	reqCtx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	req, err := http.NewRequestWithContext(reqCtx, method, c.baseURL+path, body)
	if err != nil {
		return nil, invalidConfig("build request: " + err.Error())
	}
	req.Header.Set("authorization", "Bearer "+c.apiKey)
	if contentType != "" {
		req.Header.Set("content-type", contentType)
	}
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		if ctxErr := ctx.Err(); ctxErr != nil {
			return nil, ctxErr
		}
		if errors.Is(err, context.DeadlineExceeded) {
			return nil, &Error{Code: CodeTimeout, Message: fmt.Sprintf("cloud %s %s timed out after %s", method, path, timeout)}
		}
		return nil, &Error{Code: CodeConnection, Message: "cloud request failed: " + err.Error()}
	}
	defer res.Body.Close()
	if res.StatusCode < 200 || res.StatusCode > 299 {
		return nil, httpError(method, path, res)
	}
	data, err := io.ReadAll(res.Body)
	if err != nil {
		if ctxErr := ctx.Err(); ctxErr != nil {
			return nil, ctxErr
		}
		return nil, &Error{Code: CodeConnection, Message: "cloud request failed: " + err.Error()}
	}
	if opts.bytes {
		return data, nil
	}
	if res.StatusCode == http.StatusNoContent || !strings.Contains(res.Header.Get("content-type"), "application/json") {
		return nil, nil
	}
	return data, nil
}

// cloudJSON sends one request and decodes its JSON body into out (when both
// exist).
func cloudJSON(ctx context.Context, c cloudConn, method, path string, opts fetchOpts, out any) error {
	data, err := cloudDo(ctx, c, method, path, opts)
	if err != nil || out == nil || data == nil {
		return err
	}
	if err := json.Unmarshal(data, out); err != nil {
		return &Error{Code: CodeSmolvm, Message: fmt.Sprintf("decode cloud %s %s: %s", method, path, err)}
	}
	return nil
}

// machineInfo is the control plane's machine record (MachineInfo).
type machineInfo struct {
	ID         string  `json:"id"`
	Name       *string `json:"name"`
	State      string  `json:"state"`
	Ready      *bool   `json:"ready"`
	ReadyAt    *string `json:"readyAt"`
	URL        *string `json:"url"`
	Ephemeral  bool    `json:"ephemeral"`
	Branchable *bool   `json:"branchable"`
	Forkable   *bool   `json:"forkable"`
	CreatedAt  string  `json:"createdAt"`
	Ports      []struct {
		Port     int  `json:"port"`
		HostPort *int `json:"hostPort"`
	} `json:"ports"`
	Source *struct {
		Type      string `json:"type"`
		Reference string `json:"reference"`
	} `json:"source"`
}

func (m *machineInfo) nameOr(fallback string) string {
	if m.Name != nil && *m.Name != "" {
		return *m.Name
	}
	return fallback
}

// agentReachable is a best-effort readiness probe: a trivial in-guest exec
// that only succeeds once the agent is up. Any failure means not ready.
func agentReachable(ctx context.Context, c cloudConn, id string) bool {
	_, err := cloudDo(ctx, c, "POST", "/v1/machines/"+id+"/exec", fetchOpts{
		json:    map[string]any{"command": []string{"sh", "-c", "true"}},
		timeout: 15 * time.Second,
	})
	return err == nil
}

// waitForReady polls a cloud machine until it is ready to do work, and fails
// on an error or terminal state or the timeout. Auth and not-found errors are
// fatal; other errors are treated as transient while it boots.
//
// Readiness is the machine's `ready` flag, true only once the guest agent is
// reachable and any published port accepts. A state of `started` alone is not
// usable. Older control planes omit `ready`; there the coarse
// started/running state is the gate, so this never hangs against them.
func waitForReady(ctx context.Context, c cloudConn, id string, opts WaitReadyOptions) error {
	opts = opts.withDefaults()
	start := time.Now()
	deadline := start.Add(opts.Timeout)
	for {
		var m *machineInfo
		var got machineInfo
		if err := cloudJSON(ctx, c, "GET", "/v1/machines/"+id, fetchOpts{}, &got); err != nil {
			var se *Error
			if ctx.Err() != nil {
				return ctx.Err()
			}
			if errors.As(err, &se) && (se.Code == CodeUnauthorized || se.Code == CodeNotFound) {
				return err
			}
		} else {
			m = &got
		}
		state := ""
		if m != nil {
			state = m.State
		}
		if m != nil && m.Ready != nil && *m.Ready {
			return nil
		}
		if state == "error" {
			return &Error{Code: CodeSmolvm, Message: fmt.Sprintf("machine %s entered error state while starting", id)}
		}
		// A definitively terminal, non-ready state won't become ready by waiting.
		if state == "stopped" || state == "deleted" {
			return &Error{Code: CodeSmolvm, Message: fmt.Sprintf("machine %s entered %s before becoming ready", id, state)}
		}
		running := state == "started" || state == "running"
		// Back-compat: `ready` absent entirely means an old server; gate on state.
		if m != nil && m.Ready == nil && running {
			return nil
		}
		// A machine with no published port never flips `ready` on control
		// planes that gate readiness on a port accepting a connection. After a
		// grace, confirm the guest agent directly and treat that as ready.
		if running && (m == nil || len(m.Ports) == 0) && time.Since(start) >= noPortReadyProbeGrace && agentReachable(ctx, c, id) {
			return nil
		}
		if !time.Now().Before(deadline) {
			if state == "" {
				state = "unknown"
			}
			return &Error{Code: CodeTimeout, Message: fmt.Sprintf("machine %s not ready after %s (state=%s)", id, opts.Timeout, state)}
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(opts.Interval):
		}
	}
}

// cloudTransport drives a machine on Smol Cloud over the /v1 REST API.
type cloudTransport struct {
	conn cloudConn
	n    string
	id   string

	mu sync.Mutex
	// verifiedUsers are users this control plane has been proven to honour.
	verifiedUsers map[string]bool
}

func newCloud(c cloudConn, name, id string) *cloudTransport {
	return &cloudTransport{conn: c, n: name, id: id, verifiedUsers: map[string]bool{}}
}

func (t *cloudTransport) path(suffix string) string { return "/v1/machines/" + t.id + suffix }

// commandRequest is MachineCommandRequest plus the per-command user the
// control plane added after the generated schema.
type commandRequest struct {
	Command        []string          `json:"command"`
	Env            map[string]string `json:"env"`
	Cwd            *string           `json:"cwd"`
	TimeoutSeconds *int64            `json:"timeoutSeconds"`
	User           *string           `json:"user,omitempty"`
}

func timeoutSeconds(d time.Duration) int64 {
	return int64((d + time.Second - 1) / time.Second)
}

func newCommandRequest(command []string, opts *ExecOptions) commandRequest {
	r := commandRequest{Command: command, Env: map[string]string{}}
	if opts == nil {
		return r
	}
	if opts.Env != nil {
		r.Env = opts.Env
	}
	if opts.Workdir != "" {
		r.Cwd = &opts.Workdir
	}
	if opts.Timeout > 0 {
		s := timeoutSeconds(opts.Timeout)
		r.TimeoutSeconds = &s
	}
	if opts.User != "" {
		r.User = &opts.User
	}
	return r
}

// assertCloudUserEcho: the control plane echoes the user it ran a command as;
// one that predates the field echoes nothing because it silently dropped it.
func assertCloudUserEcho(raw []byte, requested string) error {
	var echo struct {
		User *string `json:"user"`
	}
	_ = json.Unmarshal(raw, &echo)
	if echo.User == nil || *echo.User != requested {
		return notSupported(fmt.Sprintf("the control plane did not honour user %q: it predates per-command "+
			"users and would run the command as the image's default account", requested))
	}
	return nil
}

// ensureUser proves the control plane honours a per-command user before a
// real command runs as it: an older control plane drops the field and would
// run the command as the image's default account without any sign. A
// harmless `true` whose response echoes the user settles it, once per user.
func (t *cloudTransport) ensureUser(ctx context.Context, user string) error {
	t.mu.Lock()
	ok := t.verifiedUsers[user]
	t.mu.Unlock()
	if ok {
		return nil
	}
	timeout := int64(30)
	probe := commandRequest{Command: []string{"true"}, Env: map[string]string{}, TimeoutSeconds: &timeout, User: &user}
	raw, err := cloudDo(ctx, t.conn, "POST", t.path("/exec"), fetchOpts{json: probe})
	if err != nil {
		return err
	}
	if err := assertCloudUserEcho(raw, user); err != nil {
		return err
	}
	t.mu.Lock()
	t.verifiedUsers[user] = true
	t.mu.Unlock()
	return nil
}

func (t *cloudTransport) name() string      { return t.n }
func (t *cloudTransport) machineID() string { return t.id }

func (t *cloudTransport) info(ctx context.Context) (*machineInfo, error) {
	var m machineInfo
	if err := cloudJSON(ctx, t.conn, "GET", t.path(""), fetchOpts{}, &m); err != nil {
		return nil, err
	}
	return &m, nil
}

func (t *cloudTransport) state(ctx context.Context) (string, error) {
	m, err := t.info(ctx)
	if err != nil {
		return "", err
	}
	if m.State == "" {
		return "unknown", nil
	}
	return m.State, nil
}

func (t *cloudTransport) ready(ctx context.Context) (bool, error) {
	m, err := t.info(ctx)
	if err != nil {
		return false, err
	}
	return m.Ready != nil && *m.Ready, nil
}

func (t *cloudTransport) readyAt(ctx context.Context) (time.Time, error) {
	m, err := t.info(ctx)
	if err != nil || m.ReadyAt == nil || *m.ReadyAt == "" {
		return time.Time{}, err
	}
	at, err := time.Parse(time.RFC3339Nano, *m.ReadyAt)
	if err != nil {
		return time.Time{}, &Error{Code: CodeSmolvm, Message: "parse readyAt: " + err.Error()}
	}
	return at, nil
}

func (t *cloudTransport) waitUntilReady(ctx context.Context, opts WaitReadyOptions) error {
	return waitForReady(ctx, t.conn, t.id, opts)
}

// endpoint reaches a published guest port through the control plane's
// authenticated connect bridge. A bare "/" or "" stays `connect/<port>`: the
// control routes no `connect/<port>/`.
func (t *cloudTransport) endpoint(port int, path string) (PortEndpoint, error) {
	sub := strings.TrimLeft(path, "/")
	rel := fmt.Sprintf("/v1/machines/%s/connect/%d", t.id, port)
	if sub != "" {
		rel += "/" + sub
	}
	ws := t.conn.baseURL
	if strings.HasPrefix(ws, "http") {
		ws = "ws" + strings.TrimPrefix(ws, "http")
	}
	return PortEndpoint{
		HTTPURL: t.conn.baseURL + rel,
		WSURL:   ws + rel,
		Headers: map[string]string{"authorization": "Bearer " + t.conn.apiKey},
	}, nil
}

func (t *cloudTransport) url(ctx context.Context) (string, error) {
	m, err := t.info(ctx)
	if err != nil || m.URL == nil {
		return "", err
	}
	return *m.URL, nil
}

// decodeExecBytes prefers the byte-exact, untruncated base64 field and falls
// back to the text when a control plane predates it or it is malformed. An
// empty base64 field beside non-empty text means that family was dropped
// (?output=text), not that the output was empty.
func decodeExecBytes(b64 *string, text string) []byte {
	if b64 != nil && (*b64 != "" || text == "") {
		if b, err := base64.StdEncoding.DecodeString(*b64); err == nil {
			return b
		}
	}
	return []byte(text)
}

func (t *cloudTransport) exec(ctx context.Context, command []string, opts *ExecOptions) (*ExecResult, error) {
	user := ""
	if opts != nil {
		user = opts.User
	}
	if user != "" {
		if err := t.ensureUser(ctx, user); err != nil {
			return nil, err
		}
	}
	// A command may run far longer than the default request timeout, so size
	// it off the command's own timeout plus headroom, never below the default.
	timeout := cloudTimeout
	query := ""
	if opts != nil {
		if opts.Timeout > 0 {
			timeout = max(cloudTimeout, time.Duration(timeoutSeconds(opts.Timeout))*time.Second+cloudExecTimeoutHeadroom)
		}
		// ?output= trims the response to one encoding; older control planes
		// ignore it and send both.
		if opts.Output != "" {
			query = "?output=" + url.QueryEscape(opts.Output)
		}
	}
	raw, err := cloudDo(ctx, t.conn, "POST", t.path("/exec"+query), fetchOpts{json: newCommandRequest(command, opts), timeout: timeout})
	if err != nil {
		return nil, err
	}
	// Defense in depth: the probe proved support; confirm this run too.
	if user != "" {
		if err := assertCloudUserEcho(raw, user); err != nil {
			return nil, err
		}
	}
	var r struct {
		ExitCode        *int    `json:"exitCode"`
		Stdout          *string `json:"stdout"`
		Stderr          *string `json:"stderr"`
		StdoutTruncated bool    `json:"stdoutTruncated"`
		StderrTruncated bool    `json:"stderrTruncated"`
		StdoutB64       *string `json:"stdoutB64"`
		StderrB64       *string `json:"stderrB64"`
	}
	if raw != nil {
		if err := json.Unmarshal(raw, &r); err != nil {
			return nil, &Error{Code: CodeSmolvm, Message: "decode exec response: " + err.Error()}
		}
	}
	res := &ExecResult{StdoutTruncated: r.StdoutTruncated, StderrTruncated: r.StderrTruncated}
	if r.ExitCode != nil {
		res.ExitCode = *r.ExitCode
	}
	if r.Stdout != nil {
		res.Stdout = *r.Stdout
	}
	if r.Stderr != nil {
		res.Stderr = *r.Stderr
	}
	res.StdoutBytes = decodeExecBytes(r.StdoutB64, res.Stdout)
	res.StderrBytes = decodeExecBytes(r.StderrB64, res.Stderr)
	return res, nil
}

func (t *cloudTransport) run(context.Context, string, []string, *ExecOptions) (*ExecResult, error) {
	return nil, notSupported("Run(image, ...) is not available on the cloud target; create a machine from an image " +
		"with Create(ctx, MachineConfig{Image: ...}, &ConnectOptions{Target: TargetCloud}) and use Exec instead.")
}

// execStream reads the server's SSE stream: each event is `event: <kind>`
// plus one or more `data:` lines (joined with "\n"), ended by a blank line.
// The exit event's data is JSON {exitCode}. Stopping early closes the stream.
func (t *cloudTransport) execStream(ctx context.Context, command []string, opts *ExecOptions) iter.Seq2[ExecEvent, error] {
	return func(yield func(ExecEvent, error) bool) {
		if err := ctx.Err(); err != nil {
			yield(ExecEvent{}, err)
			return
		}
		// A stream's response cannot echo the user, so the probe is the only check.
		if opts != nil && opts.User != "" {
			if err := t.ensureUser(ctx, opts.User); err != nil {
				yield(ExecEvent{}, err)
				return
			}
		}
		body, err := json.Marshal(newCommandRequest(command, opts))
		if err != nil {
			yield(ExecEvent{}, invalidConfig("encode request: "+err.Error()))
			return
		}
		path := t.path("/exec/stream")
		streamCtx, cancel := context.WithCancel(ctx)
		defer cancel()
		req, err := http.NewRequestWithContext(streamCtx, "POST", t.conn.baseURL+path, bytes.NewReader(body))
		if err != nil {
			yield(ExecEvent{}, invalidConfig("build request: "+err.Error()))
			return
		}
		req.Header.Set("authorization", "Bearer "+t.conn.apiKey)
		req.Header.Set("content-type", "application/json")
		req.Header.Set("accept", "text/event-stream")
		res, err := http.DefaultClient.Do(req)
		if err != nil {
			if ctxErr := ctx.Err(); ctxErr != nil {
				yield(ExecEvent{}, ctxErr)
			} else {
				yield(ExecEvent{}, &Error{Code: CodeConnection, Message: "cloud exec/stream failed: " + err.Error()})
			}
			return
		}
		defer res.Body.Close()
		if res.StatusCode < 200 || res.StatusCode > 299 {
			yield(ExecEvent{}, httpError("POST", path, res))
			return
		}
		event := ""
		var data []string
		flush := func() (ExecEvent, bool) {
			kind, joined := event, strings.Join(data, "\n")
			event, data = "", nil
			switch kind {
			case "stdout", "stderr":
				return ExecEvent{Kind: EventKind(kind), Data: []byte(joined)}, true
			case "error":
				return ExecEvent{Kind: EventError, Message: joined}, true
			case "exit":
				var e struct {
					ExitCode int `json:"exitCode"`
				}
				_ = json.Unmarshal([]byte(joined), &e)
				return ExecEvent{Kind: EventExit, ExitCode: e.ExitCode}, true
			}
			return ExecEvent{}, false
		}
		reader := bufio.NewReader(res.Body)
		for {
			line, err := reader.ReadString('\n')
			if err != nil && !errors.Is(err, io.EOF) {
				if ctxErr := ctx.Err(); ctxErr != nil {
					yield(ExecEvent{}, ctxErr)
				} else {
					yield(ExecEvent{}, &Error{Code: CodeConnection, Message: "cloud exec/stream failed: " + err.Error()})
				}
				return
			}
			eof := err != nil
			if !eof || line != "" {
				line = strings.TrimSuffix(strings.TrimSuffix(line, "\n"), "\r")
				switch {
				case line == "" && !eof:
					if ev, ok := flush(); ok && !yield(ev, nil) {
						return
					}
				case strings.HasPrefix(line, "event:"):
					event = strings.TrimSpace(line[len("event:"):])
				case strings.HasPrefix(line, "data:"):
					data = append(data, strings.TrimPrefix(line[len("data:"):], " "))
				}
			}
			if eof {
				if ev, ok := flush(); ok {
					yield(ev, nil)
				}
				return
			}
		}
	}
}

func (t *cloudTransport) readFile(ctx context.Context, path string) ([]byte, error) {
	return cloudDo(ctx, t.conn, "GET", t.path("/files/"+encodePath(path)), fetchOpts{bytes: true})
}

func (t *cloudTransport) writeFile(ctx context.Context, path string, data []byte, mode os.FileMode) error {
	if data == nil {
		data = []byte{}
	}
	if _, err := cloudDo(ctx, t.conn, "PUT", t.path("/files/"+encodePath(path)), fetchOpts{body: data}); err != nil {
		return err
	}
	// The files PUT carries no mode, so apply a requested one with chmod.
	if mode != 0 {
		_, err := t.exec(ctx, []string{"chmod", strconv.FormatUint(uint64(mode.Perm()), 8), path}, nil)
		return err
	}
	return nil
}

func (t *cloudTransport) pullImage(context.Context, string) (*ImageInfo, error) {
	return nil, notSupported("PullImage is not available on the cloud target.")
}

func (t *cloudTransport) listImages(context.Context) ([]ImageInfo, error) {
	return nil, notSupported("ListImages is not available on the cloud target.")
}

func (t *cloudTransport) sync(context.Context) error {
	return notSupported("Sync is local-only because cloud machines cannot carry host-directory mounts.")
}

func (t *cloudTransport) setNetworkPolicy(context.Context, NetworkPolicy) error {
	return notSupported("SetNetworkPolicy is local-only.")
}

func (t *cloudTransport) stop(ctx context.Context) error {
	_, err := cloudDo(ctx, t.conn, "POST", t.path("/stop"), fetchOpts{})
	return err
}

func (t *cloudTransport) pause(ctx context.Context) error {
	_, err := cloudDo(ctx, t.conn, "POST", t.path("/pause"), fetchOpts{timeout: cloudStartTimeout})
	return err
}

func (t *cloudTransport) resume(ctx context.Context) error {
	if _, err := cloudDo(ctx, t.conn, "POST", t.path("/resume"), fetchOpts{timeout: cloudStartTimeout}); err != nil {
		return err
	}
	return waitForReady(ctx, t.conn, t.id, WaitReadyOptions{})
}

func (t *cloudTransport) start(ctx context.Context, interceptor *EgressInterceptor) error {
	if interceptor != nil {
		return notSupported("EgressInterceptor is local-only.")
	}
	if _, err := cloudDo(ctx, t.conn, "POST", t.path("/start"), fetchOpts{timeout: cloudStartTimeout}); err != nil {
		return err
	}
	return waitForReady(ctx, t.conn, t.id, WaitReadyOptions{})
}

func (t *cloudTransport) delete(ctx context.Context) error {
	_, err := cloudDo(ctx, t.conn, "DELETE", t.path(""), fetchOpts{})
	return err
}

// deleteWithUsage: the control plane takes a final usage sample before the
// teardown, so the report is settled.
func (t *cloudTransport) deleteWithUsage(ctx context.Context) (*MachineUsageReport, error) {
	var r MachineUsageReport
	if err := cloudJSON(ctx, t.conn, "DELETE", t.path("?includeUsage=true"), fetchOpts{}, &r); err != nil {
		return nil, err
	}
	return &r, nil
}

func (t *cloudTransport) usage(ctx context.Context) (*MachineUsageReport, error) {
	var r MachineUsageReport
	if err := cloudJSON(ctx, t.conn, "GET", t.path("/usage"), fetchOpts{}, &r); err != nil {
		return nil, err
	}
	return &r, nil
}

func (t *cloudTransport) share(ctx context.Context) (*ShareLink, error) {
	var l ShareLink
	if err := cloudJSON(ctx, t.conn, "POST", t.path("/share"), fetchOpts{}, &l); err != nil {
		return nil, err
	}
	return &l, nil
}

func (t *cloudTransport) unshare(ctx context.Context) error {
	_, err := cloudDo(ctx, t.conn, "DELETE", t.path("/share"), fetchOpts{})
	return err
}

func (t *cloudTransport) ports(ctx context.Context) ([]PublishedPort, error) {
	m, err := t.info(ctx)
	if err != nil {
		return nil, err
	}
	out := make([]PublishedPort, 0, len(m.Ports))
	for _, p := range m.Ports {
		host := 0
		if p.HostPort != nil {
			host = *p.HostPort
		}
		out = append(out, PublishedPort{Host: host, Guest: p.Port, Protocol: "tcp"})
	}
	return out, nil
}

// setPorts publishes TCP guest ports; the machine's node picks each one's host
// port, and clients reach them through Endpoint, tunnels and the proxy.
func (t *cloudTransport) setPorts(ctx context.Context, ports []PublishedPort) error {
	seen := map[int]bool{}
	wire := []map[string]int{}
	for _, p := range ports {
		if protocolOrTCP(p.Protocol) == "udp" {
			return notSupported("UDP ports are local-only; the cloud publishes TCP ports.")
		}
		if !seen[p.Guest] {
			seen[p.Guest] = true
			wire = append(wire, map[string]int{"port": p.Guest})
		}
	}
	_, err := cloudDo(ctx, t.conn, "PUT", t.path("/ports"), fetchOpts{json: map[string]any{"ports": wire}})
	return err
}

func (t *cloudTransport) portLinks(ctx context.Context) (map[int]string, error) {
	var out struct {
		URLs map[string]string `json:"urls"`
	}
	if err := cloudJSON(ctx, t.conn, "POST", t.path("/port-links"), fetchOpts{}, &out); err != nil {
		return nil, err
	}
	links := make(map[int]string, len(out.URLs))
	for port, url := range out.URLs {
		n, err := strconv.Atoi(port)
		if err != nil {
			return nil, fmt.Errorf("port link for %q: %w", port, err)
		}
		links[n] = url
	}
	return links, nil
}

func (t *cloudTransport) revokePortLinks(ctx context.Context) error {
	_, err := cloudDo(ctx, t.conn, "DELETE", t.path("/port-links"), fetchOpts{})
	return err
}

// checkpointWire is the control plane's PortableCheckpointInfo.
type checkpointWire struct {
	ID          string  `json:"id"`
	MachineID   string  `json:"machineId"`
	Status      string  `json:"status"`
	SizeBytes   int64   `json:"sizeBytes"`
	ReusedBytes int64   `json:"reusedBytes"`
	Arch        string  `json:"arch"`
	CreatedAt   string  `json:"createdAt"`
	DownloadURL string  `json:"downloadUrl"`
	Path        string  `json:"path"`
	SourcePause float64 `json:"sourcePauseMs"`
	Elapsed     float64 `json:"elapsedMs"`
}

func (w checkpointWire) info() PortableCheckpointInfo {
	created, _ := time.Parse(time.RFC3339Nano, w.CreatedAt)
	return PortableCheckpointInfo{
		ID: w.ID, MachineID: w.MachineID, Status: w.Status, SizeBytes: w.SizeBytes,
		ReusedBytes: w.ReusedBytes, Arch: w.Arch, CreatedAt: created, DownloadURL: w.DownloadURL, Path: w.Path,
		SourcePause: time.Duration(w.SourcePause * float64(time.Millisecond)),
		Elapsed:     time.Duration(w.Elapsed * float64(time.Millisecond)),
	}
}

func (t *cloudTransport) checkpoint(ctx context.Context, output string, opts CheckpointOptions) (*PortableCheckpointInfo, error) {
	if opts.Store != "" {
		return nil, invalidConfig("checkpoint stores are local-only; cloud checkpoints are managed by Smol Cloud.")
	}
	if output != "" {
		return nil, invalidConfig("cloud checkpoint capture returns durable metadata; use its DownloadURL to save an artifact.")
	}
	var w checkpointWire
	if err := cloudJSON(ctx, t.conn, "POST", t.path("/checkpoints"), fetchOpts{timeout: 30 * time.Minute}, &w); err != nil {
		return nil, err
	}
	info := w.info()
	return &info, nil
}

func (t *cloudTransport) checkpoints(ctx context.Context) ([]PortableCheckpointInfo, error) {
	var ws []checkpointWire
	if err := cloudJSON(ctx, t.conn, "GET", t.path("/checkpoints"), fetchOpts{}, &ws); err != nil {
		return nil, err
	}
	out := make([]PortableCheckpointInfo, len(ws))
	for i, w := range ws {
		out[i] = w.info()
	}
	return out, nil
}

func (t *cloudTransport) resize(context.Context, ResizeOptions) (*MachineResources, error) {
	return nil, notSupported("live resize is currently available on the local target.")
}

type portWire struct {
	Port     int `json:"port"`
	HostPort int `json:"hostPort"`
}

func portBody(ports []PortSpec) []portWire {
	out := make([]portWire, len(ports))
	for i, p := range ports {
		out[i] = portWire{Port: p.Guest, HostPort: p.Host}
	}
	return out
}

func isNotFound(err error) bool {
	var se *Error
	return errors.As(err, &se) && se.Code == CodeNotFound
}

// fork makes a live-RAM copy-on-write branch on the source's node. The
// control plane returns the running child; wait for its agent so the handle
// is usable.
func (t *cloudTransport) fork(ctx context.Context, name string, opts BranchOptions) (transport, error) {
	body := map[string]any{"name": name, "ports": portBody(opts.Ports)}
	if opts.Branchable {
		body["branchable"] = true
	}
	var clone machineInfo
	err := cloudJSON(ctx, t.conn, "POST", t.path("/branches"), fetchOpts{json: body}, &clone)
	if isNotFound(err) {
		// Older control planes expose only /fork.
		legacy := map[string]any{"name": name, "ports": portBody(opts.Ports)}
		if opts.Branchable {
			legacy["forkable"] = true
		}
		err = cloudJSON(ctx, t.conn, "POST", t.path("/fork"), fetchOpts{json: legacy}, &clone)
	}
	if err != nil {
		return nil, err
	}
	if err := waitForReady(ctx, t.conn, clone.ID, WaitReadyOptions{}); err != nil {
		return nil, err
	}
	return newCloud(t.conn, clone.nameOr(name), clone.ID), nil
}

// forkBatch is one transactional call: the control plane forks all children
// or none. The size spec goes raw so the server resolves names uniformly.
func (t *cloudTransport) forkBatch(ctx context.Context, opts BranchBatchOptions) ([]transport, error) {
	body := map[string]any{"ports": portBody(opts.Ports)}
	if len(opts.Names) > 0 {
		body["names"] = opts.Names
	}
	if opts.Count > 0 {
		body["count"] = opts.Count
	}
	if opts.NamePrefix != "" {
		body["namePrefix"] = opts.NamePrefix
	}
	var resp struct {
		Clones []machineInfo `json:"clones"`
	}
	err := cloudJSON(ctx, t.conn, "POST", t.path("/branches/batch"), fetchOpts{json: body}, &resp)
	if isNotFound(err) {
		err = cloudJSON(ctx, t.conn, "POST", t.path("/fork-batch"), fetchOpts{json: body}, &resp)
	}
	if err != nil {
		return nil, err
	}
	errs := make([]error, len(resp.Clones))
	var wg sync.WaitGroup
	for i, c := range resp.Clones {
		wg.Add(1)
		go func() {
			defer wg.Done()
			errs[i] = waitForReady(ctx, t.conn, c.ID, WaitReadyOptions{})
		}()
	}
	wg.Wait()
	if err := errors.Join(errs...); err != nil {
		return nil, err
	}
	out := make([]transport, len(resp.Clones))
	for i, c := range resp.Clones {
		out[i] = newCloud(t.conn, c.nameOr(c.ID), c.ID)
	}
	return out, nil
}

// assign is one idempotent call: the control plane forks and provisions one
// child and returns it only once fully installed, plus the owner token. Files
// go base64-encoded for a binary-safe hop.
func (t *cloudTransport) assign(ctx context.Context, opts AssignOptions) (transport, string, string, error) {
	body := map[string]any{"leaseId": opts.LeaseID}
	if opts.Task != nil {
		body["task"] = opts.Task
	}
	if opts.Files != nil {
		files := make([]map[string]string, 0, len(opts.Files))
		for path, content := range opts.Files {
			files = append(files, map[string]string{"path": path, "contentB64": base64.StdEncoding.EncodeToString(content)})
		}
		body["files"] = files
	}
	if opts.Secrets != nil {
		body["secrets"] = opts.Secrets
	}
	if opts.Ports != nil {
		body["ports"] = portBody(opts.Ports)
	}
	if opts.TTL > 0 {
		body["ttlSecs"] = timeoutSeconds(opts.TTL)
	}
	if opts.Heartbeat > 0 {
		body["heartbeatSecs"] = timeoutSeconds(opts.Heartbeat)
	}
	var resp struct {
		MachineID  string       `json:"machineId"`
		OwnerToken string       `json:"ownerToken"`
		Machine    *machineInfo `json:"machine"`
	}
	if err := cloudJSON(ctx, t.conn, "POST", t.path("/assign"), fetchOpts{json: body}, &resp); err != nil {
		return nil, "", "", err
	}
	if err := waitForReady(ctx, t.conn, resp.MachineID, WaitReadyOptions{}); err != nil {
		return nil, "", "", err
	}
	name := resp.MachineID
	if resp.Machine != nil {
		name = resp.Machine.nameOr(resp.MachineID)
	}
	return newCloud(t.conn, name, resp.MachineID), opts.LeaseID, resp.OwnerToken, nil
}

func (t *cloudTransport) heartbeatLease(ctx context.Context, leaseID, ownerToken string) error {
	_, err := cloudDo(ctx, t.conn, "POST", "/v1/leases/"+leaseID+"/heartbeat", fetchOpts{json: map[string]any{"ownerToken": ownerToken}})
	return err
}

func (t *cloudTransport) completeLease(ctx context.Context, leaseID, ownerToken, reason string, score *float64, result any) error {
	body := map[string]any{"ownerToken": ownerToken, "reason": reason}
	if score != nil {
		body["score"] = *score
	}
	if result != nil {
		body["result"] = result
	}
	_, err := cloudDo(ctx, t.conn, "POST", "/v1/leases/"+leaseID+"/complete", fetchOpts{json: body})
	return err
}

func (t *cloudTransport) getLease(ctx context.Context, leaseID string) (map[string]any, error) {
	var out map[string]any
	if err := cloudJSON(ctx, t.conn, "GET", "/v1/leases/"+leaseID, fetchOpts{}, &out); err != nil {
		return nil, err
	}
	return out, nil
}

// ---------------------------------------------------------------------------
// CLI session reuse
// ---------------------------------------------------------------------------

// readCLICloudTable reads the flat `[cloud]` table the smol CLI writes to
// ~/.config/smolvm/config.toml (that path on every platform).
func readCLICloudTable() map[string]string {
	home, err := os.UserHomeDir()
	if err != nil {
		return map[string]string{}
	}
	text, err := os.ReadFile(filepath.Join(home, ".config", "smolvm", "config.toml"))
	if err != nil {
		return map[string]string{}
	}
	out := map[string]string{}
	inCloud := false
	for _, raw := range strings.Split(string(text), "\n") {
		s := strings.TrimSpace(strings.TrimSuffix(raw, "\r"))
		if strings.HasPrefix(s, "[") && strings.HasSuffix(s, "]") {
			inCloud = s == "[cloud]"
			continue
		}
		if !inCloud || strings.HasPrefix(s, "#") || !strings.Contains(s, "=") {
			continue
		}
		i := strings.Index(s, "=")
		v := strings.TrimSpace(s[i+1:])
		v = strings.TrimPrefix(strings.TrimPrefix(v, `"`), "'")
		v = strings.TrimSuffix(strings.TrimSuffix(v, `"`), "'")
		out[strings.TrimSpace(s[:i])] = v
	}
	return out
}

// tokenIsExpired is conservative: expired only when the timestamp parses
// cleanly and is in the past, so a valid key is never blocked over a quirk.
func tokenIsExpired(expiresAt string) bool {
	if expiresAt == "" {
		return false
	}
	at, err := time.Parse(time.RFC3339Nano, expiresAt)
	return err == nil && !at.After(time.Now())
}

// cliSession is the CLI's stored login, used only once the cloud target is
// already selected.
func cliSession(target Target) (apiKey, endpoint string) {
	if target == TargetLocal {
		return "", ""
	}
	cloud := readCLICloudTable()
	if cloud["api_key"] == "" || tokenIsExpired(cloud["token_expires_at"]) {
		return "", ""
	}
	return cloud["api_key"], cloud["endpoint"]
}

// cliConfigAPIKey is the `api_key` under `[cloud]` in
// <config-dir>/smolvm/config.toml, where config-dir is $XDG_CONFIG_HOME or
// ~/.config.
func cliConfigAPIKey() string {
	base := os.Getenv("XDG_CONFIG_HOME")
	if base == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return ""
		}
		base = filepath.Join(home, ".config")
	}
	text, err := os.ReadFile(filepath.Join(base, "smolvm", "config.toml"))
	if err != nil {
		return ""
	}
	inCloud := false
	for _, raw := range strings.Split(string(text), "\n") {
		line := strings.TrimSpace(strings.TrimSuffix(raw, "\r"))
		if strings.HasPrefix(line, "[") {
			inCloud = line == "[cloud]"
			continue
		}
		if !inCloud || !strings.HasPrefix(line, "api_key") {
			continue
		}
		rest := strings.TrimSpace(strings.TrimPrefix(line, "api_key"))
		if !strings.HasPrefix(rest, "=") {
			continue
		}
		rest = strings.TrimSpace(rest[1:])
		if len(rest) >= 2 && (rest[0] == '"' || rest[0] == '\'') {
			if end := strings.IndexByte(rest[1:], rest[0]); end >= 0 {
				return rest[1 : end+1]
			}
		}
	}
	return ""
}

// cloudConnFor is the control plane and key a cloud operation uses: explicit
// options, then the environment, then the CLI session.
func cloudConnFor(c ConnectOptions, operation string) (cloudConn, error) {
	key := c.APIKey
	if key == "" {
		key = os.Getenv("SMOL_CLOUD_TOKEN")
	}
	cliKey, cliURL := cliSession(c.Target)
	if key == "" {
		key = cliKey
	}
	if key == "" {
		key = cliConfigAPIKey()
	}
	if key == "" {
		return cloudConn{}, invalidConfig(operation + " requires an API key — " + noKeyHint + ".")
	}
	base := c.BaseURL
	if base == "" {
		base = os.Getenv("SMOL_CLOUD_URL")
	}
	if base == "" {
		base = cliURL
	}
	if base == "" {
		base = defaultCloudURL
	}
	return cloudConn{baseURL: strings.TrimRight(base, "/"), apiKey: key}, nil
}

// ---------------------------------------------------------------------------
// Cloud factories
// ---------------------------------------------------------------------------

// networkMode is the create request's `network` for a boolean request. The
// control plane opens egress when the field is absent, so an explicit false
// must be sent as blocked; unset keeps the control plane's default.
func networkMode(network *bool) map[string]any {
	if network == nil {
		return nil
	}
	if *network {
		return map[string]any{"mode": "open"}
	}
	return map[string]any{"mode": "blocked"}
}

// cloudNetwork is the cloud API's form of a NetworkPolicy.
func cloudNetwork(p NetworkPolicy) (map[string]any, error) {
	switch p.mode {
	case "allow-all":
		return map[string]any{"mode": "open"}, nil
	case "deny-all":
		return map[string]any{"mode": "blocked"}, nil
	}
	_, cidrs, hosts, err := egressPolicy(p)
	if err != nil {
		return nil, err
	}
	return map[string]any{"mode": "allowCidrs", "cidrs": cidrs, "hosts": hosts}, nil
}

func createCloud(ctx context.Context, cfg MachineConfig, c ConnectOptions) (transport, error) {
	if cfg.EgressInterceptor != nil || c.EgressInterceptor != nil {
		return nil, notSupported("EgressInterceptor is local-only.")
	}
	// The cloud create API has no workload user; refuse rather than silently
	// run the workload as the image's default user.
	if cfg.User != "" {
		return nil, notSupported("User is local-only.")
	}
	conn, err := cloudConnFor(c, "cloud target")
	if err != nil {
		return nil, err
	}
	if cfg.Image == "" {
		return nil, invalidConfig(`cloud target requires an image — set MachineConfig.Image when creating with Target: TargetCloud.`)
	}
	// A remote machine has no host filesystem to bind; reject rather than
	// silently drop the mounts.
	if len(cfg.Mounts) > 0 {
		return nil, notSupported("host mounts are local-only and are not applied on the cloud target; " +
			"use cloud volumes for persistent storage instead.")
	}
	resources := map[string]any{"diskGb": nil}
	if cfg.Resources.CPUs > 0 {
		resources["cpus"] = cfg.Resources.CPUs
	}
	if cfg.Resources.MemoryMB > 0 {
		resources["memoryMb"] = cfg.Resources.MemoryMB
	}
	if cfg.Resources.StorageGB > 0 {
		resources["diskGb"] = cfg.Resources.StorageGB
	}
	body := map[string]any{
		"name":            nil,
		"source":          map[string]any{"type": "image", "reference": cfg.Image},
		"resources":       resources,
		"autoStopSeconds": nil,
		"ttlSeconds":      nil,
	}
	if cfg.Name != "" {
		body["name"] = cfg.Name
	}
	if len(cfg.Resources.AllowCIDRs) > 0 || len(cfg.Resources.AllowHosts) > 0 {
		cidrs, hosts := cfg.Resources.AllowCIDRs, cfg.Resources.AllowHosts
		if cidrs == nil {
			cidrs = []string{}
		}
		if hosts == nil {
			hosts = []string{}
		}
		body["network"] = map[string]any{"mode": "allowCidrs", "cidrs": cidrs, "hosts": hosts}
	} else if n := networkMode(resolveNetwork(cfg)); n != nil {
		body["network"] = n
	}
	// Published ports: only the guest port; the control plane allocates the
	// node host port.
	if len(cfg.Ports) > 0 {
		ports := make([]map[string]int, len(cfg.Ports))
		for i, p := range cfg.Ports {
			ports[i] = map[string]int{"port": p.Guest}
		}
		body["ports"] = ports
	}
	if len(cfg.Env) > 0 {
		body["env"] = cfg.Env
	}
	if cfg.Workdir != "" {
		body["workdir"] = cfg.Workdir
	}
	if cfg.AutoStop > 0 {
		body["autoStopSeconds"] = timeoutSeconds(cfg.AutoStop)
	}
	if cfg.TTL > 0 {
		body["ttlSeconds"] = timeoutSeconds(cfg.TTL)
	}
	// Branchable is a create-time property: the control plane stores it and
	// the branch endpoint checks the stored flag.
	if cfg.Branchable {
		body["forkable"] = true
	}
	var created machineInfo
	if err := cloudJSON(ctx, conn, "POST", "/v1/machines", fetchOpts{json: body}, &created); err != nil {
		return nil, err
	}
	id := created.ID
	name := created.nameOr(cfg.Name)
	if name == "" {
		name = id
	}
	// The machine exists now; if start or readiness fails, delete it so it
	// does not leak (and bill) as an orphan.
	startPath := "/v1/machines/" + id + "/start"
	if cfg.Branchable {
		// A branch source boots with cloneable guest RAM.
		startPath += "?forkable=true"
	}
	var startErr error
	if _, err := cloudDo(ctx, conn, "POST", startPath, fetchOpts{timeout: cloudStartTimeout}); err != nil {
		// Readiness decides; keep why start failed to explain a readiness failure.
		startErr = err
	}
	if err := waitForReady(ctx, conn, id, WaitReadyOptions{}); err != nil {
		var se *Error
		if startErr != nil && errors.As(err, &se) {
			err = &Error{Code: se.Code, Message: fmt.Sprintf("%s (start failed: %s)", se.Message, startErr), Status: se.Status}
		}
		_, _ = cloudDo(context.WithoutCancel(ctx), conn, "DELETE", "/v1/machines/"+id, fetchOpts{})
		return nil, err
	}
	return newCloud(conn, name, id), nil
}

// machineRows reads a list response: a bare array, or {machines: [...]}.
func machineRows(raw []byte) ([]machineInfo, error) {
	var rows []machineInfo
	if err := json.Unmarshal(raw, &rows); err == nil {
		return rows, nil
	}
	var wrapped struct {
		Machines []machineInfo `json:"machines"`
	}
	if err := json.Unmarshal(raw, &wrapped); err != nil {
		return nil, &Error{Code: CodeSmolvm, Message: "decode machine list: " + err.Error()}
	}
	return wrapped.Machines, nil
}

func listRaw(ctx context.Context, conn cloudConn) ([]machineInfo, error) {
	raw, err := cloudDo(ctx, conn, "GET", "/v1/machines", fetchOpts{})
	if err != nil || raw == nil {
		return nil, err
	}
	return machineRows(raw)
}

func listCloud(ctx context.Context, c ConnectOptions) ([]MachineSummary, error) {
	conn, err := cloudConnFor(c, "list")
	if err != nil {
		return nil, err
	}
	rows, err := listRaw(ctx, conn)
	if err != nil {
		return nil, err
	}
	out := make([]MachineSummary, len(rows))
	for i, m := range rows {
		created, _ := time.Parse(time.RFC3339Nano, m.CreatedAt)
		s := MachineSummary{
			Name: m.nameOr(m.ID), ID: m.ID, State: m.State,
			// The cloud API carries no caller labels yet.
			Labels:     map[string]string{},
			Persistent: !m.Ephemeral,
			// Remote machines never depend on a client process.
			Detached:  true,
			CreatedAt: created,
		}
		if m.Source != nil && m.Source.Type == "image" {
			s.Image = m.Source.Reference
		}
		if m.Branchable != nil {
			s.Branchable = *m.Branchable
		} else if m.Forkable != nil {
			s.Branchable = *m.Forkable
		}
		out[i] = s
	}
	return out, nil
}

// connectCloud resolves like the CLI: the id path first, then on 404 the
// machine list matched by name, so Connect(other.Name()) works too.
func connectCloud(ctx context.Context, id string, c ConnectOptions) (transport, error) {
	if c.EgressInterceptor != nil {
		return nil, notSupported("EgressInterceptor is local-only.")
	}
	conn, err := cloudConnFor(c, "connect")
	if err != nil {
		return nil, err
	}
	var m machineInfo
	err = cloudJSON(ctx, conn, "GET", "/v1/machines/"+id, fetchOpts{}, &m)
	if err != nil {
		var se *Error
		if !errors.As(err, &se) || se.Status != http.StatusNotFound {
			return nil, err
		}
		rows, listErr := listRaw(ctx, conn)
		if listErr != nil {
			return nil, listErr
		}
		found := false
		for _, row := range rows {
			if (row.Name != nil && *row.Name == id) || row.ID == id {
				m, found = row, true
				break
			}
		}
		if !found {
			return nil, err
		}
	}
	mid := m.ID
	if mid == "" {
		mid = id
	}
	return newCloud(conn, m.nameOr(id), mid), nil
}

// restoreCloud restores a durable cloud checkpoint (or a checkpoint file on
// this host, uploaded first) into a new machine, returning once it is ready.
func restoreCloud(ctx context.Context, checkpoint, name string, c ConnectOptions, opts RestoreCheckpointOptions) (transport, error) {
	conn, err := cloudConnFor(c, "RestoreCheckpoint")
	if err != nil {
		return nil, err
	}
	id := checkpoint
	if isCheckpointFile(checkpoint) {
		path, err := filepath.Abs(checkpoint)
		if err != nil {
			return nil, err
		}
		up, err := uploadCheckpointFile(ctx, conn, path, nil)
		if err != nil {
			return nil, err
		}
		id = up.ID
	}
	body := map[string]any{"name": name}
	if opts.NetworkPolicy != nil {
		n, err := cloudNetwork(*opts.NetworkPolicy)
		if err != nil {
			return nil, err
		}
		body["network"] = n
	}
	var created machineInfo
	if err := cloudJSON(ctx, conn, "POST", "/v1/checkpoints/"+encodeURIComponent(id)+"/restore", fetchOpts{json: body}, &created); err != nil {
		return nil, err
	}
	_, err = cloudDo(ctx, conn, "POST", "/v1/machines/"+created.ID+"/start", fetchOpts{timeout: 30 * time.Minute})
	if err == nil {
		err = waitForReady(ctx, conn, created.ID, WaitReadyOptions{})
	}
	if err != nil {
		_, _ = cloudDo(context.WithoutCancel(ctx), conn, "DELETE", "/v1/machines/"+created.ID, fetchOpts{})
		return nil, err
	}
	return newCloud(conn, created.nameOr(name), created.ID), nil
}

func uploadCheckpointCloud(ctx context.Context, path string, c ConnectOptions, progress func(sent, total int64)) (*CloudCheckpointInfo, error) {
	conn, err := cloudConnFor(c, "UploadCheckpoint")
	if err != nil {
		return nil, err
	}
	abs, err := filepath.Abs(path)
	if err != nil {
		return nil, err
	}
	return uploadCheckpointFile(ctx, conn, abs, progress)
}

const (
	uploadParallelism  = 4
	uploadPartAttempts = 4
)

// uploadCheckpointFile uploads a checkpoint file taken on this host so it can
// be restored in the cloud. The bytes go straight to object storage in
// parallel parts; the cloud then reads the checkpoint's own manifest.
func uploadCheckpointFile(ctx context.Context, conn cloudConn, path string, progress func(sent, total int64)) (*CloudCheckpointInfo, error) {
	st, err := os.Stat(path)
	if err != nil {
		return nil, err
	}
	size := st.Size()
	var upload struct {
		Checkpoint    CloudCheckpointInfo `json:"checkpoint"`
		PartSizeBytes int64               `json:"partSizeBytes"`
		UploadURLs    []string            `json:"uploadUrls"`
	}
	if err := cloudJSON(ctx, conn, "POST", "/v1/checkpoints", fetchOpts{json: map[string]any{"sizeBytes": size}}, &upload); err != nil {
		return nil, err
	}
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()

	var mu sync.Mutex
	sent := make([]int64, len(upload.UploadURLs))
	setSent := func(part int, n int64) {
		mu.Lock()
		defer mu.Unlock()
		sent[part] = n
		if progress != nil {
			var total int64
			for _, s := range sent {
				total += s
			}
			progress(total, size)
		}
	}
	parts := make(chan int)
	errs := make([]error, len(upload.UploadURLs))
	var wg sync.WaitGroup
	for range min(uploadParallelism, len(upload.UploadURLs)) {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for part := range parts {
				start := int64(part) * upload.PartSizeBytes
				length := min(upload.PartSizeBytes, size-start)
				for attempt := 0; ; attempt++ {
					err := putPart(ctx, upload.UploadURLs[part], io.NewSectionReader(f, start, length), start, length, func(n int64) { setSent(part, n) })
					if err == nil {
						break
					}
					setSent(part, 0)
					if attempt+1 >= uploadPartAttempts || ctx.Err() != nil {
						errs[part] = err
						break
					}
					select {
					case <-ctx.Done():
					case <-time.After(2 * time.Second << attempt):
					}
				}
			}
		}()
	}
	for part := range upload.UploadURLs {
		parts <- part
	}
	close(parts)
	wg.Wait()
	if err := errors.Join(errs...); err != nil {
		return nil, err
	}
	var info CloudCheckpointInfo
	if err := cloudJSON(ctx, conn, "POST", "/v1/checkpoints/"+encodeURIComponent(upload.Checkpoint.ID)+"/complete", fetchOpts{timeout: 30 * time.Minute}, &info); err != nil {
		return nil, err
	}
	return &info, nil
}

type countingReader struct {
	r    io.Reader
	n    int64
	tick func(int64)
}

func (c *countingReader) Read(p []byte) (int, error) {
	n, err := c.r.Read(p)
	c.n += int64(n)
	if n > 0 {
		c.tick(c.n)
	}
	return n, err
}

// putPart PUTs one slice of a file to a pre-signed URL. The API key is never
// sent to storage.
func putPart(ctx context.Context, target string, body io.Reader, start, length int64, onSent func(int64)) error {
	var reader io.Reader = http.NoBody
	if length > 0 {
		reader = &countingReader{r: body, tick: onSent}
	}
	req, err := http.NewRequestWithContext(ctx, "PUT", target, reader)
	if err != nil {
		return err
	}
	req.ContentLength = length
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		if ctxErr := ctx.Err(); ctxErr != nil {
			return ctxErr
		}
		return &Error{Code: CodeConnection, Message: fmt.Sprintf("upload part at byte %d failed: %s", start, err)}
	}
	defer res.Body.Close()
	if res.StatusCode < 200 || res.StatusCode > 299 {
		b, _ := io.ReadAll(io.LimitReader(res.Body, 300))
		return &Error{Code: CodeConnection, Message: fmt.Sprintf("upload part at byte %d failed: %d %s", start, res.StatusCode, b), Status: res.StatusCode}
	}
	return nil
}
