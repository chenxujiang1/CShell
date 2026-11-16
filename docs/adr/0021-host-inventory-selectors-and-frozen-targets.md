# ADR-0021: Host inventories, bounded selectors and frozen targets

- Status: Accepted
- Date: 2026-10-01

## Inventory and saved Profiles

P2-040 introduces a separate versioned Host catalog. A Host has a UUID, display name, address, positive port, exact tags, key/value labels, a platform hint and a saved SSH Profile ID. Multiple destinations can reuse one Profile's authentication and route. Host metadata contains no password or private key body. Profile hints sent to the editor contain only ID, name, default endpoint and tags. A platform hint does not prove the available shell or command syntax.

The catalog permits 10,000 Hosts, 256 groups and an 8 MiB serialized document. Metadata has bounded byte lengths; identities, Profile/destination duplicates, missing static members, missing groups, ancestry cycles and dynamic membership cycles are rejected. Label keys cannot contain '=' so every stored property can be edited and queried exactly. Parent groups organize the library and do not implicitly include child members. Removing a referenced Host or group is rejected until references are updated.

Static groups store a deduplicated set of Host IDs. Dynamic groups store a validated selector. Dynamic groups can explicitly refer to other groups; dependency and parent depth is bounded to 32. Membership evaluation follows dependency order, independently of catalog order. Selection results have a stable Host ID order.

## Query language

Selectors support AND/OR/NOT and &&/||/!, parentheses, all/none, exact tag/env/label/platform/name/address predicates and typed host/profile/group UUID predicates. NOT binds before AND, which binds before OR. Operator words are case insensitive; field names and values are exact and case sensitive. env:value is label:env=value. Quoted values support escaped quotes and backslashes. A full label predicate can contain a 64-byte key and a 256-byte value.

The grammar is limited to 4 KiB, 256 tokens, 128 AST nodes, 32 nested levels and 321-byte predicate literals. Evaluation uses a private topological arena rather than recursive expression traversal. Empty queries, unknown fields, malformed IDs, bad escapes, missing group references and excess complexity fail without falling back to all. Diagnostics carry an error category and byte offset, never query text. Explicit selections and query results are limited to 1,000 targets; an excess count rejects the whole selection, with no truncation.

## Immutable target snapshot

Preview returns exact Host records and both the Host and Profile revisions. Freeze rechecks these revisions and records an independent snapshot UUID, selection, Host records, original saved SSH connection templates and the complete required jump dependency closure. Empty frozen selections are rejected. Unrelated Profiles are excluded. Subsequent tag, group, endpoint or Profile changes cannot change an existing snapshot. Restore validates the versioned, bounded document, identities, required configuration closure and saved connection configuration without consulting the current library.

The original Profile template remains available through credential_template for the existing vault binding checks. resolved_connection copies that template and overrides only the Host destination address and port. Future task execution must validate credentials against the frozen original template and perform SSH host-key checks for the actual destination and each jump. This snapshot does not authorize bypassing credential binding or host-key verification. Query and freeze do not create sessions or make network connections.

## Storage, IPC and editor

The daemon stores host_catalog_state with an independent CAS revision. SQLite schema 11 adds this table. Each supported upgrade creates a backup and commits its complete migration atomically; failure preserves the old schema/data. Loading rejects oversized stored blobs before allocating their contents. Invalid or unavailable Profile references block saves and execution snapshots; Load still exposes orphaned Hosts so the user can repair them.

IPC minor 23 introduces HOST_GROUP_CONTROL at bit 28 and envelope payloads 45/46. Load/Save/Preview/Freeze requests have strict operation-specific shapes. Save and selection operations require both catalog revisions; the shared daemon catalog mutation lock serializes them with Profile changes. Request/response Debug omits document and selector bodies, and owned JSON buffers are cleared on drop. This does not promise erasure of all application/UI copies.

Desktop windows share a bounded background connection with a five-second request deadline. Each editor retains the revision pair captured when its form was opened; a newer catalog does not silently rebase an old form. Save verifies the exact returned catalog and next revision. Conflicts, failed/lost/mismatched save replies or failed reloads pause writes until explicit reload. Mutations are not replayed automatically. Preview and Freeze compare the returned revision pair, selection and full Host list with the confirmed local catalog. The Host editor blocks terminal keyboard/IME dispatch and disarms synchronized input while open.

## Verification boundary

Automated coverage includes selector precedence/quoting/Unicode/limits/privacy, static and dynamic groups, cycle/reference deletion protection, leaf-first dependency depth, exact metadata editing, 1,000-target immutable snapshots, required jump templates, SQLite CAS/reopen/backup/corrupt blobs/migration rollback, capability and framed daemon operations, headless stale editor state and authenticated lost-reply/conflict handling. These checks do not replace native GUI/IME interaction, external SSH host-key/authentication acceptance or the full platform matrix. The task scheduler, persistent TaskRun records, result console and multi-host SFTP remain the next Phase 2 tasks.
