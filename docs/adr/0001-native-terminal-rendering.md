# ADR-0001：终端与大日志采用原生 GPU 渲染

- 状态：Superseded by [ADR-0002](0002-pure-rust-gui-stack.md)
- 日期：2026-08-28

## 背景

CShell 将命令回显、持续输出、超大滚动缓冲和多 GB 日志浏览视为核心指标。要求包括：输入持续可响应、输出不丢失、滚动位置不跳跃、日志大小不决定内存占用，以及 GUI 重启不终止后台任务。

xterm.js 官方流控说明指出，快速生产者可能使模拟器变慢甚至不再响应输入；其典型处理吞吐约为 5～35 MB/s，待处理输入缓冲存在硬上限。这意味着 Tauri + xterm.js 可以作为早期原型，却不适合作为上述硬指标的正式性能边界。

## 备选方案

| 方案 | 优点 | 主要问题 | 决策 |
|---|---|---|---|
| Tauri + React + xterm.js | 开发快、UI 生态好、Android 路径直接 | JS 主线程解析、WebView 差异、输入队列上限、大滚动缓冲受限 | 不作为正式终端 |
| Iced/egui + wgpu | 全 Rust、GPU 直接 | 桌面工作台、输入法、可访问性和移动端成熟度不足 | 暂不采用 |
| 整体 fork WezTerm | 完整 GPU 终端、Rust、MIT | 上游合并成本高、产品 UI 改造大、Android 缺口、滚动缓冲以内存为主 | 只复用终端模型思想/组件 |
| Qt Quick + 原生 TerminalItem + Rust core | 成熟桌面/Android平台、独立渲染线程、可定制 GPU Item | C++/Rust FFI、LGPL 合规、自研渲染工作量 | 采用 |

## 决策

1. GUI 使用 Qt 6 Quick/QML。
2. 终端状态使用 Rust `wezterm-term` 适配层；锁定版本并由兼容测试保护，不把其类型泄漏到领域层。
3. 终端和日志分别实现 `TerminalItem`、`LogViewportItem`，使用 Qt Quick Scene Graph、批量几何与字形图集，只渲染可见行和少量 overscan。
4. 不使用每行一个 QML delegate，也不使用 `QQuickPaintedItem` 作为正式渲染器。
5. `cshelld` 负责输出日志、VT 解析和终端模型；GUI 仅取得版本化的可见帧。
6. 桌面 `cshelld` 从 P0 独立进程运行；Android 复用同一核心但允许进程内运行。
7. Qt 仅使用 LGPLv3 可用模块并动态链接，随项目维护 LGPL 合规清单；任何 GPL-only 模块必须显式拒绝或使项目许可证决策重新评审。

## 后果

- 增加 C++/Rust FFI、GPU 字形渲染和三平台驱动测试的成本。
- 终端吞吐、日志随机访问、滚动锚定和任务存活不再受浏览器主线程约束。
- Android 路径仍然存在，但桌面性能和稳定性优先于移动端开发便利性。
- 必须在 Phase 0 做可运行的垂直切片；如果原生 Item 无法达到指标，项目应暂停并重新选型，而不是回退到未验证的 WebView 方案。

## 依据

- [xterm.js Flow Control](https://xtermjs.org/docs/guides/flowcontrol/)
- [WezTerm terminal core](https://github.com/wezterm/wezterm/blob/main/term/README.md)
- [Qt Quick Scene Graph](https://doc.qt.io/qt-6/qtquick-visualcanvas-scenegraph.html)
- [Qt for Android](https://doc.qt.io/qt-6/android.html)
- [Qt licensing](https://doc.qt.io/qt-6/licensing.html)
