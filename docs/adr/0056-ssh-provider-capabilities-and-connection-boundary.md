# ADR-0056: SSH provider requirements and connection boundary

- Status: Accepted (code written; verification deferred)
- Date: 2026-10-13

## Scope

Complete the P3-020 provider capability extension with typed, backend-independent
requirements, truthful registration, deterministic preflight and project-owned
connection operations. No PoC has justified an additional native SSH backend.
System OpenSSH and libssh are identifiable future adapters, not registered or
launched by this change. Their implementation requires a separate accepted ADR.
Compilation, tests, MSRV, audit, GUI and real SSH verification remain deferred
under the user's instruction. Only source editing and cargo fmt are performed.

## Requirements and selection

The domain owns SshProviderId, SshAlgorithmPolicy, 23 SshCapability values and
SshProviderRequirements. They contain no host names, paths, commands, credentials
or process arguments. Requirements include an optional explicit provider,
capabilities, jump count and algorithm policy. Jump/forward/agent/certificate
dependencies are expanded before matching. Automatic selection follows the
registration order; explicit selection never substitutes another provider.

The application owns the version-1 cshell.ssh-providers catalog, descriptors,
evaluation and bounded report codec. Reports contain the original requirements,
selected provider and typed refusals for unregistered providers, missing
capabilities, unsupported algorithms and excessive jump depth. Clients validate
catalog invariants and independently recompute the exact scoped evaluation.
Request JSON is limited to 4 KiB and report JSON to 32 KiB; unknown fields and
unknown typed values are rejected. Negative evaluation is a successful query,
not an IPC or authentication failure.

## Registered capability truth

Only russh is registered. Its actual implementation supplies password/public-key
authentication, keyboard interactive, user and host certificates, PTY, streaming
exec, SFTP, direct-tcpip, up to eight jump hosts, L/R/D forwards and SOCKS5/HTTP
CONNECT proxy transports. OpenSSH agent is advertised on Unix and Windows;
Pageant only on Windows. An agent capability does not prove that a service,
identity or external signer is currently available.

GSSAPI, native FIDO2, native PKCS#11, ProxyCommand and X11 forwarding are absent.
Signing by an external agent does not advertise native hardware middleware.
The existing x11-forwarding feature is a reservation and no longer claims an
implemented X11 operation. The older boolean capability view stays available,
with platform agent availability and X11 corrected. Only the Modern algorithm
policy is registered; Compatible/Legacy are rejected rather than silently using
Modern. Enabling legacy-crypto builds cryptographic primitives, not an accepted
connection policy. The existing AlgorithmPolicy name aliases the domain type.
No existing persisted Profile field or frozen snapshot is changed.

## Runtime boundary

SshProvider is object safe and supplies owned-stream connect and unauthenticated
scan futures. Selection returns a project-owned provider trait object.
Interactive authentication accepts a caller-owned async challenge responder,
sharing the existing bounded prompt/round validation with the adapter PoC API;
no new saved Profile authentication kind or credential UI is introduced.
SshConnection owns its backend privately and exposes typed project operations:
verified fingerprint, PTY, exec, SFTP, boxed direct streams, L/R/D forwarding,
original-transport remote cancellation and disconnect/closure proof. There is no
Deref or raw russh handle exposed through this connection. Capability checks use
the connection's retained adapter descriptor, not a newly selected backend.
Adapter-specific
library APIs remain available for existing PoC tests and are not production
daemon dependencies.

The daemon validates the complete frozen jump graph and combines leaf and hop
authentication/proxy requirements before reading credentials or opening sockets.
One selected provider handles the whole route, including scan and leaf; it is
not reselected after a failure. Interactive sessions, command/upload/download
executors and independent tunnels hold SshConnection. Trust, CA/revocation/pins,
authentication generations, task dispatch/cancellation, owned-hop cleanup,
native ACK and dedicated recovery constraints are retained by the same paths.
Channel methods reject unsupported capabilities before dispatching the operation.
No implicit backend switch, command replay, policy downgrade or executable search
occurs. A capability expresses client implementation, not server authorization
or successful connectivity.

## Discovery surfaces

Protocol minor 50 adds SSH_PROVIDER_CONTROL bit 54 and envelope tags 53/54.
An empty request reads the catalog; a nonempty typed request evaluates it.
Both are read-only and require the authenticated local IPC handshake and feature
negotiation. No older-daemon fallback is used. The daemon evaluates synchronously
without storage, vault access, network or executable discovery.

CLI providers get/check and the desktop SSH capability window use 20-second
queries, bounded typed replies, request identity and input/decision binding.
CLI prints negative evaluations and exits unsuccessfully. The desktop provides
capability requirements, explicit/automatic backend selection, hop count and
algorithm choices; edits make the prior evaluation stale. Its worker is bounded
and cancellation-aware. Terminal/IME/search/accessibility/synchronized input is
guarded while this window is open.

## Integration and remaining verification

cshell-ssh now depends on the existing domain/application contracts. Cargo.lock
records these workspace edges only; no dependency versions or database schema
are changed. This completes the listed P3-020 functional code scope; no gate or
acceptance claim is made.

The unified verification batch must cover object safety and Send futures;
platform agent declarations; truthful X11/legacy flags; all capability
dependencies; explicit unregistered selection and automatic ordering; depth and
route/auth preflight without I/O; exact negative/positive report binding; old
protocol behavior and malformed/bounded documents; GUI input isolation and
shutdown; retained host trust/authentication/forward lifecycle; and real direct,
multi-hop, task, SFTP and dedicated-tunnel regressions on the supported platforms.
