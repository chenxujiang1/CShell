# CShell 产品与技术设计方案

> 状态：Draft 1.0  
> 日期：2026-08-28  
> 首发平台：Windows x64、Linux x64、macOS arm64/x64  
> 后续平台：Windows/Linux ARM64、Android  
> 核心范围：SSH、SFTP、本地终端、多标签同步输入与批量执行
> 开发实施计划与进度记录仅在本地维护，不纳入 Git。

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

- RDP、VNC、串口、TELNET、RLOGIN、FTP、X/YMODEM。
- SSH1，以及默认启用 RC4、3DES、DSA、SHA-1 等过时算法。
- 自动更新、代码签名、商店发布、安装包发布流程。
- 首版云账号、云同步、团队服务端、堡垒机服务端。
- 首版不实现 X11 转发，但在 `SshProvider`、会话配置和隧道模型中预留能力；桌面 v1.0 稳定后实现，不内置 X Server。
- 在客户端中内置完整 Ansible/SaltStack；CShell 是客户端与轻量编排工具，不是配置管理平台。

### 2.3 “全面对标”的定义

对标指 SSH/SFTP 工作流的功能等价，不追求菜单名称、私有文件格式或脚本 API 的逐字兼容。验收以用户任务为单位：同样的连接、管理、操作和自动化目标能够完成，并且多主机能力优于单纯同步输入。

### 2.4 功能优先级矩阵

`P0` 是首个可用版本，`P1` 构成多主机产品核心，`P2` 达到成熟桌面对标，`P3+` 是桌面 v1.0 后的 X11、其他架构和 Android。

本节的 `P0/P1/P2` 是功能优先级，不等同于第 18 节及开发计划中的工程 `Phase 0/1/2`。首个可日常使用的单主机版本仍在工程 Phase 1 完成后验收。

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

设置弹窗和二级菜单按[ADR-0077](adr/0077-bounded-dialogs-and-connection-settings-pages.md)统一视口边界、字段宽度、滚动和按钮换行；连接设置按常规/认证/路由/主机密钥/终端分类，底部保存与连接独立于内容滚动。SFTP 按文件、队列、编辑、历史分类。自动布局检查与实际视觉/完整工作流验收分别记录。

桌面工作台按[ADR-0076](adr/0076-classic-terminal-workbench-layout.md)采用经典会话树/分组标签/大终端布局。主窗口只保留常用连接、切换与查找操作；主机批量管理、CA、Provider、脚本、继承与恢复经分类菜单进入，避免功能横栏和重复标签挤占终端。保存的逻辑文件夹、SSH与本地配置进入左侧树，单击选择、双击连接、右键编辑。13项工作台布局/交互测试只证明相应实现条件，用户试用和完整GUI工作流对标验收仍须完成。

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

终端 AccessKit 节点只在系统读屏激活后生成，不进入常规回显热路径；节点包含当前可见屏幕文本、光标与选择位置，并与键盘焦点同步。语义文本限制为 200 行、每行 240 列及 32 KiB UTF-8，超限时明确标记截断；历史日志和滚动缓冲不作为单个无界节点输出。

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
| `Host` | id、name、address、port、profile_id、tags、labels、platform_hint | 独立目的地址，引用保存的 SSH Profile，不含明文凭据 |
| `HostGroup` | id、name、parent_id、membership | 静态 Host ID 集合或严格 selector；父组仅分类 |
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

P2-040 按 [ADR-0021](adr/0021-host-inventory-selectors-and-frozen-targets.md) 实现独立主机库、静态/动态组、精确 selector 与不可变目标快照。查询支持 AND/OR/NOT、符号运算符、括号及 tag/env/label/platform/name/address/host/profile/group；空查询、非法语法或缺失引用整次拒绝，不回退全部目标。主机库最多 10,000 台、256 组，目标最多 1,000 台，超限不截断。父组不隐式扩展成员；显式 group 查询才组合集合。

预览/冻结绑定主机与 Profile 双版本；固定快照包含完整 Host 元数据、原始 SSH 认证模板及所需跳板闭包，恢复时不重查当前库。credential_template 保留既有保险库绑定，resolved_connection 仅覆盖目的地址/端口；后续执行须校验实际目的与每跳主机密钥。查询/冻结不创建会话。daemon schema 11 的独立 CAS 存储、IPC minor 23 / HOST_GROUP_CONTROL bit 28 / envelope 45/46 已接入；桌面旧表单不自动重基，冲突或保存结果不明需主动 reload。任务调度、结果持久化、控制台与多主机 SFTP 继续按后续原计划实现。

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
- 进程生命周期测试必须让 PTY 主进程再派生真实孙进程；Unix 测试中的孙进程还必须切换到独立前台 PGID，并以持续心跳验证关闭后已经停止，不能只检查直接 shell 的退出状态。Windows Job Object 同时覆盖 daemon 正常退出与不可恢复崩溃。Linux/macOS 的生产 PTY 在 `setsid` 后先进入同一可执行文件的最小 guardian 模式，再由 guardian 启动真实 shell；该模式必须在 tracing、线程和异步运行时初始化前执行。guardian 记录显式传入的 owner PID，忽略自身 `SIGHUP`、阻塞并轮询 `SIGTERM`，同时检测 `getppid` 变化；真实 shell 在 `exec` 前恢复默认 `SIGHUP` 和空信号掩码。owner 正常关闭、被 `SIGKILL`、shell 自行退出时，guardian 以 `ps` 的 PID/PPID/session 快照收集同一 PTY session 及仍可追踪的后代，逐 PID 发送 `SIGTERM`，宽限 150 ms 后发送 `SIGKILL`。验收必须让真实 shell 忽略 `SIGHUP` 并持续写心跳，再强杀 owner，确认 guardian、shell 和心跳均停止。已完成双重 fork、脱离原 session 且被系统接管的 daemon 不属于 PTY 所有权边界，不能仅凭祖先快照可靠识别，产品不得声称会终止这类显式脱管进程。
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

同步输入应用核心按 [ADR-0019](adr/0019-synchronized-input-control.md) 实施：不可持久化的 Arm/Disarm 控制器、固定标签/会话/作用域快照、256 个目标与 1 MiB 动作上限、超过 10 个目标的 Arm 二次确认、30 秒单次输入确认，以及仅元数据的审计端口。派发前与目标入队时均复核源和目的身份/安全状态；源变化后停止后续入队，已入队结果保留。

Compose 按 [ADR-0020](adr/0020-compose-and-quick-commands.md) 接入单/多行临时编辑、当前终端或已 Arm 固定目标、可见换行/控制字符和发送确认。逻辑 Compose 由各目标按自身括号粘贴模式编码，可选 Enter 放在正文及括号结束后，同一动作入队。当前终端确认不广播到已 Arm 的其他目标；IPC 能力未协商时明确拒绝。切换源、失焦、关闭及 Disarm 清理草稿并撤销尚未开始写出的输入。

桌面适配接入窗口/标签组 SSH 选择和逐项手选、固定目标确认、Arm 警示栏与目标边框、Ctrl+Shift+Esc 退出和显式安全输入开关。所有窗口共享运行控制器；仅为已打开且被选中的隐藏标签建立 Attach 连接，不创建新会话。键盘、IME commit、Paste 发送逻辑 InputAction，键事件保留目标需要的释放/组合信息，编码由 daemon 使用每个目标的模式完成。认证配置界面不进入终端广播；失焦、源身份变化或关闭清理待确认正文。

每条生产输入绑定 SessionId 与唯一连接代次；入队和写 IPC 前复核，切换/重连/安全状态变更使旧项失效。同步队列另持源身份、安全状态及 Arm 许可的弱引用，Disarm、源变化或审计写入故障使尚未开始写出的项失效。已开始的 IPC 写入及远端输入不能撤回。各目标独立使用 256 项有界队列，队列与正在写出的输入合计最多 4 MiB（含正文及固定动作开销），超限立即返回背压；项写完或丢弃后归还额度。拒绝/背压和后续 daemon 拒绝可单独展示；“入队”不代表远端执行成功。

