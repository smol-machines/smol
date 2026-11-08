package smol

// Resumable execs.
//
// A resumable exec (/v1/machines/{id}/execs) is a command the machine's node
// owns rather than the connection that started it. The client names it before
// the first try (a retried start is then the same start) and reads its output
// by offset: GET .../execs/{exec}?after=<next> answers with the output after
// next, and asking with after = next also tells the node it may drop what came
// before. A dropped connection or a control restart costs a retry: the same
// read returns the same bytes, so each byte arrives once, and the exit code
// with the last of them.

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"strconv"
	"time"
)

const (
	// resumableWait is how long a read asks the control to wait for output.
	resumableWait = 20
	// resumableReadTimeout bounds one read on the wire: its wait plus room.
	resumableReadTimeout = (resumableWait + 30) * time.Second
	// resumableBudget is how long failures in a row are retried before the
	// exec is given up on (the command keeps running on the node meanwhile).
	resumableBudget = 10 * time.Minute
)

// resumableAnswer is one answer from a start or a read.
type resumableAnswer struct {
	ExecID   *string `json:"execId"`
	Done     bool    `json:"done"`
	ExitCode *int    `json:"exitCode"`
	Error    *string `json:"error"`
	Next     *uint64 `json:"next"`
	Output   []struct {
		Stream string `json:"stream"`
		Offset uint64 `json:"offset"`
		Data   string `json:"data"`
	} `json:"output"`
	Lost *struct {
		From uint64 `json:"from"`
		To   uint64 `json:"to"`
	} `json:"lost"`
}

// resumablePiece is what following a resumable exec yields: output, a gap
// the node no longer held, or the end.
type resumablePiece struct {
	kind     string // "stdout", "stderr", "lost" or "end"
	data     []byte
	from, to uint64
	exitCode *int
	message  string
}

// resumableVerdict says what to do with a refused request: "retry",
// "unsupported" (a start on a control plane without resumable execs: use the
// plain exec) or "fail". A read is safe to repeat whatever happened; a start
// only when it cannot have reached a node (a gateway error, a draining
// control, a rate limit).
func resumableVerdict(status int, code string, starting bool) string {
	switch {
	case status == 408 || status == 429 || status == 502 || status == 504:
		return "retry"
	case status == 503 && code == "machine_interrupted":
		return "fail"
	case status == 503 && (!starting || code == "" || code == "control_draining"):
		return "retry"
	case starting && (status == 404 || status == 405 || status == 501):
		return "unsupported"
	}
	return "fail"
}

// backoff waits between retries: short at first, never above two seconds,
// for up to resumableBudget of failures in a row. It reports false once the
// budget is spent or ctx is done.
type backoff struct {
	since   time.Time
	attempt int
}

func (b *backoff) wait(ctx context.Context, retryAfter time.Duration) bool {
	if b.since.IsZero() {
		b.since = time.Now()
	}
	if time.Since(b.since) >= resumableBudget {
		return false
	}
	step := time.Duration(100<<min(b.attempt, 5)) * time.Millisecond
	if step > 2*time.Second {
		step = 2 * time.Second
	}
	b.attempt++
	if retryAfter >= 0 {
		step = min(max(retryAfter, 50*time.Millisecond), 5*time.Second)
	}
	select {
	case <-ctx.Done():
		return false
	case <-time.After(step):
		return true
	}
}

func (b *backoff) reset() { b.since, b.attempt = time.Time{}, 0 }

// resumableRequest sends one request until it has an answer worth acting on.
// It returns unsupported=true for a start the control plane has no route for.
func resumableRequest(ctx context.Context, c cloudConn, method, path string, body any, starting bool) (*resumableAnswer, bool, error) {
	var b backoff
	var payload []byte
	if body != nil {
		var err error
		if payload, err = json.Marshal(body); err != nil {
			return nil, false, invalidConfig("encode request: " + err.Error())
		}
	}
	for {
		if err := ctx.Err(); err != nil {
			return nil, false, err
		}
		status, retryAfter, data, err := resumableOnce(ctx, c, method, path, payload)
		if err != nil {
			if ctxErr := ctx.Err(); ctxErr != nil {
				return nil, false, ctxErr
			}
			// The connection dropped or timed out: the control may be
			// restarting, and the load balancer sends the next try to one
			// that serves.
			if b.wait(ctx, -1) {
				continue
			}
			return nil, false, &Error{Code: CodeConnection, Message: fmt.Sprintf("cloud %s %s kept failing; the command may still be running: %s", method, path, err)}
		}
		if status >= 200 && status <= 299 {
			var answer resumableAnswer
			if err := json.Unmarshal(data, &answer); err == nil {
				return &answer, false, nil
			}
			if b.wait(ctx, -1) {
				continue
			}
		}
		var problem struct {
			Code string `json:"code"`
		}
		_ = json.Unmarshal(data, &problem)
		switch resumableVerdict(status, problem.Code, starting) {
		case "unsupported":
			return nil, true, nil
		case "retry":
			if b.wait(ctx, retryAfter) {
				continue
			}
		}
		code := CodeSmolvm
		switch status {
		case http.StatusNotFound:
			code = CodeNotFound
		case http.StatusUnauthorized:
			code = CodeUnauthorized
		case http.StatusConflict:
			code = CodeConflict
		}
		msg := fmt.Sprintf("cloud %s %s → %d", method, path, status)
		if text := errorSentence(string(data)); text != "" {
			msg += ": " + text
		}
		return nil, false, &Error{Code: code, Message: msg, Status: status}
	}
}

