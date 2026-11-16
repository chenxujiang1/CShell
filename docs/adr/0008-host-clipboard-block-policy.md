# ADR-0008: Persistent host clipboard restrictions

- Status: Accepted
- Date: 2026-09-30

## Decision

- Extend ADR-0007 with a persistent host block list. Every unblocked host still requires confirmation for each clipboard write; remote reads remain prohibited. Automatic persistent grants are outside this change.
- Bind the restriction to a canonical host and port captured from the saved SSH launch plan, independent of later Profile edits. Host names are ASCII case insensitive with a trailing DNS dot removed. Ports remain distinct; all accounts and routes using the same endpoint share the restriction. Restriction removal returns to individual confirmation and never replays discarded output.
- Store a bounded, versioned policy document separately from Profile and workspace revisions. SQLite schema 8 adds a single policy row; v0-v7 migrations retain a consistent backup and commit schema changes in one transaction. At most 256 distinct hosts and 64 KiB encoded policy data are supported.
- IPC minor 14 negotiates HOST_CLIPBOARD_POLICY independently. Load and revision-checked Save provide stable unavailable, invalid and conflict outcomes. Clients never retry uncertain mutations. Policy responses may expose the selected session's captured endpoint; clipboard polling never obtains clipboard contents before approval.
- A daemon policy gate serializes policy saves with clipboard decisions and session registration. A successful save applies to every matching active SSH session before acknowledgement, clears pending requests for blocked endpoints and prevents further capture. Storage failure clears pending input and rejects clipboard approval until policy storage is readable. A process restart loads restrictions before a saved session is registered.
- The desktop safety panel lists restrictions, supports adding a target, blocking the selected SSH host, refreshing and removing a restriction. A stale revision must be refreshed before editing again. Clipboard data is never stored in this policy document, and diagnostic Debug output hides the host list.

## Validation

Cover canonicalization, duplicate/size/version rejection, CAS conflicts, schema 7 backup/restore, persistence across reopen, independent Profile/workspace revisions, live restriction and revocation, old-client capability refusal, and captured endpoint stability after Profile changes. Persistent automatic trust and the remaining Vault/diagnostic redaction acceptance remain P1-080 work.
