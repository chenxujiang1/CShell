# 0101 — ZMODEM terminal transfers and inherited download directory

Remote `rz`/`sz` requires a ZMODEM endpoint on the existing terminal channel.
SFTP cannot respond to those commands and is not used as a substitute.

The daemon detects CRC-validated ZRINIT/ZRQINIT hex headers across arbitrary SSH
packet boundaries before the terminal parser. A bounded `zmodem2` 0.7.3 state
machine streams files through that channel. Native multi-file selection starts
uploads. Downloads start in the effective local directory without another
confirmation. The GUI shows per-session direction, filename, progress and cancel.
The final ZFIN/OO handshake is drained before restoring terminal input.

The Tool menu exposes global defaults. `transfer-settings.json` stores the global
download directory, saved atomically beside the desktop's display settings.
`SshConnectionOptions.zmodem_download_directory` optionally overrides it in the
profile's existing options JSON (no database migration). Missing/empty overrides
inherit the current global value at transfer start; a missing global value uses
the user's Downloads folder, falling back to the home directory. Editing a global
default never materializes it into existing profiles. An invalid explicit path
prompts for a usable directory rather than silently switching destinations.

The authenticated IPC feature bit 62 and messages 65/66 carry transfer controls
and progress, never file contents. A transfer ID rejects stale/double controls.
The daemon revalidates all local paths and opens selected regular files before
starting. Protocol positions are 32-bit; files larger than 4 GiB minus one byte
are rejected. Remote names must be a single safe UTF-8 filename, excluding Windows
reserved names and alternate streams. Downloads use a temporary file inside the
chosen directory, check the advertised size, sync and publish without clobbering;
collisions get numbered names. Partial downloads are removed on cancellation,
failure, or disconnect. Remote executable mode is not applied to local files.

Binary data does not enter output recording, clipboard processing or terminal
parsing. Normal keyboard input is blocked during transfers; queued input is
fenced by both connection and transfer epochs. File input/output work uses
blocking workers with bounded queues/pump output. Disconnect stops the transfer;
SSH recovery does not replay it. Waiting selection and stalled transfers expire.

Validation includes fragmented headers/CRC rejection, binary and empty multi-file
uploads/downloads, safe publication, stale controls, cancellation cleanup,
inheritance and settings persistence, plus an opt-in test against original Linux
lrzsz binaries under WSL. The interoperability fixture is built inside default
`target/zmodem-interop`; it is neither installed nor shipped with CShell.
