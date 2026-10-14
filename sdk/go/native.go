package smol

import (
	"encoding/json"
	"fmt"
	"runtime"
	"sync"
	"unsafe"
)

// abiVersion is the native core's smol_abi_version this package speaks. It
// changes only when an op's arguments or reply change incompatibly.
const abiVersion = 1

// The native core's four symbols. The core takes JSON in and gives JSON back,
// so new engine operations never add symbols here.
var (
	smolABIVersion func() uint32
	smolCall       func(op, args string, input unsafe.Pointer, inputLen uintptr, output *unsafe.Pointer, outputLen *uintptr) unsafe.Pointer
	smolFreeString func(s unsafe.Pointer)
	smolFreeBytes  func(p unsafe.Pointer, n uintptr)
)

var (
	nativeOnce sync.Once
	nativeErr  error
)

// loadNative resolves the runtime bundle, loads the native core from it and
// points the engine at the bundle's boot helper, libraries and guest rootfs.
// It runs on first local use only, so cloud-only programs never download or
// load the engine.
func loadNative() error {
	nativeOnce.Do(func() {
		nativeErr = openNative()
		if nativeErr != nil {
			nativeErr = &Error{Code: CodeHypervisorUnavailable, Message: "local engine unavailable: " + nativeErr.Error()}
		}
	})
	return nativeErr
}

func openNative() error {
	assets, err := resolveRuntimeAssets()
	if err != nil {
		return err
	}
	if err := bindNative(assets.library); err != nil {
		return err
	}
	if got := smolABIVersion(); got != abiVersion {
		return fmt.Errorf("%s implements native ABI %d, this package needs %d; "+
			"use the runtime bundle released with this SDK version", assets.library, got, abiVersion)
	}
	_, err = call("configure_runtime_assets", struct {
		BootBinary     string `json:"boot_binary,omitempty"`
		LibDir         string `json:"lib_dir,omitempty"`
		AgentRootfs    string `json:"agent_rootfs,omitempty"`
		AgentRootfsTar string `json:"agent_rootfs_tar,omitempty"`
	}{assets.bootBinary, assets.libDir, assets.agentRootfs, assets.agentRootfsTar})
	return err
}

// nativeReply is the core's envelope: exactly one of OK and Error is set.
type nativeReply struct {
	OK    json.RawMessage `json:"ok"`
	Error *struct {
		Code    string `json:"code"`
		Message string `json:"message"`
	} `json:"error"`
}

// call runs one native op and returns its JSON result.
func call(op string, args any) (json.RawMessage, error) {
	result, _, err := callBytes(op, args, nil, false)
	return result, err
}

// callBytes runs one native op, sending input as its raw byte payload and,
// when wantOutput is set, returning the raw bytes the op hands back.
func callBytes(op string, args any, input []byte, wantOutput bool) (json.RawMessage, []byte, error) {
	encoded := "{}"
	if args != nil {
		b, err := json.Marshal(args)
		if err != nil {
			return nil, nil, &Error{Code: CodeInvalidConfig, Message: "encode " + op + " arguments: " + err.Error()}
		}
		encoded = string(b)
	}
	var inPtr unsafe.Pointer
	if len(input) > 0 {
		inPtr = unsafe.Pointer(&input[0])
	}
	var outPtr unsafe.Pointer
	var outLen uintptr
	replyPtr := smolCall(op, encoded, inPtr, uintptr(len(input)), &outPtr, &outLen)
	runtime.KeepAlive(input)
	text := cString(replyPtr)
	smolFreeString(replyPtr)

	var output []byte
	if outPtr != nil {
		if wantOutput {
			output = make([]byte, outLen)
			copy(output, unsafe.Slice((*byte)(outPtr), outLen))
		}
		smolFreeBytes(outPtr, outLen)
	}

	var reply nativeReply
	if err := json.Unmarshal([]byte(text), &reply); err != nil {
		return nil, nil, &Error{Code: CodeSmolvm, Message: "malformed native reply to " + op + ": " + err.Error()}
	}
	if reply.Error != nil {
		return nil, nil, &Error{Code: reply.Error.Code, Message: reply.Error.Message}
	}
	return reply.OK, output, nil
}

// callInto runs one native op and decodes its result into out.
func callInto(op string, args, out any) error {
	raw, err := call(op, args)
	if err != nil {
		return err
	}
	if out == nil {
		return nil
	}
	if err := json.Unmarshal(raw, out); err != nil {
		return &Error{Code: CodeSmolvm, Message: "decode " + op + " result: " + err.Error()}
	}
	return nil
}

// cString copies a NUL-terminated C string the core returned.
func cString(p unsafe.Pointer) string {
	if p == nil {
		return ""
	}
	n := 0
	for *(*byte)(unsafe.Add(p, n)) != 0 {
		n++
	}
	return string(unsafe.Slice((*byte)(p), n))
}
