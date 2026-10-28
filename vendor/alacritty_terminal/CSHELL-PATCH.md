# Local resource boundary patch (alacritty_terminal 0.26.0)

The upstream source and original license are retained. This pinned patch bounds native
zero-width storage at 64 characters per cell, reduces spare row allocation to 64,
and exposes conservative history allocation accounting/eviction for the primary
screen, including while an alternate screen is active. Shared extra allocations
are deliberately charged per reference. It does not modify terminal escape semantics.
CShell applies its own limits to incoming control strings before the VTE parser.

Per-row extra-memory charges are cached and invalidated on every mutable row
access; the cache uses an atomic to retain Send/Sync compatibility. Storage uses
safe Vec::swap rather than assuming a four-word Row layout. Evicted history has
monotonic offsets so retaining fewer rows cannot rename existing line anchors.

The upstream library unit tests are retained and run. The separate reference-test
target and its approximately 47 MB fixture bundle are not vendored; the explicit
`[[test]] ref` target is removed from this source-only package accordingly.

Titles are at most 4 KiB, and title/keyboard stacks at most 64 entries. OSC8 URIs
are at most 4 KiB and explicit hyperlink identifiers at most 256 bytes, enforced
before they enter the cell template. Allocation charges include string capacity.
