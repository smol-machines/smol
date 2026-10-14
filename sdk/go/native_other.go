//go:build !(darwin || linux)

package smol

import (
	"fmt"
	"runtime"
)

func bindNative(string) error {
	return fmt.Errorf("local machines are not supported on %s; use the cloud target", runtime.GOOS)
}
