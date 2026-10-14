package smol

import (
	"context"
	"sync"
)

// Local machines this process owns are stopped if it is interrupted
// (SIGINT/SIGTERM) without an explicit Stop or Delete. The engine already
// reaps a VM moments after its process dies however it dies, so this is
// best-effort graceful teardown. Cloud machines are remote and deliberately
// outlive the process.
var (
	liveMu      sync.Mutex
	liveLocal   = map[*localTransport]struct{}{}
	cleanupOnce sync.Once
)

func trackLocal(t *localTransport) {
	liveMu.Lock()
	liveLocal[t] = struct{}{}
	liveMu.Unlock()
	cleanupOnce.Do(installSignalCleanup)
}

func untrackLocal(t *localTransport) {
	liveMu.Lock()
	delete(liveLocal, t)
	liveMu.Unlock()
}

func stopAllLocal() {
	liveMu.Lock()
	machines := make([]*localTransport, 0, len(liveLocal))
	for t := range liveLocal {
		machines = append(machines, t)
	}
	liveMu.Unlock()
	var wg sync.WaitGroup
	for _, t := range machines {
		wg.Add(1)
		go func() {
			defer wg.Done()
			_ = t.stop(context.Background())
		}()
	}
	wg.Wait()
}
