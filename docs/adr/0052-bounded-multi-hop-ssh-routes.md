# ADR-0052: Bounded multi-hop SSH routes

- Status: Accepted (code written; verification deferred)
- Date: 2026-10-12

## Scope

Begin P3-020 with multi-hop execution for interactive SSH, host-key preview, command
tasks, uploads/downloads and mixed-task leaf executors. Tunnel management, CA
management and Provider capability extensions remain separate work. Existing
certificate/known_hosts CA support does not complete a CA management workflow.
Compilation, tests, audits, native UI and real transfers remain deferred at the
user's request. Only source review and cargo fmt run in this slice.

## Resolution and frozen plans

Keep SshRoute::Jump { profile_id }. Jump Profiles may refer to other jump Profiles.
The shared domain ssh_jump_chain resolver returns outermost-first records, rejects
self/cyclic/missing/ambiguous references and permits at most eight jumps, excluding
the final target. The outermost hop can use Direct, SOCKS5 or HTTP CONNECT; later
hops use direct-tcpip streams from their predecessor. Arbitrary ProxyCommand,
credential fallback and agent forwarding are not introduced.

Catalog validation checks every route before accepting an atomic batch. Deleting a
required Profile still rejects the batch. Existing frozen host documents already
keep the full dependency closure; their catalog validation now accepts bounded
chains. The resource budget charges every intermediate SSH/proxy endpoint using
its existing atomic lease and shared depth constant. Production task/upload/download
executors build from frozen records, never the later live catalog. Destination
overrides retain the original credential template.

## Ownership and execution

SshConnectionPlan clones the ordered records. The daemon revalidates chain/order
before opening a transport. Every hop loads its own strict known_hosts verifier
and saved credentials, authenticates separately and opens the next stream. Hop SSH
connection/authentication and direct-channel opens are bounded to thirty seconds;
the existing TCP/proxy limits and caller cancellation/Vault handling remain.

An incremental guard owns each authenticated client immediately. Later hop/target
failure, cancellation and host-key preview completion drop or disconnect the guard.
Explicit disconnect and drop close clients inner-to-outer with a two-second allowance
per hop. Shared ownership keeps all transports alive for the leaf session/task.
No automatic reconnect/replay is added; disconnect does not infer remote outcomes.

## Trust, desktop and import/export

Clipboard/SFTP workspace trust captures each ordered hop's host/port/account/saved
route and actually verified fingerprint. Fingerprint count must match the plan.
Changing an outer hop cannot reuse another path's grant. Direct/single-hop identity
keeps its exact previous version-1 serialization; multi-hop uses a version-2 tuple.

The Profile editor allows nested candidates, shows outer-to-inner order and warns
about cycles/depth. Daemon validation remains authoritative. Confirm host keys from
the outermost hop inward; target preview authenticates preceding hops and scans
only the target. Import preview shows the complete named hop chain.

OpenSSH import accepts linked explicit aliases, each with one named ProxyJump. It
rejects missing/cyclic/over-depth chains, comma-separated lists, inline user@host:port
references and unsupported Include/Match/commands. Export already includes the
dependency closure and per-record aliases; it now accepts the validated chain.
SOCKS5/HTTP export remains unsupported.

Stored shapes, SQLite schema, wire messages, SSH_PROFILE_ROUTE capability, protocol
minor and dependency versions do not change. An old daemon still rejects nested
catalogs; clients do not silently flatten routes. Existing test names that actually
assert cycles/invalid aliases are corrected; this turn does not execute tests.

## Deferred verification

Unified verification must cover zero/one/eight/nine hops, cycles/removal, frozen
closure/old bytes, OpenSSH import/export semantics, every intermediate key/auth/
proxy/channel failure, timeouts and Vault/cancellation cleanup, target rejection/
preview teardown, resource accounting, fingerprint order and exact old identity,
GUI preview and actual multi-server SSH/SFTP on all supported platforms.
