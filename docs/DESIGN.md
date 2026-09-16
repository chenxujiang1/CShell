# CShell 产品与技术设计方案

> 状态：Draft 1.0  
> 日期：2026-08-28  
> 首发平台：Windows x64、Linux x64、macOS arm64/x64  
> 后续平台：Windows/Linux ARM64、Android  
> 核心范围：SSH、SFTP、本地终端、多标签同步输入与批量执行
> 开发实施基线：[DEVELOPMENT.md](DEVELOPMENT.md)

## 1. 结论

推荐采用以下路线：

- 桌面框架：纯 Rust `winit + wgpu`；`egui` 仅负责会话树、表单、设置和任务表格等非性能关键工作台控件。
- 终端模型：Rust `alacritty_terminal` 适配层，负责VT序列、屏幕、滚动缓冲和终端mode；项目自有InputEncoder负责鼠标和键盘编码。
- 核心：独立 Rust `cshelld` + Tokio，负责 SSH、SFTP、终端模型、凭据、任务编排、输出持久化和审计。
- SSH：以 `russh`/`russh-sftp` 为首选，通过内部 `SshProvider` 接口隔离实现；在立项 PoC 后再最终锁定。
- 渲染：自研 `TerminalSurface`/`LogSurface`，通过 `wgpu + cosmic-text`、批量几何和字形图集只提交可见行与 overscan；GPU 故障时提供软件渲染回退。
- 数据：SQLite 保存非敏感元数据，分段输出存储保存大日志，系统钥匙串保存密码和私钥口令。
- 本地终端：由 `cshelld` 通过 ConPTY/Unix PTY 托管，与 SSH 终端共用终端模型、GPU 渲染、主题、日志和工作区。
- 多会话输入：参考 Xshell Synchronized Input，把逻辑按键、组合键、功能键、已提交文本和安全粘贴发送到用户明确选择的已打开标签页。
- 批量执行：使用独立任务编排引擎；同步输入不提供退出码、幂等、重试或一致性保证，不能替代批量 exec。
- 开源许可：项目主体建议采用 MIT/Apache-2.0 双许可证，正式建仓时冻结；引入依赖前执行许可证和安全审计。

重新评估后，不再建议把 Tauri + xterm.js 或 Qt Quick 作为默认桌面栈。前者的 Web 终端路径存在快速生产者、主线程和缓冲上限风险；后者虽然控件成熟，却会引入 Qt 运行库、QML 引擎、C++/Rust FFI 和 LGPL 合规边界，而本项目最困难的终端与大日志视口仍然需要自研。WezTerm 已证明 Rust + GPU 的跨平台终端路线可行，因此默认选择纯 Rust，把生态成熟度不足的风险限制在薄 UI 适配层内。

## 2. 范围与边界

### 2.1 要做

1. SSH2 终端、远程命令执行和本地终端；本地终端在首发进入正式支持范围。
2. SFTP 文件管理、上传下载、断点续传、队列和多主机文件分发。
3. 会话、文件夹、标签、分组、模板、认证配置和跳板链管理。
4. 多标签、分屏、窗口分离、同步输入、命令编辑区和快捷命令。
5. 多主机任务执行、并发控制、超时、取消、结果汇总、失败重跑和审计。
6. SSH 本地/远程/动态端口转发，代理和多级跳板。
7. 密码、公钥、keyboard-interactive、ssh-agent、Pageant、PKCS#11 等认证。
8. 日志、回放、搜索、高亮、触发器、通知和自动化脚本。
9. Xshell、PuTTY、SecureCRT、MobaXterm 和 OpenSSH 配置导入。

### 2.2 明确不做

- RDP、VNC、串口、TELNET、RLOGIN、FTP、X/Y/ZMODEM。
- SSH1，以及默认启用 RC4、3DES、DSA、SHA-1 等过时算法。
- 自动更新、代码签名、商店发布、安装包发布流程。
- 首版云账号、云同步、团队服务端、堡垒机服务端。
- 首版不实现 X11 转发，但在 `SshProvider`、会话配置和隧道模型中预留能力；桌面 v1.0 稳定后实现，不内置 X Server。
- 在客户端中内置完整 Ansible/SaltStack；CShell 是客户端与轻量编排工具，不是配置管理平台。

### 2.3 “全面对标”的定义

对标指 SSH/SFTP 工作流的功能等价，不追求菜单名称、私有文件格式或脚本 API 的逐字兼容。验收以用户任务为单位：同样的连接、管理、操作和自动化目标能够完成，并且多主机能力优于单纯同步输入。

### 2.4 功能优先级矩阵

`P0` 是首个可用版本，`P1` 构成多主机产品核心，`P2` 达到成熟桌面对标，`P3+` 是桌面 v1.0 后的 X11、其他架构和 Android。

| 能力域 | P0 | P1 | P2/P3+ |
|---|---|---|---|
| SSH 连接 | SSH2、密码/密钥/agent、known_hosts、keepalive、单跳 | 多级跳板、代理、连接复用、L/R/D 转发 | PKCS#11、OpenSSH CA、GSSAPI/FIDO2 按需求补齐 |
| 会话管理 | CRUD、文件夹、标签、搜索、收藏 | 模板/继承、认证配置、批量编辑、工作区恢复 | 商业客户端配置迁移完善 |
| 本地终端 | Windows PowerShell/CMD、Linux/macOS 默认 shell、日志与工作区 | WSL/Git Bash/自定义 Profile、shell integration | 其他架构和移动端按需 |
| 终端 | 多标签、分屏、UTF-8、复制粘贴、主题、搜索 | 多标签同步输入、Compose、快捷命令、高亮、日志 | 触发器、脚本录制、Hex、超大日志索引 |
| 多主机 | 基础批量 exec 原型 | selector、限流、分批、canary、取消、汇总、失败重跑 | 任务脚本、人工关卡、输出差异与策略扩展 |
| SFTP | 单机上传下载和目录浏览 | 队列、断点、多机分发、校验、原子提交 | 远程编辑、目录同步与高级冲突策略 |
| 自动化 | 无 | 结构化任务、CLI | 沙箱 JavaScript、触发器和外部 Python 适配 |
| 平台 | Windows x64、Linux x64、macOS arm64/x64 | 三平台一致性与稳定化 | Windows/Linux ARM64、Android |
| X11 转发 | 仅预留模型和 Provider 接口 | 无 | 桌面 v1.x：依赖外部/系统 X Server |
| Android | 无 | 无 | 桌面 v1.0 后独立立项：终端、轻量 SFTP、查看与触发任务 |

### 2.5 首发平台矩阵

| 平台 | 架构/窗口系统 | 首发级别 | 本地终端 |
|---|---|---|---|
| Windows | x86_64 | 一等支持；独立物理机与 CI 测试 | ConPTY；PowerShell 7、Windows PowerShell、CMD、WSL、Git Bash、自定义 shell |
| Linux | x86_64；X11 + Wayland | 一等支持；面向主流长期支持发行版确定 glibc 基线 | Unix PTY；读取 `$SHELL`，支持 bash、zsh、fish 和自定义 shell |
| macOS | arm64 + x86_64 | 一等支持；两种架构均进入 CI、IME、GPU 和老化测试 | Unix PTY；默认 zsh，支持 bash、fish 和自定义 shell |

首发功能在三平台保持一致；允许菜单、钥匙串、通知和系统快捷键存在明确的平台实现差异。Windows/Linux ARM64 在桌面 v1.0 后按需求加入。Android 复用领域核心和渲染思想，但单独设计生命周期与移动 UI。

## 3. 产品形态

CShell 包含三个入口，共用同一核心：

| 入口 | 用途 | 首发顺序 |
|---|---|---:|
| `cshell-gui` 桌面应用 | 纯 Rust 工作台、原生 `wgpu` 终端与日志视图 | P0 |
| `cshelld` | SSH/SFTP、终端模型、任务、日志和审计的每用户后台核心 | P0 |
| `cshell-cli` | 打开会话、执行任务、导入导出、CI 调用 | P1 |
| Android 应用 | 移动终端、轻量 SFTP、查看/触发已保存任务 | 桌面 v1.0 后 |

