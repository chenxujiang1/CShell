# ADR-0003：首发平台、本地终端与多标签同步输入

- 状态：Accepted
- 日期：2026-08-28

## 背景

CShell 以高效、功能完整和稳定的 PC 端 SSH/SFTP 工具为首要目标。用户确认本地终端属于正式范围，macOS 与 Windows、Linux 一同首发；X11 转发可以后续规划。多主机交互能力参考 Xshell 的 Synchronized Input、Send Key Input 和 Compose Pane，把相同输入发送到多个已打开标签页，但不能与具有可靠结果语义的批量任务混为一谈。

Xshell 官方资料说明，同步输入可以把普通按键、组合键和功能键发送到多个选中终端，Compose Pane 可以把多行文本发送到当前或多个会话。这些是交互效率功能，不证明各远端 shell 状态一致。

## 决策

1. 首发平台为 Windows x64、Linux x64、macOS arm64 和 macOS x64；三平台功能基线一致。
2. Linux 首发同时覆盖 X11 与 Wayland。macOS 两种架构均进入 CI、中文 IME、GPU、休眠唤醒和长连接测试。
3. Windows/Linux ARM64 和 Android 放在桌面 v1.0 之后；Android 单独设计移动 UI 与生命周期。
4. 本地终端从 P0 开始实现，由 `cshelld` 通过 Windows ConPTY 或 Unix PTY 托管，并与 SSH 终端共用 [ADR-0004](0004-terminal-core-alacritty.md) 的终端Provider、`TerminalSurface`、主题、搜索、日志和工作区。
5. Windows 自动发现 PowerShell 7、Windows PowerShell、CMD、WSL、Git Bash 和自定义 shell；Linux/macOS读取默认 shell并允许用户创建 `LocalProfile`。
6. 本地进程在 GUI 崩溃时继续运行；daemon 崩溃后不承诺重新接管原 PTY，必须明确标记断开。
7. 同步输入的目标是用户明确选择的已打开终端标签/分屏。进入状态需显式 Arm，目标集合和视觉警示始终可见。
8. 同步输入广播逻辑 `InputAction`，由各目标根据自身终端模式编码；支持文本、组合键、功能键和安全粘贴。IME仅广播最终提交文本，鼠标与resize默认不广播。
9. 每个目标使用独立有界发送队列；慢目标不阻塞其他目标，任何溢出或断线都必须逐目标提示。
10. 密码、私钥口令和keyboard-interactive安全输入永不广播。SSH与本地终端混合广播默认禁止，用户逐个显式选择后才可启用。
11. Compose发送前冻结目标快照并展示目标、换行和控制字符。同步输入不提供退出码、重试、幂等或成功判定；可靠自动化必须使用批量exec任务。
12. X11转发不进入桌面v1.0首发范围，但在`SshProvider`与隧道模型预留capability；后续依赖系统或外部X Server，不随CShell内置X Server。

## 后果

- 首发测试矩阵扩大到四个桌面架构与Linux双窗口系统，必须配置对应CI和物理测试机。
- 本地终端增加进程监督、Profile发现、cwd/env和退出恢复工作，但大量复用现有终端、渲染和日志管线。
- 按逻辑动作广播比复制原始字节更复杂，但可正确处理每个目标不同的application cursor、keypad和bracketed paste模式。
- 将同步输入与批量任务分离，避免把交互便利误认为可靠的多主机执行。
- 推迟X11与移动平台，减少首发协议和生命周期风险。

## 依据

- [Xshell 8 Manual](https://www.netsarang.com/docs/Xshell8_manual.pdf)
- [Xshell All Features](https://www.netsarang.com/en/xshell-all-features/)
- [Xshell Synchronized Input and Compose Pane](https://op-www.netsarang.com/products/xsh_key_features.html)
