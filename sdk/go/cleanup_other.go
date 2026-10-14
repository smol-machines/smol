//go:build !unix

package smol

// Local machines run only on unix hosts, so there is nothing to clean up.
func installSignalCleanup() {}