桌面版从 P0 就采用每用户单实例 `cshelld`。GUI 崩溃或重启不应中断批量任务、SFTP 传输和已建立连接。Android 受后台生命周期限制，可将同一 application/core crates 编译为进程内库，不强行照搬桌面守护进程。

## 4. 总体架构

```text
┌───────────────────────────────────────────────────────────────┐
│ cshell-gui: Rust / winit / egui                               │
│ 会话树 │ 工作区 │ SFTP │ 任务控制台 │ 设置                    │
│          ┌──────────────────────────────────────────┐         │
│          │ TerminalSurface / LogSurface             │         │
│          │ wgpu + glyph atlas + viewport only       │         │
│          └──────────────────────────────────────────┘         │
└────────────── local RPC + versioned frame snapshots ──────────┘
                               │
┌──────────────────────────────▼────────────────────────────────┐
│ cshelld: Rust Application Core                               │
│                                                              │
│ SSH I/O → Output Journal → VT Parser → Terminal Model         │
│    │            │                         │                   │
│    │            └→ Segment/Line Index     └→ FrameDelta       │
│    ├→ ConnectionPool / SSH PTY / Exec / SFTP / L-R-D Forward│
│    ├→ Local Process Supervisor / ConPTY / Unix PTY           │
│    ├→ Task Orchestrator / Scheduler / Result Aggregator       │
│    ├→ Transfer Manager / Automation / Trigger                 │
│    └→ SQLite / Vault / Audit / Metrics                        │
└──────────────────────────────┬────────────────────────────────┘
                               │
                 ┌─────────────▼──────────────┐
                 │ Local shells / SSH hosts   │
                 └────────────────────────────┘
```

关键边界：

- GUI 不持久化、不记录、不缓存凭据；用户输入只进入短生命周期可清零 buffer，Vault 解密、私钥使用和长期 secret 生命周期归 `cshelld`。
- UI 只持有 `session_id`、`run_id` 等不透明句柄。
- 原始输出不穿过 GUI；`cshelld` 先持久化并解析，再向 GUI 发布有版本的屏幕快照或可见日志页。
- GUI 丢失增量帧时请求完整快照，不能继续应用错误的 delta。
- SSH 连接、SFTP 通道、端口转发和 exec channel 统一归属 `ConnectionPool`；交互终端默认独立 transport，批量任务和跳板按策略使用有界池，避免故障放大。
- 每个会话和任务都有显式状态机，禁止用若干布尔值拼装生命周期。

## 5. 技术选型

### 5.1 UI 与终端

| 领域 | 选择 | 理由 |
|---|---|---|
| 窗口、事件与 IME | `winit` | 纯 Rust；覆盖 Windows、Linux、macOS，并保留 Android 后端；通过项目适配层隔离其未到 1.0 的 API |
| 工作台控件 | `egui` + `egui_dock` | 只用于会话树、表单、设置、SFTP/任务表格、标签和分屏；锁定版本，不用于终端正文或超大日志 |
| GPU 抽象 | `wgpu` | Windows 使用 DX12/Vulkan，Linux 使用 Vulkan/GL，macOS 使用 Metal，Android 后续复用 Vulkan/GL 路径 |
| 终端模型 | `alacritty_terminal` 适配层 | Rust、Apache-2.0、发布版可锁定；覆盖VT解析、screen/grid、滚动缓冲、selection/search和mode；输入编码由项目封装 |
| 字体与排版 | `cosmic-text` + `swash` | 纯 Rust shaping、字体回退、双向文本、连字和字形栅格化；自研有界 glyph atlas |
| 终端渲染 | 自研 `TerminalSurface` | 直接使用共享 `wgpu` device/queue、批量几何和字形图集；只渲染可见 cell，事件线程不解析终端字节 |
| 日志渲染 | 自研 `LogSurface` | 固定行高虚拟视口、磁盘分页、稳定行锚点；不为每行创建 UI 对象 |
| 可访问性 | `AccessKit` | 通过 winit 适配器暴露语义树；终端提供可见文本、光标、选择和焦点语义 |
| 国际化 | Fluent 或 ICU4X 适配层 | 从第一天启用中英文资源；领域和布局模型不依赖具体本地化库 |

终端渲染、终端模型和日志存储分开：`alacritty_terminal` 处理完整字节流，分段存储保证不丢输出，`TerminalSurface` 只绘制最新可见快照。渲染慢只能导致降低帧率，不能反向造成日志丢失或状态错误。`egui` 与两个高吞吐 Surface 共享 `wgpu` device 和最终帧合成，但不持有终端行对象。

### 5.2 Rust 核心

| 领域 | 首选 | 说明 |
|---|---|---|
| 异步运行时 | Tokio | 连接、通道、超时、取消和并发调度统一运行时 |
| SSH | russh | 纯 Rust、异步、支持现代算法、转发、OpenSSH 证书、agent forwarding 和 Pageant；必须通过 PoC 与安全审查 |
| SFTP | russh-sftp | 与 SSH provider 配套；在上层封装恢复、校验和队列 |
| 终端模型 | alacritty_terminal 适配层 | VT/grid/scrollback/mode模型；项目封装输入编码与冷滚动行导出能力 |
| 输出存储 | 自研 append-only segment + 64 位行索引 | 写入顺序化、读取分页化；日志大小不决定 UI 内存 |
| 本地 PTY | portable-pty | Windows ConPTY 与 Unix PTY 统一抽象 |
| 数据库 | SQLite + sqlx | 单机可靠、迁移简单、便于复杂查询和事务 |
| 密钥派生 | Argon2id | 便携保险库的主密码派生 |
| 加密封装 | 审计过的 AEAD 库，XChaCha20-Poly1305 | 每条 secret 使用随机 nonce，并绑定记录 ID/版本为附加数据 |
| 内存清理 | secrecy + zeroize | 降低凭据在内存中残留的时间 |
| 序列化 | serde；导出 JSON | 内部模型类型化，导出格式版本化且可读 |
| 日志 | tracing | 结构化诊断；默认强制脱敏 |

`russh` 不能直接散落在业务代码中。定义项目自己的接口：

```rust
trait SshProvider {
    async fn connect(&self, spec: ConnectSpec) -> Result<ConnectionHandle>;
    async fn open_pty(&self, conn: &ConnectionHandle, spec: PtySpec)
        -> Result<ChannelHandle>;
    async fn exec(&self, conn: &ConnectionHandle, spec: ExecSpec)
        -> Result<ExecHandle>;
    async fn open_sftp(&self, conn: &ConnectionHandle) -> Result<SftpHandle>;
    async fn open_forward(&self, conn: &ConnectionHandle, spec: ForwardSpec)
        -> Result<ForwardHandle>;
}
```

PoC 若发现 GSSAPI、FIDO2、PKCS#11、ProxyCommand、特定服务器兼容或吞吐无法达标，可以补充系统 OpenSSH 或 libssh Provider，而不改动会话、任务和 UI 模型。X11 转发也只通过后续 Provider capability 加入。

### 5.3 UI 方案复评

- Tauri/React/xterm.js：保留为低成本原型方案，不进入正式终端路径。官方文档给出的快速生产者处理吞吐和缓冲限制与本项目硬指标冲突。
- Electron：终端仍主要受 JavaScript/WebView 路径制约，且没有直接 Android 路线。
- Flutter：跨平台良好，但需要自研同等级终端模型和 GPU 文本视图，桌面工作台及 Rust 桥接成本并不低。
- `winit + wgpu + egui`：采用。纯 Rust、没有 C++ FFI，终端可直接控制 GPU 提交；代价是 winit/egui 仍在演进、非原生外观以及菜单、无障碍和平台集成需要补齐。通过锁定版本、适配层和三平台测试控制风险。
- 整体 fork WezTerm：终端成熟，但会继承庞大 GUI/mux 架构和持续上游合并成本。采用其 MIT 终端模型接口，而不是 fork 整个产品。
- Qt Quick：桌面控件、IME、无障碍和 Android 支持成熟，但增加运行库、QML、构建系统、C++/Rust FFI 和 LGPL 合规成本；它不会替代本项目必须自研的终端/日志 Surface，因此降为“若纯 Rust Phase 0 无法满足平台集成门槛时”的备选。

