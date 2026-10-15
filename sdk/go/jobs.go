package smol

import (
	"bytes"
	"context"
	"fmt"
	"time"
)

// Job is a command running detached on a cloud machine, readable and
// cancellable after the call that started it returns.
type Job struct {
	t  transport
	id string
}

// StartJob runs command on the machine without waiting for it, and returns
// the job to follow it. Cloud only; a local machine streams with ExecStream.
func (m *Machine) StartJob(ctx context.Context, command []string, opts JobOptions) (*Job, error) {
	info, err := m.t.startJob(ctx, command, opts)
	if err != nil {
		return nil, err
	}
	return &Job{t: m.t, id: info.ID}, nil
}

// Job returns a handle to an existing job by id. Cloud only.
func (m *Machine) Job(id string) *Job { return &Job{t: m.t, id: id} }

// Jobs lists the machine's jobs. Cloud only.
func (m *Machine) Jobs(ctx context.Context) ([]JobInfo, error) { return m.t.listJobs(ctx) }

// ID is the job's id.
func (j *Job) ID() string { return j.id }

// Info reads the job's current state.
func (j *Job) Info(ctx context.Context) (*JobInfo, error) { return j.t.getJob(ctx, j.id) }

// Output reads a slice of the job's stdout or stderr.
func (j *Job) Output(ctx context.Context, opts JobOutputOptions) (*JobOutput, error) {
	return j.t.jobOutput(ctx, j.id, opts)
}

// ReadAll reads everything the job has written to stream ("stdout" when
// empty) so far, or all of it once the job has finished.
func (j *Job) ReadAll(ctx context.Context, stream string) ([]byte, error) {
	var buf bytes.Buffer
	var offset int64
	for {
		out, err := j.Output(ctx, JobOutputOptions{Stream: stream, Offset: offset, Limit: 1 << 20})
		if err != nil {
			return nil, err
		}
		buf.Write(out.Data)
		offset = out.NextOffset
		if out.EOF || len(out.Data) == 0 {
			return buf.Bytes(), nil
		}
	}
}

// Cancel stops the job; it exits 143. Cancelling a finished job is harmless.
func (j *Job) Cancel(ctx context.Context) (*JobInfo, error) { return j.t.cancelJob(ctx, j.id) }

// Wait polls until the job is no longer running and returns its final state.
// The job keeps running if ctx ends or the timeout passes first.
func (j *Job) Wait(ctx context.Context, opts JobWaitOptions) (*JobInfo, error) {
	poll := opts.Poll
	if poll <= 0 {
		poll = time.Second
	}
	var deadline time.Time
	if opts.Timeout > 0 {
		deadline = time.Now().Add(opts.Timeout)
	}
	for {
		info, err := j.Info(ctx)
		if err != nil {
			return nil, err
		}
		if info.State != "running" {
			return info, nil
		}
		wait := poll
		if !deadline.IsZero() {
			left := time.Until(deadline)
			if left <= 0 {
				return nil, &Error{Code: CodeTimeout, Message: fmt.Sprintf("job %s still running after %s", j.id, opts.Timeout)}
			}
			wait = min(wait, left)
		}
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(wait):
		}
	}
}
