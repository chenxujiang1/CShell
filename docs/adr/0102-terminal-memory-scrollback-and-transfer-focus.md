# 0102 ? Terminal memory history and focused transfer feedback

Normal GUI terminals previously only forwarded wheel movements to an already-open
log surface. The normal view never created one, log paging was disabled outside
explicit log mode, and SSH log requests had no scrollback route. Disk recording
is disabled by default, so enabling the existing log view alone could not fix it.

The terminal keeps a bounded 10,000-row Alacritty history. A terminal-history flag
on the existing paged request and feature bit 63 expose read-only slices of this
RAM state for SSH and local sessions. Pages preserve Unicode and cell styles,
are bounded by rows, cell count, UTF-8 bytes and style spans, and do not move the
live display or cursor. A shared engine handle keeps history available after a
transport worker ends; closing/removing the session releases it. No journal is
created or enabled for scrollback. At capacity the oldest rows are discarded.

A single upward wheel movement creates the history view and queues its movement
until the initial page and reflow are ready. Paging connections start on demand,
stop on return to live, and are separate from keyboard/resize traffic. Each tab
retains its own view mode and anchor, including split panes and detached windows.
Shift+PageUp/PageDown page through history. End, the return button, scrolling to
the bottom, or normal terminal input restore the live view. Plain Up/Down continue
to reach the remote shell for its own command history.

ZMODEM feedback emphasizes upload/download direction, the current filename,
percentage, readable byte units, peer and download directory. Automatic progress
windows close after successful transfers, including the protocol finish handshake;
failures and actionable errors stay visible. A fast failure still opens its error
window. Active transfers and native selection block input, while an error window
alone does not prevent terminal use.

Validation covers first-wheel asynchronous reflow, old command/Unicode/color
retention, bounded history, no disk output with recording disabled, retention
after parser shutdown, real SSH history across IPC, feature negotiation and
independent anchors without live-cursor changes, plus the existing SSH, ZMODEM,
rendering and desktop regressions.

Wheel routing must be based on the terminal pane rectangle and topmost egui
layer, not egui-winit's generic `consumed` result: the transparent CentralPanel
claims pointer input over the GPU terminal as well. Terminal wheels bypass egui
before dispatch; floating windows, menus, modal dialogs and active widget drags
retain their input. The regression gate draws the actual workbench, reproduces
this consumed flag, checks overlay exclusion, then uses a real PTY and desktop
IPC connection to scroll old output across multiple RAM history pages.

Returning to live output also clears the connection's cached page and advances
its paging epoch. Starting another history view invalidates any earlier cached
or in-flight page before requesting the current tail. Replies are published only
for the active request and epoch, so an old anchored reply cannot appear between
command input and the new tail page (including repeated identical tail requests).
The regression repeats command output and one-notch scrolling three times and
checks every displayed frame stays within the expected three-row movement.
