# ADR-0011: Portable master-password Vault backend

- Status: Accepted
- Date: 2026-09-30

## Decision

- Add a daemon-only blocking backend in cshell-vault. It stores password, private-key passphrase and imported private-key bytes using opaque Profile IDs and credential kinds. Bindings are encrypted alongside secrets and checked on reads. It does not automatically migrate native keychain entries or enable a desktop credential mode.
- Binary format v1 uses Argon2id v19 with a 16-byte random salt, 64 MiB memory, three passes and four lanes. Derive 64 bytes and split separate AES-256-GCM record and manifest keys. Reject all other version/KDF parameters before derivation. These parameters follow the memory-constrained profile in [RFC 9106](https://www.rfc-editor.org/rfc/rfc9106.html#section-4); output length is extended for key separation.
- Each record has a fresh OS-random 96-bit nonce. Record AAD includes the format version, Vault identity, Profile identity and credential kind. An encrypted manifest authenticates the complete header and all record ciphertext, detecting removal, insertion and swaps. Wrong passwords and authentication failures share one diagnostic. This format does not detect deliberate rollback of an entire valid file; that requires an external trusted revision anchor.
- Bound files to 1 MiB, records to 128, bindings to 4096 UTF-8 bytes, passwords/passphrases to 4096 bytes and private keys to 64 KiB. New master passwords must contain 8–1024 bytes; unlock accepts existing nonempty passwords up to 1024 bytes. Reject duplicates, unknown kinds, trailing bytes and oversized lengths before allocation/KDF work.
- Open starts locked. Lock, reload and failed unlock discard the derived keys. Keep only encrypted records between operations. Root keys, Argon2 working memory, cipher state and plaintext buffers are cleared on drop; plaintext buffers reserve their entire bounded size before writing. Debug/error/source interfaces expose no password, binding, path or backend text.
- Hold an OS advisory lock for the backend lifetime. Writes use a same-directory encrypted temporary file, sync its contents, atomically replace the destination and sync the parent directory on Unix. Creation never overwrites an existing Vault. Before-commit failure preserves the previous state; an uncertain post-commit result clears keys and requires explicit reload. Lock files remain on disk to prevent competing lock inodes. Callers must place Vault and lock files in the protected daemon data directory.
- Windows syncs the encrypted file before replacement; complete power-loss durability of directory metadata is not claimed by this backend. Cross-platform crash/power-loss acceptance remains a later gate.
- Master-password changes decrypt and re-encrypt every record with a fresh salt/key and record nonces in one replacement. There is no password recovery/reset, plaintext export or silent fallback to the native keychain. Existing readers already holding a Secret must drop it themselves; locking cannot revoke returned memory or established SSH sessions.

## Validation and remaining work

Exercise create/open/lock/unlock, wrong passwords, record binding/kind/identity isolation, manifest and record tampering, bounded hostile files, exclusive ownership, failed/uncertain writes, fresh encryption, encrypted private-key storage and master-password rotation/reopen. Run the full workspace gate and dependency review with the production KDF parameters.

The original storage slice is integrated by [ADR-0012](0012-vault-controls-and-clipboard-identities.md): daemon/IPC credential selection, validated private-key import, desktop controls, system-keychain root protection and a conservative focus/lease lock policy. Kind 4 adds encrypted Profile mode metadata; older readers reject that unknown kind safely. Related credential writes/removals use a single authenticated replacement. The desktop does not link this crate and IPC never carries private-key bytes or derived keys. Native OS lock-notification and crash/power-loss usage acceptance are not claimed by these automated backend tests.