详细决策见 [ADR-0002](adr/0002-pure-rust-gui-stack.md) 和 [ADR-0003](adr/0003-desktop-platform-local-terminal-sync-input.md)。

## 6. 核心领域模型

### 6.1 主要实体

| 实体 | 关键字段 | 说明 |
|---|---|---|
| `Host` | id、name、address、port、tags、platform_hint | 机器本身，不含明文凭据 |
| `HostGroup` | id、name、parent_id、selector | 静态成员或基于标签的动态组 |
| `ConnectionProfile` | username、auth_ref、proxy_chain、algorithm_policy、keepalive | 可被多个 Host 复用 |
| `CredentialProfile` | kind、secret_ref、key_ref、agent_constraint | 凭据元数据，secret 存保险库 |
| `JumpProfile` | hops[] | 每跳独立主机校验和认证配置 |
| `SessionProfile` | terminal、encoding、theme、logging、startup | 可继承的终端会话设置 |
| `LocalProfile` | program、args、cwd、env、icon、integration | 本地 shell/程序启动配置，不包含 SSH 字段 |
| `Workspace` | windows、tab_groups、panes、bindings | 可恢复的工作区布局 |
| `SyncInputGroup` | member_terminal_ids、armed、policy、revision | 用户明确选择的已打开终端集合；只保存 UI 状态，不宣称一致执行 |
| `TaskDefinition` | selector、steps、policy、variables | 可复用的多主机任务定义 |
| `TaskRun` | task_snapshot、operator、started_at、status | 一次不可变执行快照 |
| `TargetRun` | host_snapshot、state、exit_code、timing、output_ref | 每台主机独立结果 |
| `TransferJob` | source、destination、checksum、commit_mode | 单机或多机文件任务 |
| `AuditEvent` | actor、action、target、time、result、correlation_id | 可追溯事件 |

所有实体使用 UUIDv7。任务开始时必须快照主机集合、命令、变量和策略；执行期间标签变化不能悄悄改变目标。

### 6.2 配置继承

优先级从低到高：

```text
全局默认 → 文件夹/标签策略 → SessionProfile → Host 覆盖 → 本次连接临时覆盖
```

合并规则按字段定义，不做通用深合并。列表字段需明确 `replace`、`append` 或 `remove`，避免端口转发、算法列表继承出不可预期结果。设置界面必须显示每个最终值的来源。

### 6.3 数据存储

- `cshell.db`：主机、分组、任务、布局、传输记录、审计索引。
- `known_hosts`：兼容 OpenSSH 语义，支持哈希主机名、`@cert-authority` 和 `@revoked`。
- `logs/`：用户明确开启的持久日志，按运行/会话保存 segment、原始流和索引。
- `runtime-output/`：默认启用的加密临时滚动层，用于大历史、GUI 重连和崩溃恢复；按配额和保留期清理。
- `vault`：只存 secret；默认根密钥由系统钥匙串保护。
- `exports/*.json`：版本化导出，不包含密码；用户显式选择时才导出加密 secret 包。

数据库迁移只前进，启动前做原子备份。大日志和文件内容不进 SQLite，只存元数据与内容寻址路径。

输出保留策略分为 `memory-only`、默认 `ephemeral-spill`、`persistent-audit`。默认临时 segment 使用每用户受限目录和保险库管理的临时密钥；正常退出、配额淘汰和保留期到期后清理。磁盘不足时先传播 SSH 背压并提示用户，绝不静默丢弃后继续显示“日志完整”。

## 7. 连接与会话设计

### 7.1 状态机

```text
Idle
  → Resolving
  → ConnectingProxy/JumpHop(n)
  → Negotiating
  → VerifyingHostKey
  → Authenticating
  → Ready
  → Reconnecting
  → Closing
  → Closed

任意阶段 → Failed(reason, retryability)
```

状态迁移产生事件，UI 不自行推测连接状态。取消使用层级化 cancellation token：关闭应用可取消全部，关闭连接取消其通道，取消任务只影响该任务创建的通道。

### 7.2 连接复用

SSH 协议允许一个底层连接承载：

- 一个或多个交互 PTY channel；
- 多个 exec channel；
- SFTP subsystem；
- 本地、远程和动态转发。

但 CShell 为故障隔离采用以下默认策略：

- 每个交互 SSH 标签默认使用独立 transport；用户可以显式启用同配置会话复用。
- 同一标签派生的 SFTP 可共享其 transport，也可在大传输时独立建连。
- 批量 exec、文件分发和跳板链使用有界连接池，不让所有目标压在一条 transport 上。
- 端口转发按生命周期组隔离，长期隧道不与短命批量任务强制共享。

复用键至少包含目标、用户名、跳板链、代理、认证身份和算法策略。不同身份或主机校验上下文不得复用。每连接设置 channel 上限、空闲回收和健康检查；服务端拒绝新 channel 时允许新建同配置连接。

### 7.3 重连语义

- 交互终端只能重建连接和 shell，不能声称恢复原远程进程；界面明确显示“新 Shell”。
- 端口转发可按策略自动恢复，并检查本地端口是否仍可绑定。
- SFTP 可从已确认偏移继续。
- exec 命令在“已发送但结果未知”后不得自动重试，除非任务步骤显式声明幂等和重试策略。

### 7.4 后台崩溃恢复

- `cshelld` 持久化已提交任务定义、目标快照和状态迁移；重启后未开始目标恢复为 `Queued`。
- daemon 退出时正在远端运行、但未取得最终 exit status 的目标恢复为 `Unknown`，禁止自动重跑。
- 交互 SSH shell 和本地 PTY 进程在 daemon 崩溃后通常不可接管，恢复为已断开并明确提示，而不是伪装成原会话。
- SFTP 只从已验证偏移恢复；GUI 重连通过最新 `FullFrame`、日志索引和持久任务状态重建界面。

### 7.5 本地终端

本地终端不是特殊的 UI 控件，而是另一种 `TerminalTransport`：

```text
TerminalSession
├─ SshPtyTransport
└─ LocalPtyTransport
```

- `cshelld` 负责启动、监控和结束本地进程；Windows 使用 ConPTY，Linux/macOS 使用 Unix PTY。
- `LocalProfile` 保存程序、参数、初始目录、环境变量覆盖、图标和可选 shell integration；配置中不保存运行时 secret。
- Windows 自动发现 PowerShell 7、Windows PowerShell、CMD、WSL 发行版、Git Bash；Linux/macOS 首先读取 `$SHELL`，再提供常见 shell 与自定义程序入口。
- 本地终端与 SSH 终端共用 VT 模型、主题、Xterm 256/True Color、字体、高亮、搜索、热/冷滚动、日志、标签、分屏和快捷键。
- GUI 崩溃时本地进程继续由 daemon 托管；关闭仍有前台进程的标签必须询问“关闭进程、仅关闭视图或取消”。
- 本地终端不能只保存 shell 主进程句柄。Windows 为 ConPTY 主进程创建启用 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的私有 Job Object，显式关闭使用 `TerminateJobObject`，daemon 进程退出导致句柄关闭时由内核清理后代；Linux/macOS 利用 PTY 启动前的 `setsid`，同时监督 shell 根进程组和通过 `tcgetpgrp` 取得且经 `getsid` 确认仍属于该 PTY session 的当前前台作业进程组，关闭时先发送 `SIGTERM`，宽限 300 ms 后发送 `SIGKILL`。macOS 组信号因组内任一不可授权成员返回 `EPERM` 时退回对已验证组长 PID 的信号，禁止对未经 session 校验的复用 PGID 发信号。监督初始化失败时必须终止并回收已启动的主进程，不能返回半托管会话。
- 进程生命周期测试必须让 PTY 主进程再派生真实孙进程；Unix 测试中的孙进程还必须切换到独立前台 PGID，并以持续心跳验证关闭后已经停止，不能只检查直接 shell 的退出状态。Windows Job Object 同时覆盖 daemon 正常退出与不可恢复崩溃；Unix 显式关闭覆盖 shell 与当前前台作业，主动脱离控制终端的后台 daemon 以及 `cshelld` 被 `SIGKILL` 后的自动清理由后续独立 guardian 方案验收，未完成前不得声称 Unix 异常退出无孤儿进程。
- clone 默认复制 Profile 并启动新进程，不声称复制原进程状态。shell integration 仅用于 cwd、命令边界和退出状态等增强信息，缺失时基础终端仍完整工作。
- 本地进程继承环境前先应用平台脱敏/覆盖策略；工作区恢复默认只恢复标签和 Profile，不自动重新执行上次命令。

