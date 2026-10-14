//go:build unix

package smol

import (
	"os"
	"os/signal"
	"syscall"
)

// installSignalCleanup stops owned machines on the first SIGINT or SIGTERM,
// then re-raises it. signal.Stop on this package's own channel (never
// signal.Reset, which would drop the program's handlers too) hands the signal
// back to the program's handler or the default action, so the exit status is
// the one the signal would have given.
func installSignalCleanup() {
	ch := make(chan os.Signal, 1)
	signal.Notify(ch, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		sig := <-ch
		signal.Stop(ch)
		stopAllLocal()
		_ = syscall.Kill(os.Getpid(), sig.(syscall.Signal))
	}()
}
