package smol

import (
	"errors"
	"fmt"
)

// Error codes. They match the smol Node and Python SDKs, so a code means the
// same thing in every language.
const (
	CodeNotFound              = "NOT_FOUND"
	CodeInvalidState          = "INVALID_STATE"
	CodeHypervisorUnavailable = "HYPERVISOR_UNAVAILABLE"
	CodeConflict              = "CONFLICT"
	CodeForbidden             = "FORBIDDEN"
	CodeStorage               = "STORAGE_ERROR"
	CodeMount                 = "MOUNT_ERROR"
	CodeConfig                = "CONFIG_ERROR"
	CodeCommandFailed         = "COMMAND_FAILED"
	CodeKVMUnavailable        = "KVM_UNAVAILABLE"
	CodeSmolvm                = "SMOLVM_ERROR"
	CodeCancelled             = "CANCELLED"
	CodeTimeout               = "TIMEOUT"
	CodeNotSupported          = "NOT_SUPPORTED"
	CodeInvalidConfig         = "INVALID_CONFIG"
	CodeInternal              = "INTERNAL"
	CodeUnauthorized          = "UNAUTHORIZED"
	// CodeConnection: a cloud request did not reach the control plane.
	CodeConnection = "CONNECTION"
)

// Error is every error the SDK returns: a stable Code plus a message. Match a
// code with errors.As, or compare against the sentinels with errors.Is:
//
//	if errors.Is(err, smol.ErrNotFound) { ... }
type Error struct {
	Code    string
	Message string
	// Status is the HTTP status of a failed cloud request; 0 otherwise.
	Status int
}

func (e *Error) Error() string { return fmt.Sprintf("[%s] %s", e.Code, e.Message) }

// Is reports a match on Code, so errors.Is(err, smol.ErrNotFound) holds for
// any not-found error whatever its message.
func (e *Error) Is(target error) bool {
	var t *Error
	return errors.As(target, &t) && t.Message == "" && t.Code == e.Code
}

// Sentinels for errors.Is. Each matches every *Error with its code.
var (
	ErrNotFound              = &Error{Code: CodeNotFound}
	ErrInvalidState          = &Error{Code: CodeInvalidState}
	ErrHypervisorUnavailable = &Error{Code: CodeHypervisorUnavailable}
	ErrConflict              = &Error{Code: CodeConflict}
	ErrKVMUnavailable        = &Error{Code: CodeKVMUnavailable}
	ErrCancelled             = &Error{Code: CodeCancelled}
	ErrTimeout               = &Error{Code: CodeTimeout}
	ErrUnauthorized          = &Error{Code: CodeUnauthorized}
	// ErrNotSupported: the selected target cannot do this (for example usage
	// metering on the local target, or host mounts on the cloud).
	ErrNotSupported = &Error{Code: CodeNotSupported}
	// ErrInvalidConfig: a required value is missing or invalid.
	ErrInvalidConfig = &Error{Code: CodeInvalidConfig}
)

func notSupported(message string) error {
	return &Error{Code: CodeNotSupported, Message: message}
}

func invalidConfig(message string) error {
	return &Error{Code: CodeInvalidConfig, Message: message}
}

// ExecutionError is returned by ExecResult.AssertSuccess for a command that
// ran but exited non-zero. errors.Is(err, smol.ErrCommandFailed) matches it.
type ExecutionError struct {
	ExitCode int
	Stdout   string
	Stderr   string
}

func (e *ExecutionError) Error() string {
	return fmt.Sprintf("[%s] Command exited with code %d", CodeCommandFailed, e.ExitCode)
}

// Is matches ErrCommandFailed.
func (e *ExecutionError) Is(target error) bool { return target == ErrCommandFailed }

// ErrCommandFailed matches every ExecutionError.
var ErrCommandFailed = &Error{Code: CodeCommandFailed}