本地审计放在受保护 runtime 子目录，文件逐次附件独立创建，只包含 IDs、时间、输入类型/长度和入队状态。GUI 非阻塞接纳最多 256 条元数据，后台写入 JSONL 并 sync_data；接纳不是落盘确认，崩溃可能丢失尚未写出的记录。写入故障或每附件 64 MiB 上限使审计不可用并阻止后续同步输入。原生 GUI/IME、外部 SSH 和四平台验收仍按 P2-010/020/099 执行，不能用本地 PTY 或 Headless UI 验证替代。

#### B. 批量任务模式

通过非 PTY exec channel 执行，具有确定的目标快照和每主机结果：

```text
Draft → Validating → AwaitingConfirmation → Queued → Running
      → Paused（批次审批）→ Running
      → Cancelling → Completed | PartialFailed | Failed | Cancelled
```

每台主机状态：

```text
Pending → Connecting → Running(step n) → Succeeded
                    └→ Failed | TimedOut | Cancelled | Unknown
```

P2-050 的持久化基础按 [ADR-0022](adr/0022-durable-task-plans-and-execution-intents.md) 实施：不可变任务/主机/Profile 快照、显式 shell 编译、固定分批、审批关卡、逐目标/步骤状态与实际退出码。SQLite schema 12 将初始快照保存一次，后续以快照 SHA-256 和 CAS 版本绑定变化行及仅元数据事件，同一事务提交；重开恢复保留排队目标，进行中目标标记 Unknown。SSH exec 前必须先持久化 StepIntent；发送后无法证明终止的超时/取消/断开不标记 Cancelled。显式重试新建运行并复核原冻结配置，Unknown 需要确认且所有步骤声明幂等。共享全局/目的地/跳板额度、实际调度与超时、SSH 输出落盘、daemon/IPC 和任务界面仍继续按 P2-050 原范围接入。

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

P2-050 按 [ADR-0023](adr/0023-shared-task-budgets-and-durable-dispatch.md) 实现共享全局/运行/目的端点/跳板额度和异步调度器。实际地址/端口用于跨 Host/Profile 别名限流，整条路径原子预留并持有到目标清理结束；同一运行只允许一个内存所有者。每次连接与执行意图先获数据库确认再调用执行器，用户取消信号直接到达 worker，存储结果不明停止后续派发。连接/步骤/排队/总期限、失败阈值、canary 与审批已执行；批间期限写入 TaskRunMeta，恢复保留期限和关卡，进行中目标仍为 Unknown。1,000 目标模型和 SQLite 执行意图回归不代表真实 SSH 连接；非 PTY SSH 流、输出落盘以及 daemon/IPC/UI/CLI 继续接入。

P2-050 按 [ADR-0024](adr/0024-streamed-task-execution-and-control.md) 接入生产非 PTY SSH 执行器、stdout/stderr 独立有界文件及 daemon Task 服务。凭据绑定原冻结 Profile，严格 known_hosts 校验实际目的与跳板；执行前落盘意图，保留 ACK 前输出，通道关闭与输出同步后才返回真实退出码。每步合计最多 1 GiB，分页最多 64 KiB，输出失败停止而不截断报成功。任务创建仅冻结并持久化，启动需显式确认及保存版本；取消绑定不可变运行 ID 并绕过存储等待，响应只确认取消请求。审批绑定关卡及版本，客户端断开不停止 daemon 任务，重启查询保守恢复而不重放。IPC minor 24 / TASK_CONTROL bit 29 / envelope 47/48 已提供创建、状态、列表、开始、取消、审批与输出查询；CLI 仅通过认证 IPC 操作，未知响应不自动重试。桌面结果控制台、完整多主机任务与原生平台验收仍继续按原范围推进。

按 [ADR-0025](adr/0025-desktop-task-review-and-explicit-retry.md) 编写桌面任务创建、冻结计划核对、启动/恢复、取消、关卡批准及目标/步骤结果入口。IPC minor 25 / TASK_PLAN_CONTROL bit 30 增加显式 Review 与 Retry：重试绑定原任务版本和选中目标，创建新 Queued 运行且不启动；Unknown 另须确认并要求所有步骤幂等，拒绝成功及非终态目标。桌面 stdout/stderr 采用最多 64 KiB 的手动字节分页，在后台转义并通过 wgpu LogSurface 绘制，重排任务及结果各有界保留一项。本轮按用户要求仅编写代码，编译/测试/原生 GUI 与验收后续统一完成；P2-060 全文检索、合流、导出及多主机 SFTP 继续保留。

### 8.3 任务步骤

P2-060 按 [ADR-0028](adr/0028-task-results-and-bounded-comparison.md) 编写 `cshell.task-results` v1，捕获冻结主机/步骤元数据、持久版本、真实状态/退出码及派生计数与结果分组，省略命令、变量、环境、认证配置和输出正文。IPC minor 27 / TASK_RESULTS bit 32 增加只读 Results 与终态任务的 Compare；后者每侧最多 64 KiB，只说明采样页相同/差异与覆盖范围，Unknown 仍不确定。桌面可按状态/原因/退出码/步骤/结果组合筛选，比较页由 LogSurface 绘制；GUI/CLI 通过同一格式化器与客户端文件发布助手导出完整元数据快照，不发送保存路径到 daemon，不覆盖既有文件。JSON 文档最多 12 MiB，流式导出最多 64 MiB，CSV 文本前缀与控制符/引号转义已编码。本轮按用户要求暂不编译/测试，全文 hash/diff、合流/搜索、原始归档、多主机 SFTP 与统一平台验收继续保留。

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

当前命令计划使用结构化 Invocation 的 Literal/Variable 参数提供按方言引用；静态 Script 不进行应用插值。Posix、PowerShell、Cmd 和 Custom 方言由用户显式选择，不从平台提示推断。Cmd 动态值采用保守 ASCII 范围并拒绝多行；需要环境覆盖时通过 powershell.exe 设置子进程环境后启动 Cmd，保留独立 stdout/stderr 和真实退出码。命令配置存入普通任务记录，调试与事件不含正文；此阶段尚未接入 upload/download/check 或脚本模板引擎。

混合任务持久核心按 [ADR-0033](adr/0033-durable-mixed-task-core.md) 新增独立 `cshell.mixed-task` v1：每主机顺序运行最多 32 个 Command/Upload/Download 动作，统一冻结 Host/Profile 闭包和步骤配置，并预分配各主机/步骤的固定 child UUID。命令保存真实 TaskRunMeta/TaskTargetRun，文件保留原 UploadTarget/DownloadTarget 的意图、进度、提交与清理回执；文件不伪造退出码。只在当前步骤有持久成功回执时推进游标，失败/超时/Unknown 阻止后续步骤。范围绑定的 leaf repository 把原运行器的变化以父行 CAS 和事件同事务保存到 schema 15，避免独立 child 控制绕过顺序；重启保留已完成前缀且不重放未知步骤。父运行只持有 owner，各 child 使用现有全局/端点/跳板预算，避免嵌套占额。daemon 内部服务接入生产 SSH/文件执行器与确认/取消，当前尚未提供混合 IPC/CLI/桌面入口；分批/canary/check/模板及混合修复、输出/结果分页继续后续编写，统一验证仍待完成。

混合任务控制按 [ADR-0034](adr/0034-mixed-task-control-and-cli.md) 接入 IPC minor 31 / MIXED_TASK_CONTROL bit 35 的 Create/Get/List/Review/Start/Cancel/Output (29～35)。创建仅冻结/持久化并回显计划，普通状态省略正文、路径和凭据模板；目标按 Host ID 分页最多 64 条，计数与完整 Pending Host/revision 列表在同一 SQLite 事务读取。启动 confirmation 文档绑定保存快照 SHA-256 和全部待执行版本，daemon 再复核并明确确认，旧文档不扩大范围、不重放修改。命令输出由父/Host/混合 step 推导 child UUID，禁止直接指定其他 child 或读取文件步骤的输出；最多 64 KiB，可选具体目标版本复核。CLI 提供 mixed-create/review/get/list/confirmation/start/cancel/output，并校验保存定义/双版本、回复分页和输出范围。GUI、混合文件修复与更完整结果/策略继续后续编写，当前只编写代码，统一验证待完成。