## 8. 多主机管理与执行

这是 CShell 与普通 SSH 客户端拉开差异的核心。

### 8.1 两种模式必须分开

#### A. 同步输入模式

参考 Xshell Synchronized Input/Send Key Input，适合操作者把相同交互输入发送到多个已经打开的终端标签，并同时观察结果：

- 目标只能是用户明确勾选的已打开标签/分屏；支持当前窗口、当前标签组和临时自定义集合，不因主机标签自动把未打开会话加入。
- 进入同步状态必须显式 `Arm`；所有目标显示醒目边框，固定栏展示目标数量、名称和一键退出快捷键。
- 广播的是 `InputAction`，包括已提交文本、Enter/Tab、控制/组合键、方向键、功能键和安全粘贴；每个目标按自己的终端模式编码，不复制源标签的原始字节。
- IME 只广播最终提交文本；中间组合态、鼠标事件、窗口 resize 和被应用快捷键消费的按键默认不广播。
- 支持单行 Compose 和多行 Compose；发送前冻结目标快照并预览文本、换行、控制字符和目标清单。
- 各目标使用独立有界发送队列；一个慢连接不能阻塞其他标签，队列溢出必须对该目标报错，禁止静默丢键。
- 密码、私钥口令、keyboard-interactive 安全输入以及终端锁定状态永不广播；本地终端只有被逐个显式选中时才能加入，SSH与本地终端混合广播默认禁止。
- 多行、控制字符、超过配置数量的目标和疑似危险命令触发二次确认；同步状态不跨应用重启自动恢复。
- 仅可记录“操作者、时间、目标集合、输入类型与字节数”等审计元数据，默认不记录可能含 secret 的输入正文。
- 不保证各目标 shell、cwd、前台程序或终端模式一致，因此不提供退出码、重试和成功结论，不用于可靠自动化。

#### B. 批量任务模式

通过非 PTY exec channel 执行，具有确定的目标快照和每主机结果：

```text
Draft → Validating → AwaitingConfirmation → Queued → Running
      → Cancelling → Completed | PartialFailed | Failed | Cancelled
```

每台主机状态：

```text
Pending → Connecting → Running(step n) → Succeeded
                    └→ Failed | TimedOut | Cancelled | Unknown
```

### 8.2 调度策略

任务可配置：

- 最大并发数，例如 20；另设每跳板机并发上限。
- 分批执行，例如每批 10 台，批间暂停 30 秒。
- 滚动策略，例如上一批成功率达到 100% 才继续。
- canary 策略，先执行指定 1～3 台。
- 单步超时、总超时和排队超时。
- 失败策略：继续、暂停、停止后续批次。
- 重试次数、退避和仅可重试错误分类。
- 输出上限；超限内容落盘，UI 保留尾部和索引。

默认并发建议为 `min(20, target_count)`，不是无限并发。主机 DNS、跳板机和认证服务都可能成为共享瓶颈。

### 8.3 任务步骤

首版支持以下步骤类型：

1. `command`：执行命令，捕获 stdout、stderr、exit code。
2. `upload`：通过 SFTP 上传文件或目录。
3. `download`：收集远端文件，路径自动带主机标识。
4. `check`：断言退出码、正则、文件、哈希或结构化结果。
5. `pause`：人工确认后继续下一批。

后续增加 `template`、`sudo` 和受限 `expect`。任务定义支持变量，但模板默认不执行任意代码：

```yaml
name: check-disk
targets: "tag:prod AND tag:linux"
policy:
  concurrency: 20
  timeout: 30s
  strategy: all
steps:
  - command: "df -P {{ mount | shell_escape }}"
    env:
      LC_ALL: C
  - check:
      stdout_regex: "{{ expected_pattern }}"
```

所有插入 shell 的变量必须显式经过 `shell_escape`，未标注的动态变量默认拒绝执行。

### 8.4 取消与结果可信度

点击取消后按顺序执行：

1. 停止派发新目标和新步骤。
2. 对运行 channel 发送关闭请求；PTY 模式可按配置先发 SIGINT。
3. 等待宽限期后断开对应 channel，不能粗暴关闭被其他功能复用的 SSH 连接。
4. 无法确认远程进程是否终止时，状态记为 `Unknown`，不能记成 `Cancelled`。

### 8.5 结果界面

任务控制台同时提供：

- 摘要：成功、失败、超时、未知、运行中数量及耗时分布。
- 主机表：排序、筛选、错误分类、exit code、当前步骤。
- 聚合输出：相同输出折叠并显示主机集合。
- 差异视图：选择两台或两组主机比较输出。
- 实时流：按主机分栏或交错显示，保留来源和时间戳。
- 导出：JSON、CSV、纯文本；secret 和匹配脱敏规则的字段不导出。
- 失败重跑：创建新的 `TaskRun`，只选择失败目标，不篡改原运行记录。

## 9. SFTP 与多主机文件分发

### 9.1 单主机文件管理

- 本地/远端双栏、树和列表视图。
- 上传、下载、拖放、重命名、删除、新建目录、chmod、软链接展示。
- 并发队列、暂停、恢复、覆盖策略和传输速率。
- 远程文件临时下载到受控目录，外部编辑器保存后检测变化并回传。
- 大目录分页/增量加载；不在 UI 线程递归遍历。

### 9.2 安全文件分发

对多主机上传默认采用两阶段流程：

```text
上传到 .<name>.cshell-<run_id>.tmp
  → 可选校验 SHA-256
  → 可选备份原文件
  → 同目录原子 rename 提交
  → 失败时保留/清理临时文件（按策略）
```

跨文件系统 rename 不保证原子性，检测后必须降级并提示。权限、owner 和时间戳是否保留应显式配置。分发结果纳入 `TaskRun`，不能只显示一个全局进度条。

## 10. SSH 安全设计

### 10.1 算法策略

提供三个策略档：

- `Modern`：默认，只允许现代 KEX、AEAD/CTR、SHA-2 和 Ed25519/ECDSA/RSA-SHA2。
- `Compatible`：为旧服务器增加有限兼容算法，并在会话上显示警告。
- `Legacy`：独立可选构建特性或隐藏高级选项；必须逐主机确认和记录审计。

不支持 SSH1。任何 host-key 变化均阻断连接；用户必须看到旧/新指纹、算法和命中记录，不提供“永久忽略全部变化”。

### 10.2 凭据

- Windows：Credential Manager/DPAPI；macOS：Keychain；Linux：Secret Service；Android：Keystore。
- 会话只保存 `credential_profile_id`，不保存明文密码。
- 私钥可引用外部文件，也可导入加密保险库；默认不复制用户文件。
- 主密码模式使用 Argon2id 派生密钥，并记录可升级的参数版本。
- secret 进入 Rust 后使用受控类型和内存清理；不得进入普通日志、错误堆栈、任务变量快照或剪贴板历史。
- 锁屏后停止接受输入并清空 UI 中的敏感临时值；是否断开现有连接由策略决定。

### 10.3 认证与信任

分阶段支持：

- P0：password、public key、keyboard-interactive、OpenSSH agent、严格 known_hosts。
- P1：Pageant、agent forwarding、OpenSSH certificates/CA、PKCS#11。
- P2：GSSAPI/Kerberos、FIDO2、安全密钥、Windows CAPI（若用户需求和维护成本成立）。

跳板链的每一跳都单独验证 host key。目标主机校验不能错误地使用跳板地址作为 known_hosts 键。

### 10.4 自动化权限

脚本和触发器采用 capability 模型：`terminal.write`、`session.open`、`task.run`、`sftp.read`、`sftp.write`、`process.spawn`、`network.connect` 分开授权。新脚本首次使用高风险能力时提示授权；后台触发器默认不能启动本地程序或读取保险库。