// resumableOnce sends one request and reads its whole answer.
func resumableOnce(ctx context.Context, c cloudConn, method, path string, payload []byte) (int, time.Duration, []byte, error) {
	reqCtx, cancel := context.WithTimeout(ctx, resumableReadTimeout)
	defer cancel()
	var body io.Reader
	if payload != nil {
		body = bytes.NewReader(payload)
	}
	req, err := http.NewRequestWithContext(reqCtx, method, c.baseURL+path, body)
	if err != nil {
		return 0, 0, nil, err
	}
	req.Header.Set("authorization", "Bearer "+c.apiKey)
	if payload != nil {
		req.Header.Set("content-type", "application/json")
	}
	res, err := http.DefaultClient.Do(req)
	if err != nil {
		return 0, 0, nil, err
	}
	defer res.Body.Close()
	data, err := io.ReadAll(res.Body)
	if err != nil {
		return 0, 0, nil, err
	}
	retryAfter := time.Duration(-1)
	if s, err := strconv.Atoi(res.Header.Get("retry-after")); err == nil {
		retryAfter = time.Duration(s) * time.Second
	}
	return res.StatusCode, retryAfter, data, nil
}

// followResumable runs req as a resumable exec in machine id, handing each
// piece to yield (which stops the follow by returning false). It reports
// supported=false, having yielded nothing, when the control plane has no
// resumable execs.
func followResumable(ctx context.Context, c cloudConn, id string, req commandRequest, yield func(resumablePiece) bool) (bool, error) {
	var raw [16]byte
	if _, err := rand.Read(raw[:]); err != nil {
		return false, &Error{Code: CodeInternal, Message: "exec id: " + err.Error()}
	}
	execID := hex.EncodeToString(raw[:])
	start := struct {
		commandRequest
		ExecID      string `json:"execId"`
		WaitSeconds int    `json:"waitSeconds"`
	}{req, execID, resumableWait}
	start.Background = false
	answer, unsupported, err := resumableRequest(ctx, c, "POST", "/v1/machines/"+id+"/execs", start, true)
	if err != nil {
		return true, err
	}
	// An answer that is not an exec's (a catch-all route) means the control
	// plane has none.
	if unsupported || answer.ExecID == nil {
		return false, nil
	}
	var next uint64
	for {
		if answer.Next == nil {
			return true, &Error{Code: CodeSmolvm, Message: "the control plane stopped serving this exec"}
		}
		if answer.Lost != nil && answer.Lost.To > next {
			if !yield(resumablePiece{kind: "lost", from: max(answer.Lost.From, next), to: answer.Lost.To}) {
				return true, nil
			}
			next = answer.Lost.To
		}
		for _, chunk := range answer.Output {
			data, err := base64.StdEncoding.DecodeString(chunk.Data)
			if err != nil {
				return true, &Error{Code: CodeSmolvm, Message: "undecodable output: " + err.Error()}
			}
			end := chunk.Offset + uint64(len(data))
			if end <= next {
				continue
			}
			if chunk.Offset > next {
				break // never out of order: read again from next
			}
			fresh := data[next-chunk.Offset:]
			next = end
			kind := "stdout"
			if chunk.Stream == "stderr" {
				kind = "stderr"
			}
			if !yield(resumablePiece{kind: kind, data: fresh}) {
				return true, nil
			}
		}
		if answer.Done && *answer.Next <= next {
			// Collected: the node may forget it now rather than at its TTL.
			delCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), 10*time.Second)
			_, _, _, _ = resumableOnce(delCtx, c, "DELETE", "/v1/machines/"+id+"/execs/"+execID, nil)
			cancel()
			end := resumablePiece{kind: "end", exitCode: answer.ExitCode}
			if answer.Error != nil {
				end.message = *answer.Error
			}
			yield(end)
			return true, nil
		}
		path := fmt.Sprintf("/v1/machines/%s/execs/%s?after=%d&wait=%d", id, execID, next, resumableWait)
		answer, _, err = resumableRequest(ctx, c, "GET", path, nil, false)
		if err != nil {
			return true, err
		}
	}
}

// execResumable is exec as a resumable exec; nil, nil when the control plane
// has no resumable execs.
func (t *cloudTransport) execResumable(ctx context.Context, command []string, opts *ExecOptions) (*ExecResult, error) {
	var stdout, stderr bytes.Buffer
	var lost uint64
	var end *resumablePiece
	supported, err := followResumable(ctx, t.conn, t.id, newCommandRequest(command, opts), func(p resumablePiece) bool {
		switch p.kind {
		case "stdout":
			stdout.Write(p.data)
		case "stderr":
			stderr.Write(p.data)
		case "lost":
			lost += p.to - p.from
		case "end":
			end = &p
		}
		return true
	})
	if !supported {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	if end == nil {
		return nil, &Error{Code: CodeSmolvm, Message: "the exec ended without its exit status"}
	}
	if end.exitCode == nil {
		msg := end.message
		if msg == "" {
			msg = "no reason given"
		}
		return nil, &Error{Code: CodeSmolvm, Message: "the command ended without an exit code: " + msg}
	}
	if lost > 0 {
		return nil, &Error{Code: CodeSmolvm, Message: fmt.Sprintf("the command exited %d, but %d bytes of its output were dropped by the machine's node before this client read them", *end.exitCode, lost)}
	}
	return &ExecResult{
		ExitCode:    *end.exitCode,
		Stdout:      stdout.String(),
		Stderr:      stderr.String(),
		StdoutBytes: stdout.Bytes(),
		StderrBytes: stderr.Bytes(),
	}, nil
}
