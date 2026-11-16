# ADR-0049: SFTP directory cursors

- Status: Accepted
- Date: 2026-10-12

## Scope

Continue P3-010 with complete forward traversal of stable daemon-local and remote
directories. The previous List/LocalList operations retain their bounded legacy
behavior. The desktop now uses explicit cursor operations; CLI exposes both forms.
This is coding only. Compilation, tests, native GUI and actual SFTP verification
remain deferred. [ADR-0050](0050-verified-session-transfers-and-explicit-recovery.md)
subsequently adds verified transfers and recovery on the original live session;
reviewed session migration and remote file mutations are subsequently coded in
[ADR-0051](0051-complete-sftp-workspace.md), together with managed Follow/NoFollow
browsing using the same reservation, deadlines and cursor lifecycle.

## Reader and bounds

Remote DirectoryCursor owns one SFTP channel and OPENDIR handle. It keeps unread
entries from each NAME packet, so a page boundary never discards the packet tail.
One-entry lookahead distinguishes a full final page from another page. EOF closes
the directory handle. Close/drop closes the channel; no file mutation occurs.
Packet limits remain 256 KiB. Invalid names, path overflow and repeated empty
responses fail instead of producing aliases or an endless loop.

LocalCursor owns ReadDir, one pending entry and directory metadata. Opening,
enumerating and lstat run on blocking workers. Absolute UTF-8 directories and
ancestors must pass the existing symlink/reparse restrictions. Detectable directory
identity/mtime changes fail with Conflict before or after a page; Unix additionally
compares device/inode. Native filesystem races still require unified verification.
Links are shown without traversal. Both readers preserve native enumeration order;
this is forward enumeration, without a global sort or an immutable snapshot promise.
Servers can change a remote directory during traversal, producing their own live
enumeration behavior. Reload starts a fresh reader.

Pages contain 1–512 entries. The daemon has at most 16 reserved/live cursors and two
per SessionId, including in-flight opens. Each cursor has a fixed two-minute lifetime.
Native I/O is serialized per cursor, outside the global map lock. Each request has
a thirty-second deadline including open, lock wait and enumeration; native close
is bounded separately to two seconds and channel drop remains the fallback.
Kernel-blocked local filesystem calls cannot be cancelled by a Tokio deadline.
EOF, errors and explicit close remove the cursor and cancel its expiry waiter.
Interrupted opens retain their reservation until expiry; abandoned readers are
closed by expiry. Daemon restart invalidates cursors; cursors are never persisted.

## Protocol

Minor 44 / SFTP_DIRECTORY_CURSOR bit 48 adds RemoteBrowse (13), LocalBrowse (14),
BrowseNext (15), BrowseClose (16). The existing SFTP_CONTROL and SFTP_BROWSER_QUEUE
capabilities are also required. Old operations and default wire encodings remain.

Begin accepts session, the appropriate directory field, and page size. Empty local
directory means the daemon user's home. Remote directories must be absolute; ambiguous
duplicate separators and traversal components are rejected. Next accepts the exact
session, opaque 16-byte cursor in transfer_id, expected_revision and page size.
Close accepts that session/cursor/revision with zero page size. Other fields must
be default; no mutation confirmation is used for these read-only operations.

Response adds browse_id (tag 7), browse_revision (8), browse_directory (9),
browse_local (10). Existing entries carry the current page. truncated means another
page exists for the new operations. First revision is 1; each successful next/close
increments once. The same continuation cannot be replayed. At EOF/close, the last
response retains the descriptor but the cursor is no longer usable. A lost reply
requires reload rather than guessing another revision. Remote continuation also
checks that the registry still contains the same live Arc<SshSession>; it never
rebinds a directory to another SSH connection. Paths/cursors stay out of Debug.

## Desktop and CLI

The desktop holds only one page per pane. Next replaces that page. Loaded directory
is shown independently of path input. Next/close carries the displayed descriptor;
the worker checks it again before sending. Navigation releases the known cursor,
and a known session switch releases both. Lost descriptors are reclaimed by expiry.
Replies validate side, directory, ID, next revision, entry limits, parent membership
and kind. Failed/unconfirmed requests invalidate the local continuation; reload is
explicit. Older daemons reject cursor browsing through capability negotiation.

CLI adds local-browse, remote-browse, browse-next and browse-close. JSON output carries
session, cursor UUID, revision, directory, local, more and entries. Next/close require
the previous descriptor's session/cursor/revision/side/directory. Legacy local-list
and remote-list remain available with their explicit truncation flag.

## Deferred verification

Unified verification must cover zero/512/513 entries, packet-tail retention,
short/empty/malformed NAME responses, duplicate continuations, scope mismatch,
expired/closed/restarted cursors, cross-session isolation, concurrent readers,
interrupted opens, bounded timers/handles, local directory changes, links/reparse
and UTF-8 failures, GUI stale commands/navigation and older peers. No automated or
native verification ran for this slice; only cargo fmt ran.
