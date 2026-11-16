# ADR-0006: Phase 1 basic SFTP control

- Status: Accepted
- Date: 2026-09-29

## Context

Phase 1 needs directory listing, single-file upload/download, cancellation, and bounded memory while SSH transports and credentials remain in cshelld. A GUI IPC disconnect must not cancel a transfer.

## Decision

- Extend local IPC minor version to 12 and negotiate SFTP_CONTROL independently. Requests without the feature receive Unsupported.
- Identify the target by a running daemon SSH SessionId; local terminal sessions are rejected. The existing verified SSH transport opens a separate SFTP channel.
- List returns at most 512 entries and an explicit truncated flag. This basic slice does not provide a directory cursor; large-directory paging belongs to the later file manager.
- Upload/download accept absolute local paths and bounded remote paths. File contents stay in daemon-side 32 KiB chunks and never cross GUI IPC. Existing destinations are refused at the preflight checks. Download uses a local temporary file, sync, then a no-replace hard link. Upload uses a remote temporary file, verifies its reported size, then renames. The remote server's rename behavior and concurrent changes limit the overwrite guarantee; content hashes and resume are later work.
- Starting a transfer returns a daemon-owned ID. The GUI polls status and can request cancellation; a lost reply is not retried automatically. One active transfer per SSH session and eight globally are permitted, with at most 64 tracked results. Finished results may be pruned when capacity is needed.
- On GUI reconnect, CurrentTransfer returns the latest tracked transfer for that SSH session. The job and cancellation flag remain in daemon memory. Daemon restart still interrupts transfers; persisted queues and offset-verified resume are outside Phase 1.

## Consequences

The basic panel can list and move single files without blocking the terminal UI. It is an implementation milestone, not the complete file manager or the user experience promotion gate. Later work adds paged navigation, multiple tracked jobs, stronger remote commit reporting, queue persistence, pause/resume, and hash verification.