## 11. 终端与工作区功能

### 11.1 终端体验

- UTF-8 为默认，提供常用编码转换；非法字节可视化而非静默丢弃。
- xterm/VT 系列常用序列、256 色和 True Color、鼠标协议、bracketed paste，由 Rust 终端模型解析。
- 活动屏幕、热滚动窗口和磁盘冷滚动层分级保存，不把大历史常驻内存。
- 普通/正则搜索、大小写、全词、结果导航和当前视口标记。
- 字符/行/列选择，双击分隔符配置，多字节复制粘贴。
- 字体回退、ASCII/CJK 双字体、字距、行距、光标、主题和透明度。
- 安全粘贴、多行预览、粘贴节流、右键行为配置。
- URL/IP/路径识别，高亮集，铃声/桌面通知。
- 原始字节十六进制查看，ANSI 日志回放和导出。

### 11.2 输出与滚动存储

每个交互会话维护三层数据：

1. `ActiveScreen`：主屏/备用屏、光标和终端模式，完全驻内存。
2. `HotScrollback`：最近可配置行数的 cell ring，支持即时向上滚动。
3. `ColdScrollback`：已离开活动区域的逻辑行和样式 run，追加到磁盘 segment，并建立 `LineId → offset` 索引。

同时保留按序号和时间戳记录的原始字节 journal。它用于审计、Hex 查看和解析器回归；冷滚动层用于快速展示，二者不能互相替代。批量任务 stdout/stderr 直接进入分段日志，不创建终端模型。

分段文件建议为 64～256 MiB，配套 64 位行偏移、时间、stream、样式和校验信息。后台可生成可寻址压缩段，但当前写入段保持追加友好。SQLite 只保存 segment 元数据，不保存日志正文。

### 11.3 “不卡顿、不跳跃”的确定语义

- 位于底部时为 `FollowTail`，新输出按显示刷新率合帧，展示最新正确终端状态。
- 用户一旦向上滚动，切换为 `Anchored(LineId, cell_offset)`；新输出只增加“新增 N 行”提示，当前内容绝不移动。
- 调整窗口宽度时以逻辑行 ID 和字符偏移恢复锚点；重排在后台计算并原子切换。
- 远端用 `CR`、光标移动更新进度条时，更新同一屏幕 cell，不错误追加成大量新行；原始变化仍在 journal 中。
- 磁盘页未就绪时使用固定高度占位，数据返回后不得改变锚点或滚动方向。
- 屏幕刷新可以合并中间帧，但不得跳过 VT 解析、逻辑行提交或原始字节落盘。

### 11.4 原生视口渲染

`TerminalSurface` 和 `LogSurface` 都遵循：

- 只读取可见行及上下少量 overscan，复杂度与文件总行数无关。
- 一个视口形成少量 GPU draw batch，不创建“每字符/每行一个 egui Widget”。
- 字形 rasterize 和 shaping 结果进入受限 LRU atlas；ASCII 快路径与 CJK/Emoji 回退分开。
- parser、磁盘读取、搜索和 shaping 不在 GUI 事件线程执行。
- GUI/渲染线程之间使用不可变 frame snapshot 或 triple buffer，渲染线程不等待 SSH/磁盘锁。
- GPU device lost 或驱动黑名单时切换软件路径；功能保持正确，允许性能下降。

### 11.5 工作区

- 多标签、水平/垂直分屏、标签组、跨窗口移动和会话克隆。
- 可停靠的会话、SFTP、隧道、快捷命令、任务和日志面板。
- 工作区自动保存与崩溃恢复；恢复布局不代表自动提交凭据或执行命令。
- Quick Start 显示最近会话、收藏、主机搜索和任务入口。
- 快捷键可映射到命令、文本、脚本或任务，但冲突必须可检测。

## 12. 快捷命令、触发器与脚本

### 12.1 快捷命令

快捷命令是结构化动作，不只是字符串：

- 发送文本到当前会话或选中会话。
- 在 Compose 中打开，用户确认后发送。
- 启动批量任务。
- 运行脚本并传参数。
- 按系统、标签和目录组织命令集。

### 12.2 触发器

触发器在解码后的输出流上进行跨数据包匹配，支持普通文本和受限正则。动作包括高亮、通知、铃声、追加文本、启动脚本和记录事件。必须有防循环、冷却时间、每分钟上限和作用域，否则“匹配输出后发送文本”容易形成无限反馈。

### 12.3 脚本

推荐内嵌 QuickJS，提供跨平台 JavaScript API：

```javascript
const run = await cshell.tasks.run({
  selector: 'tag:web && env:prod',
  command: 'systemctl is-active nginx',
  concurrency: 10,
});
await cshell.ui.showResults(run.id);
```

脚本在独立 worker 中运行，具备超时、内存上限、取消和 capability。桌面端可在 P2 提供外部 Python 适配器，但不把 Python 解释器打进核心。Windows VBScript 仅考虑迁移辅助，不作为跨平台公共 API。

脚本录制只记录 CShell 层的操作和可识别的输入/等待事件，生成前必须脱敏；不能把密码或 keyboard-interactive 回答写进脚本。

## 13. 导入、导出与 CLI

### 13.1 导入

按优先级支持：

1. `~/.ssh/config`、known_hosts、OpenSSH key。
2. Xshell session/export 文件。
3. PuTTY registry/PPK。
4. SecureCRT、MobaXterm 的可公开解析配置。

导入流程为“解析 → 预览 → 冲突处理 → 提交”，失败不能产生半条会话。加密密码若无法合法解密，则只导入元数据并要求用户重新绑定凭据。解析器使用公开样本和用户提供文件做 clean-room 实现，不复制商业产品代码。

### 13.2 导出

- 默认导出不含 secret 的版本化 JSON。
- 可选生成 OpenSSH config 兼容子集。
- 含 secret 的导出使用单独密码加密包，并明确风险、算法和格式版本。
- 任务运行支持 JSON/CSV/text 报告，输出可按脱敏规则处理。

### 13.3 CLI 示例

```text
cshell open prod-web-01
cshell ssh user@host --jump bastion
cshell task run check-disk --targets 'tag:prod' --concurrency 20
cshell task status <run-id> --json
cshell sftp put ./app.tar '/tmp/app.tar' --targets 'group:web'
cshell import openssh ~/.ssh/config
```

CLI 调用和 UI 操作走同一 application service，不允许另写一套 SSH 逻辑。

## 14. GUI、核心与渲染通信

控制面采用类型化请求/响应：

```text
session.create / session.connect / session.resize / session.close
task.validate / task.start / task.cancel / task.retry_failed
sftp.list / sftp.enqueue / sftp.pause / sftp.cancel
vault.unlock / vault.lock
```

原始终端字节停留在 `cshelld`，数据面向 GUI 发布有版本的视图数据：

```text
FrameDelta {
  session_id, generation, base_generation,
  dirty_rows[], cursor, modes, scroll_metrics
}
FullFrame      { session_id, generation, visible_rows[], cursor, modes }
LogPage        { source_id, anchor_line_id, rows[], has_before, has_after }
TaskSummary    { run_id, revision, counters, changed_targets[] }
TransferEvent { job_id, revision, state, bytes_done, bytes_total }
```

要求：

- Windows 使用当前用户 ACL 保护的 Named Pipe，Linux/macOS 使用权限为 `0600` 的 Unix Domain Socket；握手校验用户与随机实例令牌。
- delta 只能应用到指定 `base_generation`；版本不匹配立即请求 `FullFrame`。
- 有界队列与高/低水位背压覆盖 SSH channel、journal writer 和 parser mailbox；磁盘跟不上时必须减小 SSH receive window，不能丢输出。
- parser 可持续处理全部字节，但 GUI 最多按显示刷新率接收合并后的 cell 变化。
- GUI 长时间卡住时丢弃过时的“渲染快照”而不是原始数据；恢复时取最新完整快照。
- 控制事件优先于输出事件，取消操作不能被大量 stdout 淹没。
- 批量任务 UI 默认订阅摘要和选中目标尾部，绝不同时推送全部目标的完整 stdout。
- GUI 进程只消费类型化、版本化 IPC DTO；进程内渲染使用 `Arc<FrameSnapshot>` 或拥有明确生命周期的只读 buffer handle，不跨线程泄漏可变引用。

