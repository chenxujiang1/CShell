# ADR-0017: Standalone OpenSSH config export

- Status: Accepted
- Date: 2026-09-30

Update 2026-10-12: [ADR-0052](0052-bounded-multi-hop-ssh-routes.md) supersedes the
original single-hop catalog restriction below. Existing dependency closure and
per-record ProxyJump aliases now support validated chains of at most eight jumps.
SOCKS5/HTTP export restrictions remain; the extension awaits unified verification.

## Scope and selection

P1-090 adds a public OpenSSH config export from the saved Profile catalog. The desktop selects saved SSH targets and captures the catalog revision; the daemon checks the same revision from a consistent repository snapshot. Include required jump targets automatically, with at most 512 hosts including dependencies and a 1 MiB file. Reject duplicate, missing or non-SSH selections. An empty API selection means all configured SSH targets; the desktop requires an explicit nonempty selection.

Each alias is `cshell-<32 lowercase UUID hex digits>`. Profile names appear only as JSON-quoted comments, so aliases remain stable despite duplicate names in separate folders, renaming, spaces or punctuation. Sort hosts by UUID for deterministic output. Local programs, arguments, environment, terminal settings, Theme data, credential-store policy, Vault secrets, private-key contents and trust records are excluded. Export never reads key files, changes the catalog or invokes an SSH client.

Use the exported file explicitly: `ssh -F <file> <alias>`. It is an independent configuration, not a fragment to append to an existing config. OpenSSH's alternate-file and configuration evaluation behavior is described in [ssh(1)](https://man.openbsd.org/ssh); the formatter's quoting, identity and authentication options follow [ssh_config(5)](https://man.openbsd.org/ssh_config).

## Authentication, routes and quoting

- Password: only password authentication, with agent and file identities disabled.
- Private key: only public-key authentication, the selected file reference, `IdentitiesOnly yes`, agent disabled, and certificate algorithms excluded with `PubkeyAcceptedKeyTypes -*cert*`. This also excludes automatic adjacent certificate discovery.
- Certificate: only public-key authentication, the selected private key and certificate references, agent disabled, and only `*-cert-v01@openssh.com` algorithms. Do not offer a raw key as fallback.
- Agent: public-key authentication using the OpenSSH agent, without default file identities. Fingerprint-filtered and Pageant targets are unrepresentable. Windows Auto also falls back to Pageant in CShell, so it must be changed explicitly to OpenSSH before export; Unix Auto uses the OpenSSH agent and is supported.

Use the older, still-supported `PubkeyAcceptedKeyTypes` spelling for client compatibility. This is a required config subset rather than a guarantee for every historical SSH implementation. `StrictHostKeyChecking yes`, `UpdateHostKeys no`, `CheckHostIP no` and `AddKeysToAgent no` keep trust and agent mutation outside this transfer. The recipient must supply its own known_hosts and any passwords/passphrases; the exported config does not use CShell's Vault or trust database.

Support Direct and one saved jump target, exported as its stable `ProxyJump` alias. Refuse SOCKS5/HTTP routes for the whole selected batch; never silently substitute Direct. Unselected unsupported targets do not block a valid selection. Validate the complete saved catalog first, including the existing no-cycle/no-multiple-hop constraints.

Accept bounded, representable host/user values and absolute key references on the current platform. Reject environment substitutions in key references. Quote values and escape percent tokens, quotes and backslashes; keep metacharacters out of aliases and host/user fields. Referenced key files must be supplied separately. Paths are not rewritten for a different OS.

## IPC and publication

IPC minor 21 advertises `OPENSSH_EXPORT` bit 25. `ProfileOperation::ExportOpenSsh = 15` uses repeated 16-byte `export_profile_ids` at request tag 11; response tag 8 `export_json` carries config bytes according to the operation. Require both PROFILE_CONTROL and OPENSSH_EXPORT, without unrelated SSH creation, LocalProfile or import capabilities. Reject selections on other operations and reject local-context, mutation, import, credential and host-key fields on export.

The desktop computes the expected deterministic bytes from its captured saved catalog, then checks status, revision and exact returned bytes before publication. Publish a synced temporary file without overwriting the destination, including races with another exporter. Any failure or uncertain outcome requires checking the destination and a fresh explicit action. This preserves the existing public export contract in ADR-0014; it does not add a Windows power-loss guarantee.

## Validation and import compatibility

The bounded OpenSSH importer now accepts the exact safe identity/algorithm/trust options produced by this exporter; unrelated values remain rejected. Agent targets explicitly select the OpenSSH backend rather than Windows Auto/Pageant fallback. Its existing input, line and evaluation limits remain in force, so large external-client exports are not a promise of same-size import round trips. Public Profile JSON remains the complete catalog transfer format.

Tests cover selection/dependency closure, deterministic order, authentication round trips, path escaping, bounds, unsupported-target refusal, framed IPC capability/revision/field checks, no catalog mutation across repository reopening, actual egui pointer selection, stale selection removal, response verification and no-overwrite publication. An actual `ssh -G -F` oracle checks all four authentication modes and the jump alias, including paths with spaces, comments and literal percent characters. CI quality and four native platform jobs require this oracle; absence of `ssh` is then a failure. Local environments may skip that oracle only when the executable is absent and `CSHELL_REQUIRE_OPENSSH_EXPORT_ORACLE` is unset.

This oracle evaluates configuration without opening a network connection. It does not establish recipient key permissions, server authentication, manual desktop appearance, four-platform results for an unpushed commit, or the complete P1-099 acceptance gate.