桌面混合任务按 [ADR-0035](adr/0035-desktop-mixed-task-control.md) 接入命令/上传/下载步骤增删排序、方言/成功码/路径/期限、后台源摘要、主机选择、保存核对与完整冻结计划审阅。64 条目标分页及当前页筛选保留整体计数和全部 Pending 启动确认，状态与保存快照、host 展示字段和 child ID 逐项绑定；跨窗口旧分页不会覆盖新的运行选择。取消独立排队，不明回复后按权限代次显式载入再修改，不自动重放。命令输出经现有后台字节转义和有界重排在 LogSurface 绘制，绑定父/Host/step/child 及范围；切换运行清除旧输出。工作台输入隔离与确认清理复用，入口按钮支持窄窗换行。当前只编写代码，混合文件修复、更多结果/策略和统一验证继续保留。

混合文件修复按 [ADR-0036](adr/0036-parent-bound-mixed-file-repair.md) 接入父/Host/step/具体父目标版本绑定的只读探测、显式续传、临时清理和认领；只允许当前停止文件步骤修改，原 child ID 与回执不替换，scope 只读模式拒绝写入。修复成功仅关闭该步骤并把后续未开始位置留为 Pending，继续仍须新 Pending 确认，不自动执行下一命令。认领前核对同版本已完成探测与原摘要，并由 leaf 再检查内容；Unknown 发布不重放、cleanup 不删除最终文件。IPC minor 32 / MIXED_FILE_REPAIR bit 36 与独立 repair status 文档避免改变旧 MixedGet 形状，CLI/GUI 接入后台作业与明确确认。schema 15/依赖版本保持，当前只编写代码，统一验证及其他 Phase 2～4 后续继续保留。

P2-060 混合任务结果按 [ADR-0037](adr/0037-mixed-result-captures-and-export.md) 接入全部父目标停止且无活动 worker 时的固定元数据抓取、真实退出码/原因/文件回执统计和组合分组。Unknown 保持不明，后续修复不改写已抓取结果；省略命令/变量/环境、凭据配置、文件路径及输出正文。minor 33 / MIXED_RESULTS bit 37 加 MixedResults/ResultPage 与 result_id tag 19，daemon 最多 8 项/128 MiB 编码数据的临时缓存、10 分钟 token、每页 64 目标/12 MiB、完整结果及导出 64 MiB；客户端检查游标/完整目标/跨页 child ID、派生摘要及完整 SHA-256 后才呈现或导出。桌面按抓取结果状态/组合筛选，GUI/CLI JSON/CSV 新文件导出使用相同流式格式与本机发布助手，筛选不缩减导出，CSV 文本保护且文件步骤无伪造退出码。schema 15/依赖保持，本轮只写代码与格式；持久原始归档、完整输出比较/搜索/合流、更完整策略/模板及统一验证继续保留，不关闭 P2 gate。

### 8.4 取消与结果可信度

2026-10-11 P2剩余功能按 [ADR-0046](adr/0046-aggregate-output-workspace-and-retention.md) / [ADR-0047](adr/0047-explicit-file-publication-policy.md) 完成接续编码：版本绑定的聚合摘要/持久行索引/跨Host字面搜索/UTF-8行对齐diff，多命令双流归档包/客户端整包校验/离线新目录提取，原始输出预览与显式保留操作/持久部分提交回执，带占用锁的临时归档遗留清理；上传/下载及mixed文件步骤增加相同内容复用和独立备份后替换，原不覆盖策略默认保留。minor42，OUTPUT_WORKSPACE bit45/ops57–63、FILE_PUBLICATION bit46，既有schema15与依赖版本保持。当前P2功能代码收口，编译/测试/审计/MSRV/平台/native/实际传输与P2-099仍按要求统一后做，不宣称P2验收完成。

P2-050 按 [ADR-0045](adr/0045-typed-task-editor-and-check-templates.md) 接共享 TaskDefinitionEditor：普通任务、混合命令及快捷 RunTask/RunScript 使用完整结构化 Shell/custom launcher、Script/Invocation、顺序 literal/variable 参数、变量/环境、成功码/幂等/策略表单，JSON 导入导出保留。重复键、缺失引用及无效成功码阻止保存，mixed 单步骤并发1/父策略限制和 RunScript 单Script限制保留。六个内置文件/目录/服务检查及磁盘报告生成草稿，确认替换或同Shell追加，追加值冲突和限额整次拒绝；检查沿用真实命令退出码与成功回执，使用不执行。CLI template-list/template离线生成定义。协议/schema/依赖版本无变化；仅代码与格式化，其他P2范围及统一验证继续。

混合分批调度按 [ADR-0043](adr/0043-durable-mixed-rollout-control.md) 接 optional rollout policy（批大小/canary/关卡/delay/failure limit）与第一冻结Host账本的control carrier，复用既有opaque target CAS/events；legacy None省略字段保bytes/hash，无schema15变化。控制revision与目标revision独立，先持久Start再reload rows；只执行一个batch，canary全成功、真实Failed/Unknown/TimedOut阈值、取消/期限停止派发并保native uncertainty，未派发目标明确关闭。关卡持久nonce、暂停归还owner，批准只变Ready不启动；first-start期限含等待且不因继续/重启重置，delay持久。minor39 / MIXED_ROLLOUT bit43、Get55/Approve56与start control_revision、GUI/CLI表单/关卡/确认接入，读取保守恢复orphan不重放。只代码与格式，check/templates/高级编辑/快捷任务及其他P2代码、统一矩阵后续保留。

混合任务显式新运行重试按 [ADR-0042](adr/0042-explicit-mixed-new-run-retry.md) 接选定停止失败/取消/超时/Unknown目标的 source UUID/hash/Host revision 集合、从step0重放和 fresh parent/target-snapshot/全部child IDs；原定义、目标和凭据闭包完整保留，原回执/临时文件不改写。既有命令执行意图须原定义幂等，Unknown另确认，任何不明/active文件回执先原运行修复；含文件计划另确认从头重放，目的已存在可冲突、下载重新读取。普通snapshot v1省略retry字段保原bytes/hash，重试用v2 provenance记录source row SHA/状态/版本/重放索引和ack。SQLite同事务检查parent/精确source rows后插新Pending rows/events，无schema15变更；minor38 / MIXED_TASK_RETRY bit42、MixedRetry54与GUI跨页选择/确认/CLI helper已编码，新运行创建后仍须核对并单独start，不自动执行或rebase。只写代码与格式，统一验证及批次/canary/关卡/threshold/check/模板和其余Phase2～4继续保留。

P2-060 可移植原始输出归档按 [ADR-0041](adr/0041-portable-raw-output-archive.md) 接停止命令单 Host/step 的 stdout/stderr 与可用 journal 捕获，保存原 native state/child/版本/快照 SHA，不含命令/凭据/路径元数据，payload保留原始字节。CSHOUT01 固定 magic/规范 manifest/顺序 payload/前缀 SHA footer，全文件另有摘要，读取验证长度/entry SHA/footer/EOF，不执行或提取内容。daemon 双遍固定句柄+变更检查、private spool、共享双reader/协作取消和10分钟期限、32 job/4 capture/2 GiB owned cache/10分钟idle TTL已编码；重启不恢复，清理失败或硬退出的private orphan后续另处理。minor37 / TASK_OUTPUT_ARCHIVE bit41与ops50～53、1 MiB有序摘要块、GUI/CLI捕获/状态/取消/本机新文件下载与离线检查接入，先整包校验后sync/不覆盖发布，客户端路径不出IPC。schema15/依赖版本保持；只写代码和格式，统一验证、多主机/多命令bundle、提取UI、归档清理策略及其余Phase2～4继续保留。

