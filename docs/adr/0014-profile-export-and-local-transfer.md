# ADR-0014: Profile export and LocalConnection transfer

- Status: Accepted
- Date: 2026-09-30

## Public contract

Extend `cshell.profile-catalog` with version 3 and an optional `local_connections` array. Versions 1 and 2 retain their existing contracts and reject nonempty local records. Keep the 1 MiB file limit and 2048 combined folder/Profile/SSH/local record limit, strict unknown-field rejection and full catalog validation.

A local record contains `profile_id`, `program`, `cwd`, `close_policy` and optional `args` / `env_overrides`. Omitted or null context fields preserve the corresponding existing field on Replace; for a new target they become empty. Explicit empty arrays/maps clear those fields. Skip keeps the complete existing configuration. Remap target UUIDs through the same Profile map used by SSH targets. Kind replacement removes the incompatible old connection in the same validated SQLite transaction. Import does not start programs, expand arguments or environment values, read key files, contact hosts, change trust or access the Vault.

Preview shows the local program, working directory, close policy, counts and omission behavior. Argument and environment contents never appear in the preview or transfer-record Debug output. All existing local validation limits and reserved `CSHELL_` environment restrictions apply, including to the final merged configuration.

## Export

Export the authoritative saved daemon catalog at the revision captured by the desktop button. Reject a stale revision and mutation/credential fields. Export includes folders, Profiles, SSH public targets and local program configuration. The application export API cannot access the Vault or system keychain. Passwords, passphrases, imported private-key bytes, credential-store policy, host-key trust and runtime session state are excluded. SSH key/certificate paths and Theme names are references.

Default export omits local arguments and environment overrides, because arbitrary user values may contain secrets. A separate checkbox explicitly includes both as plaintext; it is off by default and explains this behavior. This public document is not an encrypted credential package. Freeze missing root folder/Profile terminal overrides from the current global defaults so a fresh import retains effective settings without changing the recipient's global defaults. Internal folder inheritance is retained; choosing Skip/Replace against existing folders follows the usual conflict policy.

The desktop validates the returned bounded v3 document and revision before saving. Require an absolute destination with an existing parent directory. Write and sync a randomly named temporary file in an owned staging directory on the destination filesystem, then publish with `persist_noclobber`. Never overwrite an existing destination, including a concurrent winner. Clean the owned staging directory, including any remaining temporary hard link. On Unix also sync the destination parent. Errors do not include paths or document contents; uncertain publication/sync/cleanup results tell the user to inspect the destination before retrying. No automatic retry occurs. The no-clobber operation is not guaranteed universally atomic by tempfile; do not claim cross-platform power-loss durability or atomic publication on every filesystem.

## IPC and verification

IPC minor 18 adds PROFILE_TRANSFER bit 22, ExportCatalog operation 11, request `include_local_context` tag 10 and response `export_json` tag 8. Export and v3 PreviewImport/CommitImport require the new capability. V3 import also requires the existing connection import and SSH/local capabilities; v1/v2 remain compatible without bit 22. Debug output hides export bytes and the desktop command's destination. No schema migration is needed.

Tests cover default omission and explicit inclusion, inherited terminal settings, fresh round trips, remapped IDs, retained omitted fields, explicit clearing, Skip, both directions of kind replacement, invalid local records and legacy version refusal. Real framed IPC/SQLite tests cover stale export revision, capabilities, forbidden options, mixed SSH/local round trips, injected local insert failure rollback and reopen persistence. Pointer-driven UI tests cover the default checkbox, opt-in, revision capture and disabled export. File tests cover complete concurrent publication, no overwrite and staging cleanup.

Theme payload import/export is subsequently added by [ADR-0015](0015-terminal-theme-transfer.md). OpenSSH export, independently password-encrypted credential packages, current-revision native CI and user visual acceptance remain separate work.

## Reference

- [tempfile NamedTempFile documentation](https://docs.rs/tempfile/latest/tempfile/struct.NamedTempFile.html): persistence does not sync automatically; no-clobber persistence never overwrites and may leave a temporary hard link.
