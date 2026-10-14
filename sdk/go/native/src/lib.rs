//! Native (C ABI) core for the smol Go SDK — embeds the smolvm engine.
//!
//! Go analogue of the sibling `smol-node` (NAPI) and `smol-py` (pyo3) crates.
//! It drives the same synchronous `smolvm::embedded` runtime; Go calls it from
//! a goroutine, so a blocking call parks one OS thread, not the program.
//!
//! The whole surface is one entry point, `smol_call(op, args, ...)`, taking
//! and returning JSON, plus a raw byte side channel for file contents. A new
//! engine operation is a new `op`, never a new exported symbol, so the Go
//! package and a downloaded library can only disagree about `smol_abi_version`.
//!
//! Every reply is `{"ok": <value>}` or `{"error": {"code", "message"}}`, with
//! the same codes smol-node and smol-py raise, so the Go `*smol.Error` matches
//! the other SDKs code for code.

mod ops;

use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};

use serde_json::{json, Value};
use smolvm::error::{AgentErrorKind, Error as SmolvmError};

/// Bumped whenever an op's arguments or reply change incompatibly. The Go
/// package refuses a library whose version differs from the one it expects.
pub const ABI_VERSION: u32 = 1;

pub(crate) const NOT_FOUND: &str = "NOT_FOUND";
pub(crate) const INVALID_STATE: &str = "INVALID_STATE";
pub(crate) const HYPERVISOR_UNAVAILABLE: &str = "HYPERVISOR_UNAVAILABLE";
pub(crate) const CONFLICT: &str = "CONFLICT";
pub(crate) const FORBIDDEN: &str = "FORBIDDEN";
pub(crate) const STORAGE_ERROR: &str = "STORAGE_ERROR";
pub(crate) const MOUNT_ERROR: &str = "MOUNT_ERROR";
pub(crate) const CONFIG_ERROR: &str = "CONFIG_ERROR";
pub(crate) const COMMAND_FAILED: &str = "COMMAND_FAILED";
pub(crate) const KVM_UNAVAILABLE: &str = "KVM_UNAVAILABLE";
pub(crate) const SMOLVM_ERROR: &str = "SMOLVM_ERROR";
pub(crate) const CANCELLED: &str = "CANCELLED";
pub(crate) const INTERNAL: &str = "INTERNAL";

/// An error on its way back to Go: an SDK code plus a message.
#[derive(Debug)]
pub(crate) struct CallError {
    pub code: &'static str,
    pub message: String,
}

impl CallError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(CONFIG_ERROR, message)
    }
}

impl From<SmolvmError> for CallError {
    fn from(e: SmolvmError) -> Self {
        let (code, message) = code_and_message(&e);
        Self { code, message }
    }
}

impl From<serde_json::Error> for CallError {
    fn from(e: serde_json::Error) -> Self {
        Self::config(format!("invalid arguments: {e}"))
    }
}

pub(crate) type CallResult = Result<Value, CallError>;

/// The SDK error code and message for an engine error. Identical to
/// smol-node's `code_and_message` and smol-py's, so all SDKs agree.
pub(crate) fn code_and_message(e: &SmolvmError) -> (&'static str, String) {
    match e {
        SmolvmError::VmNotFound { name } => (NOT_FOUND, format!("VM not found: {name}")),
        SmolvmError::InvalidState { expected, actual } => (
            INVALID_STATE,
            format!("Invalid state: expected {expected}, got {actual}"),
        ),
        SmolvmError::HypervisorUnavailable(reason) => (
            HYPERVISOR_UNAVAILABLE,
            format!("Hypervisor unavailable: {reason}"),
        ),
        SmolvmError::Agent {
            operation,
            reason,
            kind,
        } => {
            let code = match kind {
                AgentErrorKind::NotFound => NOT_FOUND,
                AgentErrorKind::Conflict => CONFLICT,
                AgentErrorKind::Forbidden => FORBIDDEN,
                AgentErrorKind::Other => SMOLVM_ERROR,
            };
            (code, format!("Agent error ({operation}): {reason}"))
        }
        SmolvmError::RootfsNotFound { path } => {
            (NOT_FOUND, format!("Rootfs not found: {}", path.display()))
        }
        SmolvmError::DiskNotFound { path } => {
            (NOT_FOUND, format!("Disk not found: {}", path.display()))
        }
        SmolvmError::MountSourceNotFound { path } => (
            NOT_FOUND,
            format!("Mount source not found: {}", path.display()),
        ),
        SmolvmError::Storage { operation, reason } => {
            (STORAGE_ERROR, format!("Storage ({operation}): {reason}"))
        }
        SmolvmError::Mount { operation, reason } => {
            (MOUNT_ERROR, format!("Mount ({operation}): {reason}"))
        }
        SmolvmError::InvalidMountPath { reason } => {
            (MOUNT_ERROR, format!("Invalid mount path: {reason}"))
        }
        SmolvmError::Config { operation, reason } => {
            (CONFIG_ERROR, format!("Config ({operation}): {reason}"))
        }
        SmolvmError::CommandFailed { command, reason } => (
            COMMAND_FAILED,
            format!("Command '{command}' failed: {reason}"),
        ),
        SmolvmError::KvmUnavailable(reason) => {
            (KVM_UNAVAILABLE, format!("KVM unavailable: {reason}"))
        }
        SmolvmError::KvmPermission(reason) => {
            (KVM_UNAVAILABLE, format!("KVM permission denied: {reason}"))
        }
        _ => (SMOLVM_ERROR, e.to_string()),
    }
}

