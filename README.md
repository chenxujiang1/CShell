# CShell

CShell 是一个首发覆盖 Windows x64、Linux x64、macOS arm64/x64 的开源 SSH/SFTP 与本地终端客户端，桌面稳定版之后再考虑其他 CPU 架构和 Android。

项目目标不是复刻 Xshell 的界面，而是在 SSH/SFTP 范围内实现同等级的终端体验、会话管理与安全能力；支持本地 shell，并将 Xshell 式多标签同步输入、可靠批量执行、结果汇总和文件分发作为彼此独立的一等能力。

性能关键路径采用 Rust 后台核心、原生终端模型、`winit + wgpu` GPU 视口和磁盘分段日志；`egui` 只承载非性能关键的工作台控件，不使用 WebView 或普通文本控件承担终端解析与大日志渲染。

Phase 0 工程准入已通过，现已开始 Phase 1 初期开发。产品与架构方案见 [docs/DESIGN.md](docs/DESIGN.md)。开发计划、进度与验收过程记录仅在本地维护，不纳入 Git。

当前明确不做：RDP、串口、TELNET、RLOGIN、自动更新、代码签名和发布流程。X11 转发预留协议边界，在桌面 v1.0 稳定后规划。