P2-060 单命令 stdout/stderr 记录顺序合流按 [ADR-0040](adr/0040-task-output-recording-order-merge.md) 为新 writer 增 `events.bin` v1：固定 72 字节 header/event/footer、原 child/Host/step、递增 sequence、本机记录时间、流偏移/长度/内容 SHA-256，最多 65,536 事件；超出日志容量仍继续原始输出并封口标明截断。原始双流 sync 后 footer 留长度/前缀摘要，未封口和部分尾部保留覆盖事实；不补造旧输出时间，不把本机 seal 视为远端成功。读取固定句柄检查完整有界 index 与选中原流字节摘要，64 事件/64 KiB 合计 body，next 绑定 journal SHA，显式覆盖/截断/尾部信息。minor 36 / TASK_OUTPUT_MERGE bit 40、OutputMerge 49 与 GUI/CLI/LogSurface 分页和原流跳转已编码，复用停止 scope/child 与共享双 reader；schema 15/依赖版本保持，仅 CLI 增现有 serde edge。只写代码与格式，统一验证、可移植原始归档、跨主机合流/聚合索引、文本 diff 和其他 Phase 2～4 继续保留。

P2-060 已保存输出检索按 [ADR-0039](adr/0039-stored-output-literal-search.md) 接普通/混合停止命令单流的 UTF-8/hex 字面字节查询，复用原版本/Host/step/child 绑定和固定句柄校验。最多 1,024 字节 pattern、256 偏移/批，跨 64 KiB 缓冲/重叠匹配，额外匹配决定续查，cursor=末条起点+1；每批对整个本地已保存流计算 SHA-256，续查绑定上一摘要，变化失败，无部分成功结果。与完整分析共用 2 worker/1 GiB/协作取消及 5 分钟期限。minor 35 / TASK_OUTPUT_SEARCH bit 39 与 ops 46～48、32 项临时 job/结束后 10 分钟/64 KiB 文档已编码；回复只留 pattern 长度/hash、scope/binding/offsets，不含查询正文或输出正文，query/cancel 快路径。GUI/CLI 支持明确启动/查询/取消/续查和既有 LogSurface 字节页跳转，输入或范围变化禁用旧结果应用。当前为每批重新读取整个文件的扫描，持久索引、跨目标聚合搜索、文本 diff、时间合流/原始归档与其他后续范围继续保留；schema 15/依赖版本保持，仅代码和格式、后续统一验证。

P2-060 完整已保存输出分析按 [ADR-0038](adr/0038-full-stored-output-analysis.md) 接入后台 SHA-256 与逐字节比较，绑定普通 run revision 或混合两侧 parent-target revisions，server 推导正确冻结 child UUID/step。仅停止命令回执可读，Unknown 不变，不执行 SSH 或重放。固定只读句柄、私有目录/非链接文件检查与前后长度/时间/Unix identity 检查，两个 worker、每侧 64 KiB 缓冲、每流 1 GiB、协作取消/5 分钟期限、进度及缺失/变化/存储错误已编码；完整指本地已保存流，不证明全部远端字节到达。minor 34 / TASK_OUTPUT_ANALYSIS bit 38 与 ops 43～45 提供启动/查询/取消，32 项临时 job/结束后 10 分钟保留、64 KiB 文档，无输出正文/路径；query/cancel 不等待任务存储操作。GUI 普通/混合共用后台连接和明确作业范围，显示完整摘要/不同字节数/首差异并跳转既有字节页；CLI typed request/job JSON 入口接入。只写代码与格式，schema 15/依赖版本保持；文本对齐 diff、搜索/合流/原始归档、更完整策略及统一验证继续保留。

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

2026-10-12 P3-010 按 [ADR-0048](adr/0048-sftp-dual-pane-and-durable-session-queue.md)～[ADR-0051](adr/0051-complete-sftp-workspace.md) 完成上述功能编码：双栏正向分页与有界访问目录树、普通文件拖放、完整 SHA-256 校验及显式断点恢复、私有持久队列与 1～4 并发/逐文件限速、不覆盖/相同内容复用/独立备份后替换、文件操作预览确认、NoFollow/显式 Follow、同 peer 新会话显式迁移，以及受控工作副本/内联或外部编辑器/变化检测和版本绑定回传。目录删除仅空目录，拖放传输仅普通文件；不递归遍历或同步目录。远程编辑回传要求原目标摘要和权限仍匹配，先保留独立备份，其他 Pending 保持暂停。协议 minor46 / SFTP_WORKSPACE bit50。代码完成与验收分开，资源/路径/服务器与平台边界及后续验证见 [Phase 3 STATUS](phase3/STATUS.md)；早期 P1-070 基础语义保留，旧直接端口不隐式升级。

P1-070 基础版先接入已验证 SSH 会话的 SFTP 子通道：目录最多返回 512 项并显式标记截断，单文件上传/下载按 32 KiB 分块，daemon 持有传输任务与取消状态，GUI 重连可按会话找回最近任务。只允许本地绝对路径；已有目标在提交前检查并拒绝，远端 rename 的并发覆盖语义由服务端决定。此阶段只校验上传后的大小，不承诺哈希验证、断点续传或大目录分页；见 [ADR-0006](adr/0006-phase1-basic-sftp-control.md)。

### 9.2 安全文件分发

P2-070 按 [ADR-0029](adr/0029-durable-multi-host-upload-core.md) 首先编写独立上传运行类型，固定源大小/SHA-256 与 Host/Profile 快照，使用逐目标 CAS、持久意图和真实文件提交回执，不伪造 SSH exit code。schema 13 将快照、目标状态与事件持久化；daemon/CLI 通过 IPC / TASK_UPLOADS bit 33 创建、查询、启动、取消、检查、确认前缀恢复/清理/采纳。32 KiB 流式传输后完整读回校验，首版安全发布仅采用服务端明确支持的 fsync/hardlink，无覆盖创建目标后清理临时名，不使用不确定覆盖的 rename fallback；源修改导致最终发布拒绝，但临时文件可能已有流入字节。Unknown 不自动重放，目的内容匹配仅供检查，采纳必须明确确认。下载、混合 TaskRun 步骤、备份/替换与实际文件系统/服务器/平台验收继续实现，当前只写代码，未运行统一验证。

桌面按 [ADR-0030](adr/0030-desktop-batch-upload-control.md) 接入后台摘要与取消、源/目的/策略表单、查询/组/主机面板选择、冻结计划核对、显式启动与逐目标虚拟化状态/筛选和恢复确认。IPC minor 29 增加 UploadList (19)，独立摘要分页最多 64 个运行。Probe/Cleanup/Adopt 由 daemon 后台作业执行并通过 UploadGet 返回绑定目标/版本的状态与探测内容；控制连接断开不丢弃作业，取消绕过客户端普通队列。丢失修改回复需显式载入状态，任何写入/恢复均不自动重放；认领前的内容一致只作检查结果，仍要求单独接受归属不明。所有本轮代码继续等待后续统一验证。

多主机下载按 [ADR-0031](adr/0031-durable-multi-host-download-core.md) 增加独立 Download 运行与 schema 14 CAS/事件。每目标先通过持有的只读 SFTP 句柄读取并保存自己的大小/SHA-256，再以 32 KiB 分块写入 daemon 侧既有目录中的专属临时文件；同步检查点、本地完整读回及远端再次摘要核对后，以不覆盖 hard link 发布为 `<Host ID>-<local_name>`。共享命令/上传连接预算，明确区分内容固定、字节进度、发布意图、提交、认领和临时清理。Unknown 不自动重放，续传要求原源摘要、完整前缀核对和具体目标版本；本地探测、清理、认领由后台恢复作业处理。IPC minor 30 / TASK_DOWNLOADS bit 34 与 CLI download-definition/create/get/list/start/cancel/probe/resume/cleanup/adopt 接入；状态不发送认证模板或文件正文。桌面下载、混合步骤、备份/替换和文件系统/服务器/迁移矩阵继续后续推进，当前只编写代码。