## 15. 性能、可靠性与可观测性

### 15.1 首版性能预算

基准机定义为：8 核主流 CPU、16 GiB 内存、NVMe、集成 GPU、1920×1080、60 Hz。网络协议吞吐和纯渲染吞吐分别测试；不得用本机 synthetic feed 的成绩冒充 SSH 端到端成绩。

| 指标 | 目标 |
|---|---:|
| 冷启动到可操作 | 主流开发机小于 2 秒 |
| 键盘事件到数据发出 | P95 小于 5 ms；输出洪泛时 P99 小于 20 ms |
| 收到回显到画面呈现 | 正常负载 P95 小于 16.7 ms，P99 小于 33 ms |
| 单终端持续输入管线 | 50 MB/s 持续 60 秒：不丢字节、不 OOM、控制输入仍响应；100 MB/s 为进阶目标 |
| 高吞吐画面 | 不积压超过 2 个待呈现 frame；允许合并中间帧，但最终屏幕与滚动历史正确 |
| 10 GB 已有日志首屏 | 小于 500 ms，且内存增量小于 250 MiB |
| 10 GB 日志随机跳转 | 已有索引时 P95 小于 100 ms |
| 日志连续滚动 | 60 Hz 下 P95 frame 小于 16.7 ms、P99 小于 33 ms，无内容锚点跳动 |
| 搜索与索引 | 后台执行、可取消；搜索期间 GUI frame 和输入延迟不突破上述预算 |
| 同时保持空闲 SSH 连接 | 技术预览 500 条稳定 1 小时；v1.0 稳定至少 24 小时 |
| 批量 exec | 1,000 主机、并发 50，可完成并给出逐主机状态 |
| SFTP 大文件 | 不把完整文件载入内存，内存随块大小而非文件大小增长 |
| GUI 崩溃恢复 | `cshelld` 中任务和传输继续；GUI 重启可恢复最新终端帧与日志位置 |

显示器每秒只能呈现有限帧数，因此“完整”定义为：每个字节都被持久化和解析、每个提交到滚动历史的逻辑行均可回看、最终屏幕状态正确；不要求把高于刷新率的每个中间进度动画逐帧显示。

这些是带失败门槛的工程目标，不是未经测量的承诺。Phase 0 未达到 50 MB/s、10 GB 日志随机访问和稳定滚动目标时，不进入完整产品开发。

### 15.2 日志分层

- 应用诊断日志：结构化、滚动、默认脱敏。
- 会话 journal：原始字节、方向、序号和单调时钟，用于精确回放与审计。
- 渲染滚动层：逻辑行、cell/style runs、换行关系和稳定 `LineId`，用于快速浏览。
- 批量任务日志：stdout/stderr 分流、目标 ID、步骤、时间、exit code，按 segment 存储。
- 任务审计：定义快照、目标快照、操作者、状态迁移和结果摘要。
- 安全事件：host key 变化、legacy 算法、保险库解锁、脚本授权。

日志中的命令可能本身含 secret，任务定义允许标记敏感参数；UI 和落盘日志均用占位符替换。不能只依赖正则事后清洗。

### 15.3 并行流水线

```text
Tokio SSH I/O
  → Session ingress queue（有界）
  ├→ Journal writer（追加写、校验）
  ├→ VT parser strand（同一会话严格有序）
  │    ├→ ActiveScreen / HotScrollback
  │    ├→ ColdScrollback segment builder
  │    └→ immutable FrameSnapshot
  └→ Trigger matcher / metrics

GUI event thread → 输入与布局，不解析输出
wgpu render loop → 读取最新 snapshot，合成 egui 与自定义 Surface
Search pool      → mmap/page cache 上增量搜索，可取消
```

同一会话的 VT 解析必须串行以保持顺序，不等于每会话占一个 OS 线程。多个会话可并行分配到固定 parser worker pool。任何阶段队列达到高水位都向上游传播背压；只有可重建的过时 frame snapshot 可以丢弃，原始输出、解析事件和逻辑历史不能丢弃。

## 16. 测试策略

### 16.1 测试金字塔

- 单元测试：状态机、继承合并、selector、shell escaping、脱敏、重试判定。
- 属性/模糊测试：SSH 配置导入、终端字节边界、触发器、路径处理和 IPC 解码。
- 协议集成：容器矩阵覆盖 OpenSSH 多版本、不同算法、认证、SFTP 和跳板链。
- 故障注入：断网、半关闭、跳板重启、磁盘满、权限变化、输出洪泛、取消竞态。
- UI 端到端：三平台的输入法、复制粘贴、分屏、快捷键、恢复和多主机结果。
- 本地终端端到端：Windows ConPTY、Linux/macOS PTY、shell发现、cwd/env、退出码、GUI崩溃续存和daemon崩溃断开语义。
- 安全测试：依赖审计、secret 泄漏扫描、路径穿越、恶意 ANSI/OSC、脚本逃逸。

### 16.2 关键回归用例

1. host key 首次接受、匹配、变化、撤销和 CA 签名。
2. 同一跳板连接下大量目标并发时的限流与隔离。
3. 命令已发送后断线，结果必须是 `Unknown` 而不是自动重试成功。
4. 取消一个任务不会关闭其他终端复用的连接。
5. SFTP 临时文件校验失败不覆盖目标文件。
6. GB18030/Shift-JIS/UTF-8 分片跨包时不乱码、不触发错误匹配。
7. 多行粘贴和同步输入不会广播密码输入；IME只发送提交文本，组合键/功能键按各目标模式编码，单个慢目标不会阻塞其他目标。
8. 10 GB 日志可增量搜索，不一次性载入内存。
9. 用户向上滚动后持续注入输出，`Anchored(LineId, cell_offset)` 内容逐像素保持不动。
10. 高吞吐时反复调整窗口宽度，后台 reflow 不改变逻辑锚点且不阻塞输入。
11. 强制结束 GUI 后 `cshelld` 继续任务；重启 GUI 能用 full snapshot 恢复并继续应用 delta。
12. 模拟 GPU device lost、字形 atlas 抖动和软件回退，不能导致终端状态或选择范围损坏。
13. 模拟磁盘慢、磁盘满和 segment 校验失败，系统正确背压、告警和恢复，不伪造完整性。

## 17. 建议仓库结构

```text
CShell/
├─ apps/
│  ├─ desktop/                 # Rust：winit + wgpu + egui
│  └─ android/                 # 桌面 v1.0 后，复用 core/renderer 的独立移动壳
├─ daemon/                     # cshelld 桌面后台进程
├─ crates/
│  ├─ cshell-domain/           # 实体、状态机、错误模型
│  ├─ cshell-application/      # 用例编排
│  ├─ cshell-ssh/              # SshProvider 与 russh 适配
│  ├─ cshell-sftp/             # 文件操作、队列、校验
│  ├─ cshell-terminal/         # alacritty_terminal适配、screen/scrollback/InputEncoder
│  ├─ cshell-local/            # LocalProfile、ConPTY/Unix PTY 与进程监督
│  ├─ cshell-render/           # wgpu、cosmic-text、glyph atlas、Surface
│  ├─ cshell-ui/               # egui 工作台、布局和平台 UI 适配
│  ├─ cshell-theme/            # Xterm 调色板、主题、字体与 .xcs 转换
│  ├─ cshell-highlight/        # 搜索/关键字/语义高亮 span，不执行触发器动作
│  ├─ cshell-output-store/     # journal、segment、行索引、搜索
│  ├─ cshell-orchestrator/     # 多主机调度和结果聚合
│  ├─ cshell-automation/       # 快捷命令、触发器、脚本
│  ├─ cshell-storage/          # SQLite、迁移、日志索引
│  ├─ cshell-vault/            # 钥匙串、加密保险库
│  ├─ cshell-platform/         # PTY、通知、剪贴板等适配
│  └─ cshell-ipc/              # 类型化 IPC DTO
├─ cli/
├─ tests/
│  ├─ protocol-lab/
│  ├─ compatibility/
│  └─ e2e/
├─ docs/
│  ├─ adr/
│  ├─ threat-model/
│  ├─ DEVELOPMENT.md           # 开发实施、任务编号和验收基线
│  └─ DESIGN.md
├─ xtask/                      # 构建、资源生成和性能基准入口
└─ Cargo.toml
```

