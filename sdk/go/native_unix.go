//go:build darwin || linux

package smol

import (
	"fmt"

	"github.com/ebitengine/purego"
)

// bindNative loads the native core and binds its symbols. purego calls C
// without cgo, so programs using this package still build with CGO_ENABLED=0.
func bindNative(path string) error {
	lib, err := purego.Dlopen(path, purego.RTLD_NOW|purego.RTLD_LOCAL)
	if err != nil {
		return fmt.Errorf("load %s: %w", path, err)
	}
	purego.RegisterLibFunc(&smolABIVersion, lib, "smol_abi_version")
	purego.RegisterLibFunc(&smolCall, lib, "smol_call")
	purego.RegisterLibFunc(&smolFreeString, lib, "smol_free_string")
	purego.RegisterLibFunc(&smolFreeBytes, lib, "smol_free_bytes")
	return nil
}
