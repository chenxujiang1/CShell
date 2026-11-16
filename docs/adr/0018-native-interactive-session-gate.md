# ADR-0018: Native interactive session acceptance probe

- Status: Accepted
- Date: 2026-10-01

## P1-099 scope

Add `cargo xtask interactive-sessions` as a reproducible functional gate for 20 simultaneous, independent local PTY sessions. Invoke it from `native-ci-bench`, which already runs on Windows x64, Linux x64 and macOS arm64/x64. The test owns a separate registry, journal directory, authenticated named pipe or Unix socket, and fresh handshake token. It never connects to the user's daemon, restarts it or reads their saved Profiles, Vault or trust.

The custom bench runs a small 2-session/2-round smoke probe during ordinary workspace tests. The xtask sets `CSHELL_INTERACTIVE_GATE=1` for the full 20-session/10-round probe. Counts are fixed by the mode; environment variables cannot silently reduce the full gate. Each child is the same probe executable running a literal helper argument, using the native PTY implementation and terminal pipeline.

## Assertions

1. Subscribe to every session through real authenticated local transport and the production full-frame/delta replica. Observe a distinct child-ready marker and verify the exact live session set.
2. For ten rounds, resize every session and submit typed input through IPC. Give each session a distinct nonce including its destination index; each child refuses input addressed to another session. Each child emits 64 colored burst lines and an identity-specific response containing the transformed nonce. Ordinary terminal input echo cannot satisfy the response check. Send all inputs before draining any subscriber, then read subscriptions concurrently.
3. Require the correct session's transformed response and requested terminal dimensions. Refuse output from another session, mismatched control/close identities, duplicate/missing session-list entries, invalid control status or request correlation. Bound individual IPC/observation waits and measured input-to-frame interactions to five seconds.
4. Drop every subscription halfway through the run. Verify all children remain live, reconnect all 20 with fresh replicas, and recover the latest acknowledged output and dimensions. This models GUI disconnection while the daemon remains alive.
5. Explicitly close a KeepAlive view and verify the entire live set is preserved. Then close every process through IPC and require an empty registry response.
6. Stop the owned server, reject any IPC connection failures, reopen every closed segmented journal with the production reader/writer index, and find its last acknowledged response with valid index integrity.

The cleanup guard also stops owned tasks and terminates registered children when an assertion fails. Normal PTY registry destruction covers failures during initial spawning. No child or IPC endpoint is intended to outlive the probe.

## Desktop workbench logic gate

`cargo xtask workbench-sessions` runs a separate desktop integration test with a fixed 20 real PTYs, an owned authenticated IPC server and production `DesktopApp`, desktop connection workers, terminal surfaces, selection extraction and asynchronous search. It also runs in ordinary full workspace tests on the native platform matrix.

For every tab, attach its runtime session, send a unique destination-specific nonce, confirm its transformed output and dimensions, extract an exact copy selection and find two exact search matches. Navigate to the second match, then switch through all tabs and verify each tab's query, options, selection, active match and output are retained after asynchronous refresh. Restart the desktop IPC workers while preserving the daemon and require all 20 tabs to reconnect with the same state.

Split the production workspace into four visible panes, move existing tabs into those groups, verify distinct session bindings and cached snapshots, focus and send input to each pane, and lay out the actual egui workbench with four nonempty pane rectangles. Close all 20 tabs through the GUI's acknowledgement path and verify KeepAlive preserves the exact PTY set. Explicitly terminate the owned children afterward. Cleanup also applies to failures; the fixture uses child-only environment overrides and never changes global daemon discovery settings.

This test exposed normal authenticated IPC retirement being counted as rejection before the first request/response, and Windows named-pipe closing errors not being recognized in the service loop. Apply the existing disconnect policy consistently to initial I/O and recognize `ERROR_NO_DATA`/`ERROR_PIPE_NOT_CONNECTED` on Windows. Authentication failures, protobuf errors, frame size limits and other I/O failures remain errors; regression tests cover malformed initial protobuf and unrelated Windows errors.

The IPC server keeps its pending accept future pinned across connection completion events. On Windows that future owns a named-pipe instance; recreating it whenever another connection finishes can close a pipe already opened by a new client. Only replace the future after a completed accept. A Windows regression gate exercises 128 authenticated request connections while preceding clients retire, verifies every correlated reply, and requires 128 completed connections with no rejection. This preserves real handshake failures instead of masking them with client retries.

The successful JSON receipt reports 20 copy, search, cached-tab and reconnection checks, four focused-pane input checks and 20 KeepAlive view closes. It explicitly records `native_presentation_measured: false` and `os_clipboard_measured: false`: the gate extracts the same text used by the GUI copy shortcut and exercises real IPC/workbench state, but creates no native window and does not write the user's system clipboard. Native GPU presentation, pointer/keyboard/IME/DPI behavior and manual visual acceptance retain their separate requirements.

## Native window redraw scheduling

The separate `log-window-e2e` and `workspace-window-e2e` gates use actual native windows and GPU presentation. The workspace gate requires a presented secondary terminal window as well as the primary log window's scrolling, paging, reflow, resizing and device recovery assertions. A secondary window alone cannot satisfy it.

Do not forward egui-winit's `repaint` response for an incoming `RedrawRequested` into another immediate redraw request for the same window. That event is already being serviced. On Windows the previous behavior continuously regenerated the secondary window's `WM_PAINT` and starved the primary window, which received no redraw events. The desktop instead schedules egui's root viewport `repaint_delay`, combines each window's deadline with cursor and daemon polling deadlines, consumes a due deadline once and clears it for idle output. Other input/window events retain their repaint requests.

The animation regression uses actual egui output to check a delayed repaint, its wake deadline, single consumption and idle cancellation. Native gate timeout diagnostics distinguish missing redraw events from skipped surface presentations and report whether content and renderer were available when the last redraw was observed. Unobserved state remains unknown. Keep the original failing log and source revision when recording a later successful gate; local Windows results do not imply the four-platform CI has run.

## Evidence and limits

Print a version 1 JSON result only after all assertions pass, including mode, OS/architecture, session/round/sample counts, resize/reconnect/closed-journal counts, IPC rejections, p95/p99 input-to-IPC-frame times, functional timeout and `gpu_measured: false`. Full mode produces 200 acknowledged samples and resize checks, 20 reconnections and 20 closed journal checks. Keep captured stdout with the CI run or local source state that produced it.

These times measure this functional workload with native PTYs and local IPC in `ci-local`; they are not the render completion percentiles from `echo-latency-bench`. That existing gate separately measures local PTY and loopback SSH through GPU completion and enforces its hardware/software budgets. The 20-session probe does not establish external SSH server compatibility, saved Profile creation, real GUI rendering of all 20 panes, daemon crash/power-loss recovery, resource thresholds, or acceptance on platforms that have not run the current code. Existing Profile/SSH integration, resource, journal crash and native window gates remain necessary evidence for P1-099.