桌面下载按 [ADR-0032](adr/0032-desktop-batch-download-control.md) 接入独立后台控制连接、路径/策略编辑、查询/组/主机面板选择、逐主机准确目的路径预览与冻结核对、64 条分页、目标虚拟化和内容/阶段筛选、取消与版本绑定的只读探测/续传/清理/认领。保存复核定义、双版本和目标展示元数据，不启动传输；源内容由 daemon 逐主机确定。操作条件在 application 中复用，认领前核对当前版本的已完成探测并另行接受归属不明。上传/下载共用认证控制传输，取消独立排队；不明回复后只有捕获相同本地权限代次的显式 reload 能恢复修改，旧查询、自动轮询和 create 回复不会覆盖后来的不明结果。切换面板清除确认并复用工作台输入隔离。当前仍只编写代码，统一验证与混合步骤等后续范围继续保留。

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
- [ADR-0009](adr/0009-credential-diagnostic-boundaries.md) 将 host key 确认令牌纳入临时凭据清零，并规定 SSH 错误诊断及其 source 链仅暴露失败类别；终端事件 Debug 隐藏输出与服务端文本，产品中的终端内容和公开指纹仍通过显式接口展示。
- [ADR-0010](adr/0010-sftp-diagnostics-and-cleanup-state.md) 扩展 SFTP 诊断边界；临时文件位置及已提交状态通过受限的传输恢复字段供文件面板展示，清理失败不能被当作已取消，提交后清理失败不能重放传输。
- [ADR-0011](adr/0011-portable-master-password-vault.md) 冻结主密码 Vault 后端的加密格式、凭据绑定、锁定清零和文件替换语义。[ADR-0012](adr/0012-vault-controls-and-clipboard-identities.md) 接入主密码/系统钥匙串根密钥模式、私钥验证导入、凭据 UI、15 分钟租期及失焦锁定；锁定代次阻止延迟解锁与在途 SSH 注册，已有连接保持运行。
- P1-080 按 [ADR-0026](adr/0026-native-windows-session-vault-locking.md) 编写 daemon 的 Windows 原生会话监测：不可见顶层窗口接收本进程会话的 WTS 锁屏/注销/断开和已注册的睡眠/恢复通知，启动及恢复通过 WTSInfoEx 复核；每秒对账，3 秒无有效观测则限制凭据使用。受限状态推进锁定代次、撤销新认证，密码派生持有的状态锁释放后清除密钥；凭据包恢复等待状态锁时不占用最终发布门槛，避免阻塞系统限制通知。系统恢复不自动解锁 Vault。IPC minor 26 / VAULT_SESSION_STATE bit 31 / VaultResponse tag 14 提供只读系统会话状态，桌面锁定代次变化清空敏感输入和凭据包预览。Linux/macOS 原生适配及跨平台/原生锁屏、恢复与 GUI 验收后续完成；本轮只编写代码，旧质量记录不覆盖本轮修改。
- OSC 52 远端读取剪贴板始终禁止；远端写入须经过按会话/主机的授权策略与 UI 确认。在授权链路完成前显式关闭 OSC 52，不能依赖终端库的默认设置。
- P1 会话授权遵循 [ADR-0007](adr/0007-session-clipboard-consent.md)：只接受已验证且运行中的 SSH 会话；每次写入单独确认，可拒绝或禁用本会话。请求最多 16 KiB、30 秒过期，不持久化授权；新 Shell 重新确认，远端读取始终无响应。
- [ADR-0008](adr/0008-host-clipboard-block-policy.md) 提供持久主机禁用列表，按启动时捕获的主机和端口生效，保存后立即清除匹配会话的待确认请求；移除禁用恢复逐次询问，不重放历史请求。主机策略通过独立 revision 保存，并提供安全设置入口。
- P1-080 按 [ADR-0027](adr/0027-unix-desktop-session-vault-monitoring.md) 编写 Linux/macOS 监测适配。Linux 使用固定系统 bus、固定 login1 唯一 owner 与当前 UID 会话身份，订阅锁定/安全属性/睡眠/关闭信号并作有界新鲜读取，服务重连不切换到其他会话；`LockedHint` 依赖桌面正确上报。macOS 经公开 IOKit/CF API 读取本用户控制台字典与变更 seed，类型或结构不符即限制访问；所用控制台字段来自 Apple 公开源码中的私有 key header，兼容性需要原生验证。两者以包含睡眠的系统时钟检测观测间断，复用锁定代次、延迟密钥清理与明确重新解锁，现有连接不由监测器终止。原生兼容性、锁定/快速切换/睡眠、GTK/Qt/IME 与当前四平台门禁待统一验证，本轮不关闭 P1 验收。
- ADR-0012 的策略格式 v2 支持显式持久自动授权，绑定实际验证指纹、账号与路线（含跳板指纹）；只允许从正在运行的已验证会话新增授权，窗口失焦或切换来源后不写剪贴板。撤销使旧 token/分片失效，自动批准结果未知时不重试。
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

第一批结构化 SendText/OpenCompose 动作已接入桌面快捷命令库：文件夹、标签和系统筛选；所有发送先预览。配置保存到 daemon 的 SQLite，通过独立版本 CAS 控制并发编辑，冲突或保存结果不明后停止写入，主动重新加载后才能继续。正文保存前须确认普通配置持久化，临时 Compose 草稿不自动保存。库上限为 256 个命令、64 个文件夹、单正文 256 KiB、完整 JSON 1 MiB。

P2-030 按 [ADR-0044](adr/0044-quick-command-task-templates.md) 加 RunTask/RunScript：保存完整 TaskDefinition 和 HostSelection，RunScript 限单步远端 Script，复用任务定义/变量引用/Shell编译和选择验证；模板不含实时会话或执行许可。快捷库首次添加任务动作升为 v2，删除后保留 v2，旧文本库保 v1；minor40 / QUICK_TASK_TEMPLATES bit44，daemon 对未协商客户端拒绝 v2 读取/保存和 v2→v1 覆盖，复用原 schema10/CAS。桌面按 [ADR-0045](adr/0045-typed-task-editor-and-check-templates.md) 提供共享结构化表单与 JSON 导入导出、保存确认和当前窗口草稿入口，TaskPanel 保留 Invocation/变量/环境/custom Shell/策略，使用后另保存冻结计划并核对确认启动，不隐式执行。仅编码与格式化；基础高级表单与六个检查/报告模板已接续编码，参数权限、QuickJS、其他 P2 范围及统一验证后续保留。

### 12.2 触发器

触发器在解码后的输出流上进行跨数据包匹配，支持普通文本和受限正则。动作包括高亮、通知、铃声、追加文本、启动脚本和记录事件。必须有防循环、冷却时间、每分钟上限和作用域，否则“匹配输出后发送文本”容易形成无限反馈。

### 12.3 脚本

2026-10-14 按 [ADR-0074](adr/0074-script-live-grants-and-durable-worker-supervision.md) 接单次实时权限预览/授权/撤销、具体会话实例/认证和目录版本、秘密参数目标发送/字面等待/Profile摘要/冻结任务一次启动，触发事件核对及下一次匹配单次授权；桌面/CLI明确确认并核对回执。独立所有者阶段时限、跨进程双槽/出生标记恢复、macOS采样RSS和注册/bootstrap权利收紧，退出/清理/远端效果分别记录，不明不重放。worker/报告v6、daemon minor55/SCRIPT_CONTROL bit59/envelope63～64，无schema或包版本升级；所列编码入口已接通，未编译/测试或实际执行，平台隔离/恢复与完整验收统一后续验证。

