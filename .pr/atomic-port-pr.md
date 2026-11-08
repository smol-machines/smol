# feat(python): expose atomic local loopback port publication

Add `Machine.publish_atomic(guest)` and its async counterpart. The local engine
allocates and retains a loopback listener, returns its host port, and records
the mapping used by `endpoint()`, `ports()` and deletion. Cloud calls and invalid
guest ports fail before allocation. An older native extension reports the
missing prerequisite explicitly.

Requires the companion smolvm engine contribution that adds atomic publication
to the shared TCP acceptor and running-machine control socket. Existing
`publish()` behavior is preserved.

Validation: nine Python contract tests passed. The opt-in real-VM test passed
with a local native/helper build, published 1.32.4 runtime libraries/rootfs and
Alpine 3.20, with outbound networking disabled. Both allocated ports served HTTP
and stayed bound; deletion removed the machine. Native type checking and a
macOS extension build passed. Two existing credential-result warnings come from
building this SDK checkout against the newer engine checkout.

A coordinated published wheel and Linux live validation remain pending. The
OpenHands provider must retain its release gate until that wheel passes its
image and conversation tests. See [ATOMIC_PORTS.md](../sdk/python/ATOMIC_PORTS.md)
for the API and commands.
