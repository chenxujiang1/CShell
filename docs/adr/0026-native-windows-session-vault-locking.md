# ADR-0026: Native Windows session locking in the daemon

- Status: Accepted
- Date: 2026-10-08

## Decision

P1-080 adds a daemon-owned Windows session monitor. A dedicated thread owns an
invisible top-level Win32 window, registers WTS notifications for its own process
session, and handles lock, logoff, console/remote disconnect and suspend events.
It never changes the workstation state. Startup, reconnect/unlock and resume query
WTSInfoEx instead of trusting a window message as authorization. A one-second
timer reconciles state; an expired three-second observation denies credential
access. Registration/query/message-loop failure is unavailable, not unlocked.
Failed registration is retried by the monitor without restarting user sessions.

Restrictions advance the existing Vault generation and cancel pending SSH/host-key
authentication. They set a pending key-clear flag without waiting for an Argon2
worker holding the Vault state mutex. The existing weak expiry timer clears keys
when that bounded worker releases its state. Credential access and final session
registration check the native restriction as well as generation. The native gate
also covers the native-only credential workflow on Windows. Existing SSH and local
sessions continue. Reconnecting, unlocking the workstation or resuming never
unlocks the encrypted Vault; a fresh explicit Vault unlock is still required.

IPC minor 26 adds VAULT_SESSION_STATE (bit 31) and a readonly session-state field
in VaultResponse. Old replies mean Unsupported and retain the focus/lease policy.
The desktop clears sensitive inputs and pending confirmations on generation
changes and displays the reason an unlock is currently disabled.

Linux/macOS native notifications remain follow-up work. Their existing focus lock
and absolute 15-minute lease remain in force. This implementation does not claim
that messages are delivered synchronously at the physical OS lock instant, or that
keys already returned to an independent caller can be revoked.

## Native contracts

- [WTS registration and teardown](https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/nf-wtsapi32-wtsregistersessionnotification)
- [Session change messages](https://learn.microsoft.com/en-us/windows/win32/termserv/wm-wtssession-change)
- [WTSInfoEx state and Windows 7 caveat](https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/ns-wtsapi32-wtsinfoex_level1_w)
- [Power broadcasts](https://learn.microsoft.com/en-us/windows/win32/power/wm-powerbroadcast)
- [Suspend/resume registration](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-registersuspendresumenotification)

The product's Windows ConPTY baseline is newer than Windows 7. Raw FFI and pointer
lifetimes are confined to the Windows adapter, as in the existing IPC and process
tree adapters. Window, class, timer and WTS registrations are retired on the owning
thread; no callback retains a pointer after window destruction.

## Verification status

Code only per the user's instruction. Native lock/unlock/disconnect/sleep, worker
failure, unlock races, UI clearing, compilation, workspace gates and the four-target
matrix are deferred to the consolidated verification pass. Old gate evidence does
not cover these changes and P1 acceptance remains open.