2026-10-14 按 [ADR-0073](adr/0073-macos-script-worker-seatbelt-policy.md) 接macOS线程前固定默认拒绝Seatbelt及继承FD清理/核对，安装与Ready分别核对系统状态，原生诊断/拒绝日志静默；无外部profile或API-only降级。worker/报告及各平台启动参数v5、GUI策略回执同步，无新依赖/schema/daemon minor变化。未安装或运行验证；macOS进程内存/继承Mach权利、跨进程恢复、准备/清理时限、live授权与统一门禁继续。

2026-10-14 按 [ADR-0072](adr/0072-windows-script-worker-lpac-launch.md) 接Windows每次独立LPAC/零能力及父子SID/低完整性核对，私有阶段exe只读执行、句柄白名单和原子Job；worker/报告v4区分退出与隔离资源清理。GUI/CLI清理未确认停新运行，无普通权限回退；仅已锁Windows功能边，minor54/schema保持。未实际创建容器、运行或验证，macOS访问隔离、跨进程恢复、准备/清理时限、live授权及统一门禁继续。

2026-10-14 按 [ADR-0071](adr/0071-linux-script-worker-syscall-policy.md) 接Linux原生64位小端x86_64/arm64的seccomp-BPF全线程白名单、描述符清理和默认拒绝，worker/报告v3分别确认资源与访问策略；Windows/macOS明确仅既有API/资源限制。无新依赖或schema/daemon IPC变化；未编译/测试/实际安装过滤器，其他平台访问隔离、live授权及统一验证继续。

2026-10-14 按 [ADR-0070](adr/0070-script-worker-resource-containment-and-liveness.md) 接Windows Job、Linux/macOS资源上限、父进程失联独立退出和资源Ready握手，worker私有协议/报告升v2；只增已锁平台依赖边，daemon minor54/schema保持。资源限制与完整文件/网络访问沙箱分别记录，后者及live授权/目标API仍待实现；没有实际执行或验证。

2026-10-14 按 [ADR-0069](adr/0069-isolated-quickjs-offline-worker.md) 接独立QuickJS离线worker与父进程逐次参数/能力/预算检查、取消/退出证明和Unknown配额保留；GUI/CLI显式确认、临时参数、后台状态和取消入口已编码。仅参数/暂停/通知计数，实时终端/Profile/任务仍拒绝；引擎内存/栈与活动墙钟限制不是OS沙箱/CPU/RSS限制。新增锁定rquickjs0.14.0依赖，无schema/IPC变化；未编译或执行JS，OS级隔离、live grant/revoke/目标API与统一验证继续。

2026-10-14 按 [ADR-0068](adr/0068-script-drafts-permission-plans-and-redacted-recording.md) 先接封闭脚本/录制v1、资源/参数声明、整份文档摘要和默认无选择的离线权限计划，GUI/CLI可编辑、脱敏录制和生成参数化JS草稿；终端输入仅保存合并省略标记，未处理缺口拒绝生成。计划不构成授权，不执行JS；QuickJS独立worker、宿主broker和实际grant/revoke/目标API后续接续。无依赖/schema/协议升级，仅代码与格式化，统一验证仍待完成。

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

2026-10-14 按 [ADR-0075](adr/0075-explicit-directory-and-manifest-migration.md) 接显式SecureCRT Sessions/Xshell会话目录与四类来源混合清单，保留逻辑层级/前缀，限制枚举/原字节/源文件/最终目录规模，原生无跟随读取、合作取消和GUI双槽后台所有者；摘要绑定叶来源、Profile/文件夹/前缀UUID和跳过/省略计数。GUI/CLI复用已冻结文档SHA、目录版本、纯计划和一次确认提交，不重新读取来源。新增内部cshell-import和已锁依赖边，minor55/schema保持；首批按格式形状定义兼容矩阵，XML/XTS/加密包仍不支持。仅源码/格式化/离线依赖图，P3-050统一验证与验收待完成。

2026-10-14 按 [ADR-0067](adr/0067-rsa-ppk-and-ring-signing.md) 接RSA PPK格式适配与ring私钥签名：有界公共参数/组件/MAC/填充、固定精度CRT与随机SHA-2双配对、普通公钥/证书一次性身份绑定及后台预算；Vault/凭据包/CLI/GUI沿用既有入口，SHA-1拒绝和信任/代次约束保持。新增已锁依赖边与lock列表，无版本/schema/协议升级；仅源码和格式化，更广迁移范围与统一验证继续。

2026-10-14 按 [ADR-0066](adr/0066-mobaxterm-bookmark-metadata-import.md) 接 MobaXterm Bookmarks 导出元数据与逻辑文件夹层级，Unicode/明确Windows-1252、有界字段组/调色板宽度与行为门；GUI/CLI复用v2、摘要/文档SHA/版本/纯计划/一次确认和完整回执。凭据/密钥/信任/TERM等省略，无厂商进程、关联文件或连接；无依赖/协议/schema升级，仅源码与格式化，RSA PPK、更广范围与统一验证继续。

2026-10-14 按 [ADR-0065](adr/0065-xshell-session-metadata-import.md) 接 Xshell 单个明文 XSH 的独立有界 INI 元数据与 GUI/CLI，复用 v2、摘要/文档 SHA、版本、纯计划/一次核对提交和完整目录回执。只转文件名/显式 SSH 目标/账号，继承/代理/执行等保守拒绝；密码/密钥/信任/TERM等省略并提示，无关联文件扫描、厂商进程或连接。无依赖/协议/schema升级，仅源码与格式化；其他格式和统一验证继续。

2026-10-13 按 [ADR-0064](adr/0064-bounded-ppk-key-admission.md) 接独立有界 PPK 密钥工作流：Ed25519/ECDSA v2/v3 结构/KDF资源门、已锁库 MAC/解密与本地随机配对、Vault/密码包/统一 SSH 路径校验与双槽阻塞预算、离线 CLI 公钥信息检查。不随元数据文件自动读取私钥或口令，不导出明文密钥/改主机信任；仅既有依赖边变更，无版本/协议/schema升级。只写源码与格式化，实际验证及其他格式继续。

2026-10-13 按 [ADR-0063](adr/0063-securecrt-session-ini-metadata-import.md) 接 SecureCRT 单会话 INI 的有界独立元数据读取与 GUI/CLI；只转文件名/SSH2 主机/端口/账号，危险行为字段族拒绝，凭据/信任/仿真等省略。共用封闭来源摘要、文档 SHA、纯计划/版本/完整回执，旧 PuTTY预览保持兼容，不扫描目录或运行厂商程序。无 schema/协议/依赖升级，只有源码与格式化，其他格式和统一验证继续。

2026-10-13 P3-050 按 [ADR-0062](adr/0062-putty-registry-metadata-import.md) 接 PuTTY Sessions `.reg` 元数据子集的独立 Rust 解析和 GUI/CLI 转换预览。只转 SSH 名称/目标/端口/账号/TERM，源凭据/密钥/信任与其他设置不导入；活动代理/命令/转发等拒绝，遗漏须核对，文档 SHA/版本和完整目录回执绑定。复用 v2 导入事务及现有协议/schema，无依赖升级。源码与格式化完成，其他格式/PPK 和统一验证继续。

按优先级支持：

1. `~/.ssh/config`、known_hosts、OpenSSH key。
2. Xshell session/export 文件。
3. PuTTY registry/PPK。
4. SecureCRT、MobaXterm 的可公开解析配置。

导入流程为“解析 → 预览 → 冲突处理 → 提交”，失败不能产生半条会话。加密密码若无法合法解密，则只导入元数据并要求用户重新绑定凭据。解析器使用公开样本和用户提供文件做 clean-room 实现，不复制商业产品代码。