依赖方向固定为 `GUI/CLI → application → domain`，基础设施实现 domain/application 定义的端口。`domain` 不能依赖 winit、wgpu、egui、SQLite、alacritty_terminal 或 russh。桌面 UI 依赖项目自有的 render/application facade，不直接引用 SSH 或存储实现。

## 18. 分阶段路线

### Phase 0：性能、平台与协议 PoC，8～10 周

必须通过以下硬门槛再进入正式开发：

- Windows x64、Linux x64（X11/Wayland）、macOS arm64/x64 的 `winit + wgpu + TerminalSurface` 中文输入、组合键、剪贴板、无障碍基础语义和 GPU/软件回退。
- `alacritty_terminal` → frame snapshot → `wgpu` 的完整纯 Rust 垂直切片，并与 egui 工作台共享一帧合成。
- `LocalPtyTransport → alacritty_terminal → FrameSnapshot` 垂直切片：Windows ConPTY、Linux/macOS PTY、窗口resize、host response、退出码与GUI崩溃续存。
- 50 MB/s × 60 秒输出无丢失、输入仍响应；10 GB 日志首屏、跳转和稳定滚动达标。
- `FollowTail/Anchored`、后台 reflow、GUI 崩溃重连语义通过自动测试。
- `cshelld` Named Pipe/UDS、实例认证、版本化 frame delta 和 full snapshot。
- 100 SSH 连接并发，GUI 不订阅输出时连接与任务正常运行。
- russh 的 password/key/keyboard-interactive、SFTP、跳板、L/R/D 转发。
- known_hosts 严格校验和 agent/Pageant。
- Windows/macOS/Linux 钥匙串及保险库原型；macOS 两种架构和 Linux 两种窗口系统均有物理机或虚拟机冒烟测试。

输出为 ADR、火焰图、帧时序和可重复 benchmark，不做漂亮 UI。终端管线或 SSH provider 任一未通过都必须在此阶段更换，不能拖到产品后期。

资源门禁分为两个层级。每次提交的四目标原生 CI 使用固定且适合 GitHub 标准 runner 的压力档：100 个停止读取的已认证订阅持续 15 秒、20,000 次动态图集淘汰，以及 1 GiB journal 的写入、1,000 次随机冷分页和检查点重开；四个平台使用相同正确性和内存上限，硬件/集成 GPU 使用相同延迟上限，CPU 软件适配器使用下述独立退化上限。正式验收仍执行 100 客户端持续 60 秒、完整 60 秒输出管线和 100 GiB journal 容量门禁。标准 GitHub runner 仅有 14 GB SSD，CI 压力档不能替代在本地或专用大容量 runner 上执行的正式容量门禁。

四目标原生 CI 同时执行回显延迟门禁：复用已初始化的离屏 wgpu 设备、固定容量字形图集和生产 `terminal.wgsl`，分别以受控真实本地 PTY 子进程及完成密码认证和严格 SHA-256 主机密钥校验的回环 SSH2 交互 PTY 产生 200 个正式回显样本（另有 10 次预热，且拒绝少于 100 个样本的 P99 门禁）。计时从输入提交开始，覆盖 transport、journal、ANSI 解析、不可变快照、可见区字形/顶点构建与上传、真实 GPU draw submission 及该 submission completion；物理/集成 GPU 的 P95 必须小于 16.7 ms、P99 必须小于 33 ms。GitHub Windows/Linux runner 没有目标级 GPU 时，只有 CI 显式设置 `CSHELL_ALLOW_SOFTWARE_GPU=1` 才允许 CPU fallback adapter，并执行 P95 小于 25 ms、P99 小于 50 ms 的退化保护门禁；该结果只证明软件适配路径、完整 submission/completion 和无严重回归，不能替代硬件性能验收。所有档位均单独报告 GPU completion 分位数。SSH 客户端连接默认启用 `TCP_NODELAY`，避免交互式小包被 Nagle 与 delayed ACK 组合引入约一个确认周期的额外延迟。正式门禁只能通过 `cargo xtask echo-latency-bench`（或包含它的 `native-ci-bench`）启动，普通 `cargo test --all-targets` 只验证基准目标可执行，不能因无 GPU 的通用测试 runner 误报失败；无窗口门禁优先申请高性能适配器，仅在申请失败时显式申请 fallback adapter，Linux 原生门禁 runner 安装 Mesa Vulkan 软件适配器并仍执行完整提交与完成等待。该无窗口门禁不声称覆盖桌面 compositor 或显示器扫描输出，真实 window surface/present 仍由各平台窗口门禁验收。

### Phase 1：可用的 SSH 与本地终端客户端，14～16 周

- 会话 CRUD、文件夹/标签、Quick Start。
- SSH 终端、多标签、分屏、重连提示、基础主题和搜索。
- LocalProfile、Windows PowerShell/CMD、Linux/macOS 默认 shell；WSL、Git Bash与自定义程序发现。
- 热/冷滚动缓冲、分段 journal、原生日志视口和稳定滚动锚点。
- 密码/密钥/agent、known_hosts、代理和单级跳板。
- 会话日志和基础导入导出。
- Windows/Linux/macOS 每日构建和协议集成测试。

### Phase 2：多主机核心，10～12 周

- 静态/动态主机组、Xshell式已打开标签同步输入、单/多行 Compose 和快捷命令。
- 同步目标 Arm/Disarm、目标快照、逐目标队列与错误、逻辑按键编码、安全输入禁止广播和视觉警示。
- 批量 exec、并发/分批/canary、超时/取消、结果聚合。
- 任务定义、运行快照、审计、JSON/CSV 导出。
- 多主机 SFTP 上传/下载、校验和原子提交。
- CLI 运行和查询任务。

完成后即可发布第一个真正有差异化价值的版本。

### Phase 3：SSH/SFTP 功能完整，14～18 周

- 完整 SFTP 双栏、断点、远程编辑、队列。
- 多级跳板、端口转发管理、OpenSSH CA、PKCS#11。
- 高亮、触发器、通知、日志解释/回放/Hex。
- 工作区分离与恢复、配置继承、批量编辑。
- Xshell/PuTTY/SecureCRT/MobaXterm 导入。
- JavaScript 自动化与脚本录制。

### Phase 4：三平台桌面稳定化，12～16 周

- 500 长连接与 1,000 主机批量任务压测。
- GPU 驱动矩阵、软件渲染回退、10～100 GB 日志和长时间输出老化测试。
- 可访问性、国际化、故障注入和安全审计。
- Windows x64、Linux X11/Wayland、macOS arm64/x64 的24～72小时长连接、本地PTY、休眠唤醒、网络切换、IME和视觉回归。

### Phase 5：桌面 v1.x 扩展，按需求排期

- X11 forwarding：SSH capability、xauth cookie、安全策略和隧道状态；Linux使用系统X Server，Windows/macOS依赖用户安装的外部X Server。
- Windows ARM64、Linux ARM64在有明确用户需求和对应CI/物理测试机后加入。
- 不为实现X11而内置或维护X Server。

### Phase 6：Android，桌面 v1.0 后独立立项

- 复用SSH/SFTP核心、终端模型、主题和部分renderer，重新设计触摸、软键盘、生命周期和文件访问。
- 首版只做终端、会话、轻量SFTP、任务查看/触发；不承诺桌面级多窗口、后台长连接或所有硬件密钥能力。

估算：原生终端、大日志、本地PTY和四个首发目标架构增加约 6～9 人月。6～8 人全职团队约 18～24 个月达到成熟三平台桌面版本；2～3 人约 30～42 个月；单人通常需要 4～6 年。最大工作量不在“连上 SSH”，而在终端细节、GPU/字体/输入法、本地PTY、多平台兼容、多主机执行语义和长期稳定性；X11、其他架构和Android不计入桌面v1.0估算。

