# Phase 0 Go/No-Go 工程验收

日期：2026-09-18。依据 [开发计划](DEVELOPMENT.md) §20.1 和 [ADR-0005](adr/0005-phase0-engineering-gate.md)。本页记录客观工程门禁、平台回退选择和后续风险。

## 证据矩阵

| 范围 | 当前证据 | 判定 |
|---|---|---|
| P0-001～P0-020：workspace、MSRV、IPC、安全边界 | `cargo xtask check`、Rust 1.95 workspace/all-features/locked、`cargo deny check` 本地通过；四目标平台测试及 Windows 管道、Unix UDS/权限、协议/认证/慢订阅隔离进入原生 CI。[Run 35318908600](https://github.com/chenxujiang1/CShell/actions/runs/35318908600) 的 19/19 作业成功，含四目标 platform-test、quality 和 MSRV。 | 计划 §20.1 门槛已通过 |
| P0-030：journal/索引/恢复 | 100 GiB 容量、随机分页、检查点重启、六种外部强杀恢复、部分写及磁盘满故障注入已完成；四目标资源门禁见 [Run 35047813511](https://github.com/chenxujiang1/CShell/actions/runs/35047813511)。 | 原型门槛已通过 |
| P0-040～P0-060：终端、PTY、SSH | ANSI/OSC/xterm、FrameSnapshot/Delta、ConPTY/Unix PTY、daemon 强杀后守护、严格主机密钥、证书/Pageant、SFTP、L/R/D 与跳板均有测试；[Run 35081620498](https://github.com/chenxujiang1/CShell/actions/runs/35081620498) 的 15 项作业成功。 | 计划 §20.1 原型门槛已通过；双重 fork 并脱离 PTY session 的后台进程不承诺自动清理 |
| P0-070：平台壳 | 最小菜单/模态框、IME commit、剪贴板安全粘贴、DPI、AccessKit 语义树及窗口冒烟；[Run 35295659087](https://github.com/chenxujiang1/CShell/actions/runs/35295659087) 四目标成功。 | ADR-0005 工程范围已通过 |
| P0-075：钥匙串最小原型 | Windows、Linux、macOS Intel/ARM64 的临时 secret 写入、读取、删除原生步骤均通过；失败恢复故障注入和脱敏/清零测试通过，见 [Run 35298996239](https://github.com/chenxujiang1/CShell/actions/runs/35298996239)。该轮整体失败来自独立 Windows OpenSSH 作业，后续 [Run 35300302386](https://github.com/chenxujiang1/CShell/actions/runs/35300302386) 已恢复通过。 | 最小原型已通过；完整 Vault 属 Phase 1 |
| P0-080：TerminalSurface/LogSurface | 可见行批处理、Unicode shaping、16 MiB atlas 压力、滚动锚点/后台 reflow 单测均通过；Windows 本机模拟 device lost 后 CPU wgpu 适配器接管并继续 120 次真实 present、分页、重排及 resize。[Run 35315272713](https://github.com/chenxujiang1/CShell/actions/runs/35315272713) 的四目标原生窗口作业均成功；Linux 强制 CPU 适配器恢复，Windows 本机强制 CPU 恢复，macOS 两架构重建后持续呈现。`8344d57` 另加每帧非空日志几何断言，本机及 [Run 35318908600](https://github.com/chenxujiang1/CShell/actions/runs/35318908600) 四目标窗口门禁均通过。 | 计划 §20.1 工程门槛已通过 |
| P0-090：性能/资源 | 60 秒输出管线超过 50 MiB/s、100 慢连接、100 GiB journal、SSH/PTY 到 GPU 回显延迟及四目标 native resource gates 已完成；最新 [Run 35318908600](https://github.com/chenxujiang1/CShell/actions/runs/35318908600) 全部成功。 | 计划 §20.1 门槛已通过 |

## 平台回退选择与残余风险

1. `8344d57` 的四目标窗口 E2E 已在同一提交的 CI 中全部通过：每次真实 present 均有非空日志几何，模拟 device lost 后继续呈现、滚动、分页、reflow 和 resize。同一提交的 macOS Intel 资源门禁也已成功。
2. P0-080 选择按平台可用性恢复：先申请 wgpu CPU 适配器，若不可用则重试常规适配器。Windows 本机及 Linux CI 已验证 CPU 软件路径；macOS 两架构已验证设备重建后继续呈现，尚未证明 CPU 适配器可用。当前没有独立软件位图呈现器。若 CPU 与硬件适配器都不可申请，窗口会持续重试且无法显示内容；此持久性硬件/驱动故障列为后续风险，不将模拟 device lost 的结果外推到该场景。
3. 模拟 device lost 只能证明应用重建路径；真实驱动故障尚未验证，不能声称故障硬件上的显示能力。真实掉电按 ADR-0005 不在当前验收范围。

## 明确后移的体验项

真实读屏手感、IME 候选窗观感、Emoji/颜色主观视觉比较和更多真实设备组合在实际使用中继续优化；完整 Vault、凭据 UI、完整系统菜单和文件工作流属于 Phase 1。自动化工程门禁不代表这些体验项已人工验收。

**评审结论：Go。** `8344d57` 的 19/19 项 CI 全部成功，P0-001～P0-090 在 ADR-0005 的工程范围内满足 §20.1 硬门槛，P0-099 证据与风险已记录。Phase 1 可按开发计划启动；本结论不表示后移的主观体验或持久性硬件故障已完成验收。