P1-010 首先支持无凭据的 CShell Profile 元数据 JSON：顶层固定为 `format: "cshell.profile-catalog"`、`version: 1`、`folders` 与 `profiles` 数组。每项使用 UUID、名称、父文件夹/所属文件夹、可继承的终端设置；Profile 另含 `kind`（`ssh`/`local`）、标签和收藏状态。单文件上限 1 MiB、最多 2048 项，导入文件内部须自洽且不得包含 secret。预览按当前 catalog revision 展示创建、跳过、替换或冲突；默认遇冲突停止，用户可选跳过或替换。提交重新校验同一份预览文件与 revision，并在一个 SQLite 事务内应用整批变更。OpenSSH 基础子集已按下述 P1-090 契约接入，商业客户端格式留待后续阶段。

P1-090 按 [ADR-0013](adr/0013-openssh-profile-import.md) 增加 v2 `ssh_connections` 与有界 OpenSSH 配置导入：显式 Host、通配默认值、首个标量值、账号/端口、密钥引用和已定义别名的单跳。Include/Match/命令及其他无法转换的选项拒绝整批导入；不执行命令、不读取密钥、不改变 host-key 信任。预览展示连接参数，缓存转换结果后按 revision 原子提交，跳过/替换时重映射跳板 ID。

Profile 导出与 LocalConnection v3 按 [ADR-0014](adr/0014-profile-export-and-local-transfer.md) 接入：本地程序、cwd 和关闭策略可导入；省略参数或环境字段时替换保留旧值，新连接为空，显式空数组或映射清除，预览只显示数量。商业客户端解析仍待后续任务。

Theme 公开载荷按 [ADR-0015](adr/0015-terminal-theme-transfer.md) 接入：独立 `cshell.terminal-themes` v1 JSON，最多 256 KiB、128 个自定义主题；小写 ASCII 名称引用，默认前景/背景、光标、选择区域与 ANSI 16 色均为三通道整数 sRGB。未知字段、尺寸/范围错误和内置名称覆盖整批拒绝。桌面预览颜色样本并处理 Stop/Skip/Replace，缓存原文件与策略后绑定 catalog revision 原子提交；Skip 显示保留的颜色。schema v9 升级前备份，主题与 Profile 共用事务/revision。导入后可在 Profile/文件夹选择器引用，已有标签与冷日志随主题更新；256 色其余索引及显式 True Color 保持原语义。

### 13.2 导出

- Profile 公开 JSON v3 导出已接入桌面与 daemon，绑定已保存 catalog revision；默认省略可能含敏感值的本地参数和环境变量，显式勾选后以明文包含。Vault 密码、口令、私钥内容、host-key 信任和凭据存储策略不导出；SSH 路径与 Theme 名称仅为引用。
- 导出保存继承的终端默认值，不修改接收方全局默认设置。写入绝对新路径，通过同文件系统临时文件、sync 和不覆盖发布保存；失败或不确定结果不自动重试。Windows 断电持久性及所有文件系统上的原子发布不宣称保证。详见 [ADR-0014](adr/0014-profile-export-and-local-transfer.md)。
- 桌面导出内容可选 Theme palettes，保存独立主题文档并复用不覆盖发布；Profile JSON v3 只含名称引用，迁移自定义配色需另行导入 Theme 文件。[paper 示例](examples/terminal-theme-paper.json)可直接预览/导入，内置 Dark/Light 无需复制。当前公开格式是终端配色，不包含应用皮肤或可执行内容。
- OpenSSH config 兼容子集已按 [ADR-0017](adr/0017-standalone-openssh-export.md) 接入：明确选择 SSH Profile，自动包含单跳依赖，使用 UUID 稳定别名和独立 `ssh -F` 文件。密码、私钥、证书和 OpenSSH agent 保持各自认证方式；代理、Pageant、Windows Auto 和指纹筛选 agent 无法转换时整批拒绝。最多 512 个目标/1 MiB，绑定保存 revision 并核对完整返回内容，同步后不覆盖发布；不导出信任和 secret，绝对密钥路径需接收方另行提供。
- 含 secret 的导出已按 [ADR-0016](adr/0016-encrypted-credential-packages.md) 接入独立密码包 `CSHCPK01` v1：Argon2id / AES-256-GCM，新的随机盐/nonce，目标和整组凭据一起认证加密。用户明确选择最多 32 个 SSH Profile，已入库私钥内容另行勾选；外部私钥文件、agent、根密钥和信任不导出。先导入公开 Profile JSON，再向已解锁的加密 Vault 恢复；核对 ID/完整文件夹名称映射、认证及跳板图，按整个 Vault 凭据组处理 Stop/Skip/Replace，确认后一次写入。两分钟单次预览只缓存密文，确认需再次输入包密码，并检查目录/Vault revision 与锁定代次；失败不自动重试。桌面用同步、不覆盖的新文件发布，并在关闭/失焦时清空密码和预览。
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
sftp.list / sftp.upload / sftp.download / sftp.status / sftp.cancel (P1-070)
sftp.enqueue / sftp.pause / sftp.resume (later phases)
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
│  ├─ DEVELOPMENT.md           # 本地实施计划，不纳入 Git
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
- Windows/macOS/Linux 系统钥匙串的最小写入、读取、删除和失败恢复原型；完整 Vault 留到 Phase 1。四目标原生 CI 与可自动化的窗口/平台测试属于工程门槛，真实读屏、IME 候选窗和视觉主观体验在使用中持续优化，不作为 Phase 0 的人工阻断项；见 [ADR-0005](adr/0005-phase0-engineering-gate.md)。

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

2026-10-13 P3-040 按 [ADR-0058](adr/0058-profile-batch-edit-and-inheritance-inspection.md) 接批量 Profile 元数据/终端覆盖与既有三层继承来源检查：逐项预览、一次性令牌/双摘要/版本/效果绑定、全目录校验后 Profile 事务/CAS 原子提交，GUI/CLI 和不明停写/输入隔离已接。minor52 / PROFILE_BATCH bit56；无 schema/版本升级，仅源码与格式化。

2026-10-13 按 [ADR-0059](adr/0059-terminal-context-policy-inheritance.md) 接标签/Host/临时标量解析和独立字段来源、私有有界策略目录与三版本预览确认、GUI/CLI 和桌面显示主题。临时绑定 Tab/Session/Profile/三版本，不持久恢复或改变实际连接目标；minor53 / TERMINAL_POLICIES bit57。

2026-10-13 按 [ADR-0060](adr/0060-reviewed-terminal-context-launch.md) 接启动前完整报告/三版本核对与冻结计划、SSH/本机 TERM、Logging=false 无日志落盘及消费者明确拒绝、GUI/CLI 独立新会话确认与回执、不明不重放；minor54 / TERMINAL_CONTEXT_LAUNCH bit58。生产握手补齐已接能力；无 schema/依赖/日志格式升级。

2026-10-13 按 [ADR-0061](adr/0061-reviewed-workspace-recovery-and-pane-separation.md) 接高级布局恢复与分屏组整组分离：完整预览/绑定分类、所有窗口的自动保存暂停/明确读取屏障、SQLite CAS/完整回执后本地采用、不明不重放、失败窗口重试与 GUI/CLI 布局导入导出。复用 workspace v1/现有 schema 和协议，退出视图不终止进程，同步输入解除；无依赖升级。P3-040 已列功能代码接入，仅源码和 cargo fmt，统一验证与验收仍待完成。

2026-10-13 P3-030 按 [ADR-0057](adr/0057-terminal-themes-highlights-triggers-and-replay.md) 接主题编辑/JSON/TOML/XCS 子集、HighlightSet/有界后台纯覆盖层、显式实时触发器与防反馈/代次/事件环、原始 Hex/独立只读 ANSI 回放及时间尺寸旁路/旧日志降级。GUI/CLI 及输入/效果绑定已接，minor51 / TERMINAL_TOOLS bit55；保留 v1 16 色主题，Script 请求后续 P3-060 执行授权，视觉铃声/通知/预览边界见 ADR。无版本/schema 升级，仅源码与格式化，统一验证及 P3-040～060 后续编码继续。