## 19. 团队建议

- 2 名 Rust/网络工程师：SSH、SFTP、调度、终端模型、存储与安全。
- 2 名 Rust 桌面工程师：egui 工作区、任务、文件 UI、winit 和平台集成。
- 1 名 GPU/文本渲染工程师：wgpu、cosmic-text、字形 atlas、CJK/Emoji、日志视口。
- 1 名跨平台/质量工程师：CI、PTY、输入法、系统集成和 E2E。
- 1 名产品/UX：复杂运维工作流、危险操作防护和可用性。
- 安全与协议顾问可阶段性参与密码学、SSH 兼容和威胁建模。

## 20. 风险与应对

| 风险 | 影响 | 应对 |
|---|---|---|
| Rust SSH 库在长尾服务器上的兼容问题 | 核心阻塞 | Phase 0 协议矩阵；`SshProvider` 隔离；保留替换后端能力 |
| 原生终端与日志渲染工作量被低估 | 延期或性能不达标 | Phase 0硬门槛；复用alacritty_terminal；限制首版图像协议；独立GPU工程师 |
| alacritty_terminal API 或内存滚动模型不满足冷存储 | 被上游接口锁定 | 项目适配层、锁定发布版、差分测试；必要时维护最小fork或替换terminal provider |
| GPU 驱动、字体和 IME 平台差异 | 卡顿、崩溃或显示错误 | wgpu 公共 API；驱动矩阵；atlas 上限；软件回退；三平台 E2E |
| winit/egui API 演进或控件成熟度不足 | 升级成本、平台行为不一致 | 锁定版本；项目适配层；视觉/输入回归；不把高吞吐 Surface 建在 egui 文本控件上 |
| 原生菜单、无障碍和系统集成缺口 | 桌面体验不完整 | AccessKit 与窄平台适配 crate；Phase 0 验证 IME、剪贴板、拖放、菜单和读屏；未达门槛才重评 Qt |
| 同步输入被误当成可靠批量执行 | 生产事故 | 两种模式分离；视觉警示；批量任务走 exec 和审计 |
| 不同目标终端模式或慢连接导致同步输入错乱 | 部分标签收到错误/缺失按键 | 广播逻辑 InputAction、逐目标模式编码、有界独立队列、逐目标失败提示；禁止静默丢键 |
| 本地PTY跨平台行为差异或失控子进程 | 终端卡死、孤儿进程或恢复错误 | daemon进程监督、进程树测试、退出宽限期；GUI崩溃续存，daemon崩溃明确断开 |
| 自动重试重复执行命令 | 数据破坏 | 结果未知不自动重试；显式幂等声明；运行快照 |
| 多主机输出耗尽内存 | 崩溃 | 有界队列、落盘、尾部窗口、流控和每目标上限 |
| 凭据泄漏到日志/脚本/导出 | 严重安全问题 | secret 类型、结构化脱敏、禁止默认导出、泄漏回归测试 |
| 跳板机成为并发瓶颈 | 大规模任务失败 | 每跳板限流、连接复用、退避、分批和可观测指标 |
| “全面对标”无限扩张 | 长期无法交付 | 以本文件边界和分阶段验收锁定范围 |
| 商业产品格式或商标风险 | 法律/品牌风险 | clean-room 导入；不复制 UI/代码；公开前做名称和许可证审查 |

## 21. 版本验收标准

### v0.1 技术预览

- Windows x64、Linux x64、macOS arm64/x64连接OpenSSH，支持key/password/agent和严格host key校验。
- 三平台本地终端可启动默认shell、正确resize/退出并在GUI重启后继续；Windows同时覆盖PowerShell与CMD。
- 20 个并发交互会话稳定工作，终端输入、复制、搜索、分屏正常。
- 50 MB/s 输出、10 GB 日志视口和滚动锚点达到 Phase 0 指标。
- SFTP 基础上传下载不整文件入内存。

### v0.5 多主机预览

- 用户可以显式选择多个已打开标签，广播文本、组合键、功能键和Compose内容；目标始终可见，安全输入不广播，慢目标逐项报错。
- 用标签选择 100 台测试主机，限制并发执行命令。
- 每台主机具备独立状态、stdout/stderr、exit code 和耗时。
- 断网、超时和取消后状态符合本设计，不出现虚假成功。
- 多主机文件分发支持临时文件、校验和原子提交。

### v1.0 桌面稳定版

- Phase 1～4 功能完成，三平台行为一致或有明确平台说明。
- 500 空闲长连接稳定至少24小时；1,000目标任务并发50完成；本地PTY与活跃SSH混合老化24～72小时。
- GUI 崩溃不终止后台任务；恢复后 frame generation、滚动位置和日志均正确。
- 性能预算表所有 P95/P99、吞吐、内存及 10 GB 日志指标通过基准机与低配机测试。
- 协议、导入、故障注入和 UI 回归矩阵通过。
- 完成威胁模型、依赖许可证清单、第三方安全审计和恢复演练。
- 不含自动更新、签名和发布工作的缺失不影响上述验收。

## 22. 立刻可执行的下一步

1. 并行实现两条纯Rust垂直切片：`SSH synthetic feed → journal → alacritty_terminal → FrameSnapshot → wgpu TerminalSurface` 与 `Local PTY → alacritty_terminal → FrameSnapshot`。
2. 建立 20～100 个 OpenSSH 容器的协议实验室，覆盖跳板、算法、丢包和 SFTP。
3. 生成 10 GB ANSI/UTF-8/CJK/超长行/进度条语料，建立可重复 benchmark 与视觉回归。
4. 完成四个首发target的CI/测试机矩阵，并验证Windows ConPTY、Linux X11/Wayland、macOS arm64/x64的IME与GPU路径。
5. 完成 ADR：SSH provider、secret存储、IPC/流控、输出segment、滚动锚点、daemon恢复和任务取消语义。
6. 在做完整 UI 前验证50 MB/s、10 GB随机访问、GUI/daemon崩溃恢复、100连接、本地PTY和任务取消。

## 23. 参考基线

- [Xshell 8 官方完整功能列表](https://www.netsarang.com/en/xshell-all-features/)：用于确定 SSH/SFTP 范围内的对标项。
- [Xshell 官方产品页](https://www.netsarang.com/en/xshell/)：会话、快捷命令、触发器、Compose、认证配置和文件管理基线。
- [Xshell 8 Manual](https://www.netsarang.com/docs/Xshell8_manual.pdf)：Send Key Input、Compose Pane、本地Shell和多会话终端功能基线。
- [Xshell Synchronized Input](https://op-www.netsarang.com/products/xsh_key_features.html)：向多个选中终端发送普通按键、组合键和功能键的交互语义依据。
- [xterm.js 官方流控说明](https://xtermjs.org/docs/guides/flowcontrol/)：用于确认 Web 终端快速生产者、吞吐和缓冲风险。
- [WezTerm Terminal Core](https://github.com/wezterm/wezterm/blob/main/term/README.md)：VT 模型、滚动缓冲及输入编码能力基线。
- [WezTerm](https://github.com/wezterm/wezterm)：纯 Rust、GPU 加速、三桌面平台终端的工程可行性基线。
- [winit FEATURES](https://github.com/rust-windowing/winit/blob/master/FEATURES.md)：窗口、输入、IME 与桌面/Android 平台覆盖及能力边界。
- [wgpu](https://github.com/gfx-rs/wgpu)：DX12、Vulkan、Metal、GL 和 Android 图形后端依据。
- [cosmic-text](https://github.com/pop-os/cosmic-text)：纯 Rust shaping、字体回退、双向文本和字形栅格化基线。
- [AccessKit](https://github.com/AccessKit/accesskit)：跨平台可访问性与 winit 适配路径。
- [egui](https://github.com/emilk/egui)：纯 Rust 工作台控件、wgpu/winit 集成及 Android 支持基线；仅用于非性能关键 UI。
- [russh 官方仓库](https://github.com/Eugeny/russh)：异步 SSH2、算法、转发、证书、Pageant 和 SFTP 生态能力。