/// Bytes a call hands back beside its JSON reply (a file's contents).
pub(crate) struct Payload {
    pub input: Vec<u8>,
    pub output: Option<Vec<u8>>,
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

fn reply(result: CallResult) -> CString {
    let body = match result {
        Ok(value) => json!({ "ok": value }),
        Err(e) => json!({ "error": { "code": e.code, "message": e.message } }),
    };
    // serde_json never emits an interior NUL: a NUL inside a string is escaped.
    CString::new(body.to_string()).expect("JSON has no interior NUL")
}

/// The ABI version this library implements. See [`ABI_VERSION`].
#[no_mangle]
pub extern "C" fn smol_abi_version() -> u32 {
    ABI_VERSION
}

/// Run one operation.
///
/// `op` and `args` are NUL-terminated UTF-8; `args` is a JSON object (NULL
/// means `{}`). `input`/`input_len` carry raw bytes for ops that take them
/// (`write_file`) and may be NULL/0. For ops that return raw bytes
/// (`read_file`) the library stores a buffer in `*output`/`*output_len`, to be
/// released with `smol_free_bytes`; otherwise they are set to NULL/0.
///
/// Returns a NUL-terminated JSON reply, to be released with `smol_free_string`.
///
/// # Safety
/// Every pointer must be NULL or valid as described above for the call.
#[no_mangle]
pub unsafe extern "C" fn smol_call(
    op: *const c_char,
    args: *const c_char,
    input: *const u8,
    input_len: usize,
    output: *mut *mut u8,
    output_len: *mut usize,
) -> *mut c_char {
    if !output.is_null() {
        *output = std::ptr::null_mut();
    }
    if !output_len.is_null() {
        *output_len = 0;
    }
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        if op.is_null() {
            return (Err(CallError::config("op is required")), None);
        }
        let op = match CStr::from_ptr(op).to_str() {
            Ok(op) => op,
            Err(_) => return (Err(CallError::config("op is not UTF-8")), None),
        };
        let args: Value = if args.is_null() {
            Value::Object(Default::default())
        } else {
            match CStr::from_ptr(args).to_str() {
                Ok(text) => match serde_json::from_str(text) {
                    Ok(v) => v,
                    Err(e) => return (Err(e.into()), None),
                },
                Err(_) => return (Err(CallError::config("args are not UTF-8")), None),
            }
        };
        let mut payload = Payload {
            input: if input.is_null() || input_len == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(input, input_len).to_vec()
            },
            output: None,
        };
        let result = ops::dispatch(op, args, &mut payload);
        (result, payload.output)
    }));
    let (result, bytes) = match outcome {
        Ok(pair) => pair,
        Err(panic) => (
            Err(CallError::new(
                INTERNAL,
                format!("smol native core panicked: {}", panic_message(panic)),
            )),
            None,
        ),
    };
    if let Some(bytes) = bytes {
        if !output.is_null() && !output_len.is_null() {
            let mut boxed = bytes.into_boxed_slice();
            *output_len = boxed.len();
            *output = boxed.as_mut_ptr();
            std::mem::forget(boxed);
        }
    }
    reply(result).into_raw()
}

/// Release a reply returned by `smol_call`.
///
/// # Safety
/// `s` must be NULL or a pointer `smol_call` returned, released only once.
#[no_mangle]
pub unsafe extern "C" fn smol_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

/// Release a byte buffer `smol_call` stored in `*output`.
///
/// # Safety
/// `ptr`/`len` must be NULL/0 or exactly what `smol_call` stored, released once.
#[no_mangle]
pub unsafe extern "C" fn smol_free_bytes(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
    }
}
