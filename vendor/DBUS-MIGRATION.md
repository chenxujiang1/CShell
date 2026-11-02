# Linux D-Bus compatibility patches

CShell uses zbus 6.2.1, which incorporates the formerly separate zvariant,
zvariant_derive and zbus_names packages. No advisory exceptions were added for
their retirement. At the time of this migration, released Secret Service and
AccessKit/AT-SPI adapters still depend on zbus 5, so their source is patched here.

`dbus-migration-sources.json` records each original crates.io release and archive
SHA-256. The archive checksums were verified before extracting the sources.
Each crate retains its upstream license files and copyright notices. These are
local compatibility patches, not upstream releases.

The patches replace names and wire imports with their zbus 6 equivalents,
unify error conversions, adapt deferred builder validation, owned proxy
properties, and zbus_xml 6 iterators. AT-SPI event layouts, interface names,
discriminants and XML definitions are retained. The former unchecked body
decoder maps to `deserialize_ignore_signature`, preserving its encoding
compatibility behavior and byte validation.

zbus 6 removed the blocking API. The Secret Service synchronous facade delegates
to the original asynchronous implementation on a persistent, two-worker Tokio
runtime. Calls enter it on an owned scoped thread so callers already using a
current-thread runtime do not nest or deadlock executors. Encryption, prompts,
attribute matching and secret serialization remain in the async implementation.
Existing `service` and `username` keyring attributes are unchanged.

The Linux platform CI checks native keychain round trips and uses the independent
libsecret `secret-tool` client to create a preexisting entry, read it through the
new adapter, update it, and verify it independently. All entries use random test
identifiers and are cleaned up. The test also exercises the synchronous facade
inside a current-thread executor. Workspace CI continues to compile AccessKit
and the AT-SPI interfaces with accessibility enabled.

Remove these patches when compatible upstream releases are available. Review
the port, upstream changes, licenses and dependency advisories on every release.
