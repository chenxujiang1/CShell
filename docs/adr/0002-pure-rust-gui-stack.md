# ADR-0002：桌面 GUI 采用纯 Rust 图形栈

- 状态：Accepted
- 日期：2026-08-28
- 替代：[ADR-0001](0001-native-terminal-rendering.md) 中的 Qt GUI 选择；保留其原生 GPU 视口与后台持久化原则

## 背景

CShell 的硬指标是高吞吐 SSH 回显、超大日志随机访问、稳定滚动锚点、输入持续响应，以及 GUI 崩溃不终止后台任务。Tauri + xterm.js 的 Web 终端路径不适合作为这些指标的最终性能边界。

ADR-0001 曾选择 Qt Quick，以获得成熟的桌面控件、IME、可访问性和 Android 路径。但 Qt 不会替代本项目必须自研的终端模型、磁盘滚动存储和高吞吐日志视口；同时会增加 Qt/QML 运行库、C++/Rust FFI、第二套构建系统和 LGPL 合规边界。对于以 Rust 为核心、性能关键视口占主要研发量的产品，这些成本收益不足。

WezTerm 已证明纯 Rust GPU 终端可以在 Windows、Linux 和 macOS 成熟运行。`winit` 提供窗口与输入抽象，`wgpu` 提供 DX12/Vulkan/Metal/GL 后端，`cosmic-text` 提供纯 Rust 文本 shaping 与字体回退，AccessKit 提供跨平台可访问性适配。因此不需要为了 SSH 可靠性或终端性能引入 Qt。

## 决策

1. 桌面 GUI 使用纯 Rust：`winit + wgpu`；`egui + egui_dock` 只承载非性能关键的工作台控件。
2. 终端和大日志分别实现自研 `TerminalSurface`、`LogSurface`，直接向共享 `wgpu` device/queue 提交批量几何，只读取可见行与少量 overscan。
3. 禁止使用 egui 普通文本控件保存或渲染终端历史和多 GB 日志；egui 只持有视口句柄、选择状态和控制命令。
4. 终端状态Provider由Phase 0验证并经ADR冻结；当前选择见 [ADR-0004](0004-terminal-core-alacritty.md)。字体 shaping、回退与 glyph rasterization 使用 `cosmic-text + swash`，字形进入有界 LRU atlas。
5. `cshelld` 继续作为独立 Rust 进程负责 SSH/SFTP、输出持久化、VT 解析、终端模型和多主机任务；GUI 只消费版本化快照和日志页。
6. winit、egui、终端Provider等未承诺稳定 API 的依赖必须锁定版本，并封装在项目适配层后；通过三平台输入、IME、可访问性和视觉回归测试保护升级。
7. Android 在桌面 v1.0 后独立立项，复用 core、winit/wgpu renderer 和数据模型，但允许单独设计移动 UI 壳；不为了移动端提前牺牲桌面性能和交互。
8. 只有 Phase 0 证明纯 Rust 栈无法满足中文 IME、读屏、剪贴板/拖放、窗口管理或 GPU 回退硬门槛时，才重新评估 Qt，而不是因控件开发便利默认引入。

## 性能边界

纯 Rust 不自动等于高性能。性能来自以下不变约束：

- 原始输出先进入 append-only journal，再解析和发布视图；GUI 卡顿不能造成数据丢失。
- parser、磁盘分页、搜索、reflow 和 shaping 不在 GUI 事件线程执行。
- GUI 每个显示帧只取得最新的不可变快照；允许合并展示帧，不允许跳过 VT 解析或日志提交。
- 滚动采用 `FollowTail` 与 `Anchored(LineId, cell_offset)` 两种显式状态，后台输出不得移动用户锚点。
- 日志视口内存和 draw call 数量取决于可见行，不取决于日志总大小。

## 后果

正面影响：

- 去掉 Qt/QML 运行库、C++/Rust FFI、CMake 和 LGPL 专属合规工作，降低包体、启动链路、构建复杂度和内存安全边界。
- GUI、终端模型和图形渲染使用同一种语言与类型系统，可以直接共享不可变快照并统一性能剖析。
- `wgpu` 渲染管线可被终端、日志和普通 UI 共用，不需要跨框架复制 cell 或字形数据。

代价与风险：

- 纯 Rust GUI 生态不如 Qt 成熟；原生菜单、复杂表格、IME 边角、可访问性和平台外观需要更多工程投入。
- winit/egui 尚在演进，升级可能包含破坏性变化，必须锁定版本并维护适配层。
- wgpu、字体资源和软件回退仍会产生包体与启动成本；“去掉 Qt”不代表应用自然变小，仍须用发布构建实测。
- Android 能复用核心和渲染器，但移动交互、生命周期、输入法和后台限制仍需独立产品设计。

## 依据

- [xterm.js Flow Control](https://xtermjs.org/docs/guides/flowcontrol/)
- [WezTerm](https://github.com/wezterm/wezterm)
- [WezTerm terminal core](https://github.com/wezterm/wezterm/blob/main/term/README.md)
- [winit FEATURES](https://github.com/rust-windowing/winit/blob/master/FEATURES.md)
- [wgpu](https://github.com/gfx-rs/wgpu)
- [cosmic-text](https://github.com/pop-os/cosmic-text)
- [AccessKit](https://github.com/AccessKit/accesskit)
- [egui](https://github.com/emilk/egui)
