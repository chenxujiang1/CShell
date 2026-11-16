# 0103 — Terminal resource budgets

Keep the default 10,000 physical history rows and add conservative allocation
budgets: at most 64 MiB per interactive daemon terminal, 512 MiB for all retained
interactive histories. The global pool divides admission quotas across live
engines, weighting visible frame subscribers twice as heavily as background
sessions. It evicts reductions before granting increases, and recomputes shares
on registration, activity changes and expired engine references. Current primary
and alternate-screen contents are preserved. Closed-but-retained session history
still counts; removing the session releases its share.

The pinned Apache-2.0 alacritty_terminal 0.26.0 source patch is kept in
vendor/alacritty_terminal. It bounds native zero-width payloads at 64 characters
per cell, native OSC8 URIs at 4 KiB, explicit link IDs at 256 bytes, titles at
4 KiB and title/keyboard stacks at 64 entries. It exposes primary-history
allocation accounting even while the alternate screen is active. Shared payloads
are charged per reference, deliberately conservatively. Unused row cache storage
is released; cached per-row charges are invalidated on every mutable access.
The Row cache changes its layout, so storage uses safe Vec::swap rather than the
upstream fixed four-word swap. Original license and library unit tests are kept;
the separate approximately 47 MB reference-fixture bundle is not copied.

The adapter buffers complete ordinary OSC strings only up to 64 KiB before VTE.
Oversized strings are discarded through their terminator, then normal parsing
resumes without printing payload bytes. OSC52 is stripped from VTE and continues
to use the daemon's existing separate bounded consent observer on the original
stream. UTF-8 bytes and fragmented escape sequences retain their order.

Input is parsed in slices of at most 64 KiB and history is evicted between slices.
Resize admits at most 1,024 columns, 512 rows and 65,536 visible cells, and trims
history before widening/reflow. Global quotas bound retained histories; they are
not a limit on total process RSS. Live screens, parser state, transport buffers,
copy-on-write page payloads, allocator overhead and the bounded current parsing
slice are separate transient/live costs. The budgets are enforced at slice
boundaries rather than claiming zero temporary overshoot for every allocation.

Output ingress combines the existing bounded message count with a 4 MiB byte
budget including the currently processed message. Blocking transport submission
splits input into 64 KiB pieces and waits for capacity without silently dropping
bytes. Permits are released on processing, rejected sends, shutdown and errors;
blocked waiters are woken on closure. Nonblocking submissions larger than 4 MiB
return TooLarge rather than permanently retryable Backpressure; closed queues
return Closed. Error formatting reports lengths, never terminal contents.

Parser responses remain immediate. Snapshots coalesce at 16 ms with active frame
subscriptions and 100 ms without them; resize/reset and final shutdown flushes
remain explicit. Subscription clones retain one activity lease until the last
clone drops. Journaling, when enabled by existing session policy, still receives
the original byte stream, independent of snapshot coalescing and history eviction.

History paging clones at most 131,072 native cells/512 rows using shared immutable
payloads, then releases the parser mutex before UTF-8/style conversion. There are
at most two concurrent conversions. Selected rows are centered on the requested
anchor rather than exhausted from the oldest prefix. Text allocation is bounded
by per-row quotas (at most 2 MiB per page); all base cells survive, while excessive
combining marks or style density have an explicit truncated/simplified fallback.
The existing 4 MiB wire text and 16,384-style-span checks remain enforced.

Eviction advances monotonic physical row IDs. LogPage tag 8 carries the earliest
retained ID (zero from old peers means one); the renderer clamps an evicted
anchor, and scrollbars/jumps use the actual retained interval. Ordinary journal
pages continue to start at one. IPC minor is 58; the new field is backward
compatible at protobuf decode level. GUI and daemon are deployed together.

Regression coverage includes native allocation limits, fragmented/oversized OSC
recovery, UTF-8/link handling, byte quota under column resize, alternate-screen
preservation, 20 engines sharing a small test pool, foreground admission priority,
released-session capacity, queue closure/oversize handling, a 1,001-message burst
with coalesced publications, dense color/combining pages, reclaimed anchors,
existing terminal/parser tests, real desktop PTY scrolling and SSH/SFTP/ZMODEM
interoperability. Build outputs and trial binaries reuse the existing locations.
