# ADR-0013: Bounded OpenSSH Profile import

- Status: Accepted
- Date: 2026-09-30

Update 2026-10-12: [ADR-0052](0052-bounded-multi-hop-ssh-routes.md) extends the
original direct-jump restriction below to linked explicit aliases, bounded to eight
jumps without cycles. Comma-separated and inline-address ProxyJump forms still fail.
Verification remains deferred for this extension.

## Public document and transaction

Keep `cshell.profile-catalog` version 1 as metadata-only. Version 2 adds an optional `ssh_connections` array of the existing public, secret-free SSH target records. Reject version 1 with nonempty connections, unknown fields and invalid input graphs. Bound files to 1 MiB and the sum of folders, Profiles and targets to 2048. The initial slice does not add LocalConnection or export; [ADR-0014](0014-profile-export-and-local-transfer.md) subsequently adds v3 local transfer and Profile export. Theme payloads remain separate.

Map imported UUIDs onto retained catalog UUIDs with the existing folder/name conflict policy. Create and Replace update the target; Skip preserves its existing target. Remap jump references through the same Profile map, including skipped jump Profiles. Version 2 kind replacement removes the previous connection record in the same transaction. Validate the combined catalog: incompatible retained jump routes cause rejection. Version 1 preserves existing target behavior and rejects kind replacement when it would leave an incompatible connection, so older capabilities cannot remove a LocalConnection indirectly.

Preview shows account, endpoint, authentication method, key/certificate references and route; skipped items show the retained configuration. The GUI checks file, format, conflict policy, default username and catalog revision before commit. Cache converted JSON bytes so source-file changes after preview cannot alter the commit. Any commit attempt consumes the cache; uncertain results require a fresh preview. Recheck the revision and commit all changes in one SQLite transaction.

## OpenSSH subset

The pure parser accepts UTF-8. Explicit positive literal Host aliases become Profiles. Wildcards `*`/`?` and negated patterns supply settings for those aliases without inventing hosts. Evaluate matching sections in source order, retaining the first scalar setting. IdentityFile and CertificateFile are additive in OpenSSH: refuse multiple distinct effective entries. Support quotes, comments, case-insensitive directive names, basic quote/backslash escapes and the optional equals separator.

Supported directives: Host, HostName, User, Port, IdentityFile, CertificateFile, ProxyJump, StrictHostKeyChecking, IdentitiesOnly and PreferredAuthentications. ProxyJump must name one explicit imported alias with a direct route; reject chains, self references and missing aliases. StrictHostKeyChecking accepts only yes/ask; IdentitiesOnly accepts only yes. PreferredAuthentications may select only publickey or password. Otherwise a certificate/key reference selects that single method, or the agent when absent. This imports one CShell authentication method and does not reproduce OpenSSH's default sequential key/password fallback.

Use the displayed desktop default username when User is absent. HostName supports `%h` and `%%`; key references support `%d`, `%h`, `%n`, `%r`, `%p` and `%%`. Expand `~/` using the desktop user's home directory and require absolute resulting paths on the current platform. Reject other users' tilde paths, environment expansion, relative paths and unsupported tokens. Import only file references, never key contents.

Every unsupported directive stops the whole import, including Include, Match, ProxyCommand, RemoteCommand and LocalCommand. Parsing/importing never launches a shell, evaluates Match exec, opens included files or keys, connects to hosts, changes known_hosts or accesses credentials. Errors reveal a fixed category and optional line number, not input values. Limits: 512 aliases, 4096 lines, 8192 bytes per line, 64 MiB input-by-alias estimate and 16 Mi wildcard matching steps. Read an opened regular file with a bounded reader to handle concurrent growth.

## IPC and verification

IPC minor 17 adds PROFILE_CONNECTION_IMPORT bit 21 and ProfileImportItemData details tag 4. Version 2 PreviewImport/CommitImport require it plus SSH target/auth/route and LocalProfile capabilities (kind replacement may remove a LocalConnection). Version 1 remains available without the new capability. The desktop refuses version 2 against older daemons before sending it. No new envelope or database migration is needed.

Tests cover ordered defaults, negation, quoting, Windows separators, Unicode key references, certificate tokens, trust/authentication refusal, context and work limits. Framed daemon IPC and SQLite tests cover capability refusal, no-write preview, Skip/Replace jump mapping, stale revision, transaction failure rollback and reopen persistence. Egui pointer tests cover source capture, stale revision and changed settings. File conversion tests exclude source comments from IPC and preserve converted bytes after source-file edits. Full OpenSSH compatibility, user-server and subjective visual acceptance remain separate.

## References

- [OpenSSH ssh_config manual](https://man.openbsd.org/ssh_config)
- [OpenSSH portable argument parsing](https://github.com/openssh/openssh-portable/blob/master/misc.c)
