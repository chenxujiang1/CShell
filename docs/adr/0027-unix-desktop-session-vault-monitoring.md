# ADR-0027: Linux and macOS desktop-session monitoring

- Status: Accepted
- Date: 2026-10-08

## Shared lifecycle

Extend ADR-0026 with owned Linux/macOS monitor threads. All adapters reuse the
native session-state DTO, generation fencing, pending key clearing and explicit
Vault unlock. Monitor startup, failure and shutdown are Unavailable. Threads hold
weak controller references; their exit guards restrict access even on unwinding.
No adapter locks/unlocks the physical desktop or controls system sleep.

## Linux

Use the existing pinned zbus 5.19.0 dependency through its Tokio backend, with a
bounded signal queue, 1.5-second read timeout and a one-second reconciliation poll.
Use the fixed root-managed system bus socket, ignoring bus-address environment
overrides, and require the login1 owner's bus-reported UID to be root. Resolve the
daemon PID via logind. For a per-user service outside a login session,
accept only one active, local X11/Wayland user session of the daemon's effective UID.
Freeze path, ID, UID and monotonic creation time across reconnects; never switch to
another session silently. Pin the system service's unique bus owner and recheck it
before accepting state. Never trust an environment-provided session ID or user bus.

Subscribe before the initial read to Lock/Unlock, security-property changes and
manager sleep/shutdown signals. Events restrict immediately; only an uncached,
coherent property read can restore desktop availability. Active, State, User,
Remote, Class, Type, Id, TimestampMonotonic and LockedHint are checked. Shutdown,
sleep, inactive/remote sessions, owner loss, malformed or missing data and timeouts
restrict access. CLOCK_BOOTTIME detects sleep/monitor gaps even when the ordinary
monotonic clock stops during suspend. No logind mutations or inhibitors are issued.
Lock requests remain restricted until a reported locked-to-unlocked transition is
observed; an early false LockedHint after a lock request does not grant availability.

LockedHint is the compositor's reported hint, not universal proof that every Linux
desktop locks correctly. Supported GNOME/KDE/compositor integration and user-service
session discovery require native acceptance. Headless/no-logind environments report
Unavailable; they do not silently fall back to unlocked.

## macOS

Use public IOKit and CoreFoundation functions to read the IORegistry root console
user snapshot, bind the effective UID, and require exactly one console user with
completed login and a typed screen-lock boolean. Read IOConsoleUsersSeed around the
snapshot and reject changes during a read. A seed change fences an old unlock even
if a short lock/unlock happened between polls. Poll at 500 ms. Sleep/wake comes from
IORegisterForSystemPower on an owned CFRunLoop; acknowledge required sleep messages
with IOAllowPowerChange and never veto sleep. mach_continuous_time detects missed
sleep gaps. All CF objects, Mach/IOKit handles and callbacks retire on their owner
thread before releasing the stable callback context.

The console dictionary names are published in Apple's open-source private key
header; they are not a promised stable public schema. No private function is called.
Missing/changed keys or seed, wrong CF types, ambiguous console ownership and failed
power registration report Unavailable. Compatibility on supported macOS releases
must be established before acceptance; absence of a lock property is never treated
as false.

## Sources and status

- [logind interface and LockedHint contract](https://github.com/systemd/systemd/blob/main/man/org.freedesktop.login1.xml)
- [IOKit public API and ownership](https://github.com/apple-oss-distributions/IOKitUser/blob/main/IOKitLib.h)
- [Apple console property schema](https://github.com/apple-oss-distributions/xnu/blob/main/iokit/IOKit/IOKitKeysPrivate.h)
- [Power notification lifecycle](https://github.com/apple-oss-distributions/IOKitUser/blob/main/pwr_mgt.subproj/IOPMLib.h)

This iteration is code only. No compilation, tests or native lock/sleep actions are
run. Earlier quality/platform evidence does not cover these changes. P1 acceptance
remains open, including supported desktop/compositor and macOS schema compatibility.