2026-10-13 P3-020 按 [ADR-0056](adr/0056-ssh-provider-capabilities-and-connection-boundary.md) 接 Provider 能力/需求匹配和后端中立连接边界：仅注册 russh，明确平台 agent、未实现能力/算法/后端与跳数限制，完整路由在凭据/网络前检查，同选择贯穿 connect/scan/任务/隧道。GUI/CLI 只读范围绑定查询，minor50 / SSH_PROVIDER_CONTROL bit54；仅内部 workspace 依赖边，无版本/schema 升级。P3-020 已列功能代码完成，编译/测试/平台/真实服务器验收仍后续统一进行。

2026-10-12 P3-020 按 [ADR-0055](adr/0055-managed-openssh-host-authorities.md) 接 host CA 管理编码：持久公钥/范围/信任与吊销、版本/指纹/一次性预览确认，叠加现有 known_hosts/证书/pin，协调在途认证；只读证书信息与指定主机检查、GUI/CLI 输入/范围绑定已接。minor49 / HOST_CA_CONTROL bit53，不保存 CA 私钥或执行签发/远端管理；无 schema/依赖升级，仅代码与格式化，Provider 后续及统一验证继续。

2026-10-12 P3-020 按 [ADR-0054](adr/0054-dedicated-tunnels-and-bounded-recovery.md) 补独立 SSH 隧道和有界恢复：先双版本冻结完整路由与 verified peer，另行启动无 PTY 路径；目标/逐跳 key pin 在认证前叠加原信任验证，同认证代次、资源/期限约束，旧路径关闭证明后按显式策略最多 5 次恢复，重启/orphan 不自动重跑。minor48 / DEDICATED_TUNNELS bit52，GUI/CLI 全部操作接入；无 schema/依赖升级，仅代码和格式化，CA/Provider 后续编码与统一验证继续。

2026-10-12 P3-020 按 [ADR-0053](adr/0053-session-tunnel-management.md) 接 local/remote/dynamic 会话隧道管理编码：私有定义与版本 CAS、native 意图/ACK/端口、全局/会话/连接上限、独立后台生命周期、原已验证 peer 显式恢复与不明授权不重放，GUI/CLI 全部入口。minor47 / TUNNEL_CONTROL bit51，重连/读取不自动运行；独立隧道会话及自动恢复策略仍待编码，CA/Provider 扩展继续。仅代码与格式化，真实转发和平台/故障矩阵后续统一验证。

2026-10-12 P3-020 按 [ADR-0052](adr/0052-bounded-multi-hop-ssh-routes.md) 接续多级跳板编码：最多 8 个跳板、无环路、逐跳认证/主机密钥、反向清理，交互/命令/上传/下载使用完整保存或冻结链；多跳 clipboard/SFTP 信任捕获整条已验证路径。GUI 连接顺序和 OpenSSH linked alias 导入导出已接，无 schema/协议变更，仅代码与格式化；隧道/CA 管理/Provider 扩展和统一验证继续。

2026-10-12 按 [ADR-0051](adr/0051-complete-sftp-workspace.md) 完成 P3-010 已列功能代码：新会话同 peer 显式迁移、远程编辑与原摘要/权限绑定回传、文件管理预览确认、链接/冲突策略、并发/速率、目录树/普通文件拖放、历史与保留。minor46 / SFTP_WORKSPACE bit50 / ops22～42；覆盖与明确边界见 [Phase 3 STATUS](phase3/STATUS.md)。仍仅编码与格式化，统一验证和其他 P3 任务待完成；以下是此前切片历史，其 P3-010 待编码项由 ADR-0051 接续完成。

2026-10-12 按 [ADR-0050](adr/0050-verified-session-transfers-and-explicit-recovery.md) 接 P3-010 会话校验传输与显式恢复：源完整SHA/暂存完整SHA、durable intent/ack/prefix offset、fsync后保存、no-replace发布、提交与清理独立回执；原live SessionId单条续传/匹配目标认领/owned暂存检查后清理，其他Pending暂停，未知Create/Publish不重放，旧模式不降级。GUI/CLI全部入口，minor45 / SFTP_VERIFIED_QUEUE bit49，仅代码/cargo fmt；重启后新Session显式迁移/远程编辑/冲突和文件管理及统一验证继续。

2026-10-12 按 [ADR-0049](adr/0049-sftp-directory-cursors.md) 补 P3-010 完整正向目录分页：远端保留目录句柄及NAME尾部，本机保留ReadDir/目录身份核对，512项页/16全局cursor预占/每session2/120秒过期/30秒请求deadline，session+cursor+revision单次续读与资源回收；GUI/CLI下一页/显式关闭和descriptor复核。minor44 / SFTP_DIRECTORY_CURSOR bit48，无schema变化，仅cargo fmt；会话verified断点/远程编辑/冲突和远端文件管理及统一验证继续。

2026-10-12 按 [ADR-0048](adr/0048-sftp-dual-pane-and-durable-session-queue.md) 开始 P3-010 双栏浏览与持久会话队列接续编码：daemon本机/远端目录、私有checksum队列/版本CAS、明确保存/启动/暂停/仅取消Pending/元数据移除、派发前intent和native id、实际receipt、Unknown暂停与不重放、重连查询、GUI/CLI已接入。minor43 / SFTP_BROWSER_QUEUE bit47，无schema变化，仅cargo fmt；原会话传输端口语义保留，完整分页/verified断点/remote edit/冲突及其他P3～4、统一验证继续。

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
- P1-099 的原生会话功能门禁按 [ADR-0018](adr/0018-native-interactive-session-gate.md) 接入 `cargo xtask interactive-sessions`：20 个独立 PTY、认证 IPC、200 次输入/尺寸确认、全部订阅断开重连、KeepAlive 关闭与进程终止后的日志复核；四平台 `native-ci-bench` 自动包含此门禁。另有 `cargo xtask workbench-sessions`，通过真实 PTY/IPC 检查桌面 20 标签的复制文本、异步搜索、状态保留与重连，以及四分屏的会话绑定、布局和焦点输入；原生平台的全工作区测试包含该集成门禁。工作台逻辑门禁不创建原生窗口、不写系统剪贴板；GPU 呈现、原生键鼠/IME/DPI、人工视觉与其余验收仍需分别验证。
- 原生单/双窗口门禁要求实际 GPU 呈现、日志分页/重排/缩放与设备恢复，双窗口还要求第二窗口呈现。桌面按 egui 输出期限调度重绘，并合并各窗口的光标与 daemon 轮询期限；处理 `RedrawRequested` 时不由事件响应再次请求同一帧，避免 Windows 绘制循环饿死其他窗口。超时证据区分未收到绘制事件与表面跳过，见 [ADR-0018](adr/0018-native-interactive-session-gate.md)。
- 50 MB/s 输出、10 GB 日志视口和滚动锚点达到 Phase 0 指标。
- SFTP 基础上传下载不整文件入内存。

### 对外体验门槛

v0.1/v0.5 构建、原生 CI 通过和内部技术预览仅表示研发进度，不作为邀请用户测试或建议推广体验的依据。对外体验需同时满足：

- 计划范围内的 SSH、SFTP、本地终端及工作区功能达到对应版本验收标准。
- 主工作台、会话导航、连接编辑、标签/分屏、文件面板、状态与错误反馈形成完整 UI；按 Xshell 参考工作流完成任务级对照，并通过视觉与交互回归。
- 在目标系统上完成真实 SSH/SFTP、中文输入法、复制粘贴、缩放与高 DPI 的人工检查；关键问题修复后再复核。

达到这些条件并记录验收结果后，才向用户提示可以推广体验。

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

凭据库当前生命周期以 [ADR-0122](adr/0122-vault-protection-and-on-demand-unlock.md) 为准：本次运行解锁后不因锁屏、休眠、失焦或凭据闲置自动锁定，仅手动锁定或退出程序清除解锁状态。前述 P1 历史条目的租期和桌面自动锁库策略已被替代。
