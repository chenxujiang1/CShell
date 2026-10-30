# CShell

CShell 是使用 Rust 开发的原生 SSH、SFTP 与本地终端客户端，提供多标签终端、分屏、连接管理和文件传输。

**当前为早期试用版。** 欢迎通过实际使用反馈问题；建议先使用测试服务器及非关键文件，重要配置和文件请保留备份。

## 获取试用版

目前准备 Windows 和 macOS 试用程序。请按设备架构选择下载，具体可用版本以成功构建的产物为准。

| 设备 | 包名称 | 内容 |
| --- | --- | --- |
| Windows x64 | `CShell-trial-windows-x64.zip` | 桌面程序、后台服务、脚本工作进程 |
| Mac Apple Silicon（M 系列） | `CShell-trial-macos-arm64.zip` | `CShell.app`、首次使用说明 |
| Mac Intel | `CShell-trial-macos-x64.zip` | `CShell.app`、首次使用说明 |

Mac 双架构试用包由 [macOS personal trial 构建](https://github.com/chenxujiang1/CShell/actions/workflows/macos-trial.yml)生成。进入成功的构建记录，在 **Artifacts** 中下载对应架构；构建日志显示失败时，该版本不作为可用试用包。Windows 试用包由完整 [CI 构建](https://github.com/chenxujiang1/CShell/actions/workflows/ci.yml)生成。

下载 Actions 产物需要登录 GitHub；解压外层 artifact 后可以找到试用 ZIP 和对应的 `.sha256` 校验文件。试用产物保留 14 天，过期后需要重新构建。本仓库当前没有承诺稳定版本或自动更新渠道。

## 首次运行

### Windows

1. 解压试用 ZIP 到可写目录，保留全部文件。
2. 确认 `CShell.exe`、`cshelld.exe` 和 `cshell-script-worker.exe` 位于同一目录。
3. 双击 `CShell.exe`。程序会按需启动后台服务，用户无需安装 Rust。

如果沿用此前带 `Start-Trial.cmd` 的本地试用目录，请继续通过该启动器打开，以使用原有 `data` 目录。直接运行 EXE 可能改用系统默认数据目录而显示空列表；此时先返回原启动器，无需重新创建或覆盖配置。

### macOS

1. 在“苹果菜单 → 关于本机”确认芯片类型，下载对应的 arm64 或 x64 包。
2. 解压 ZIP，将 `CShell.app` 拖入“应用程序”，从 Finder 打开。
3. 当前 Mac 试用包只有本地 ad-hoc 签名，**没有 Developer ID 签名或 Apple 公证**。如果系统阻止打开，请核对来源和 SHA-256，再按照 [Apple 官方说明](https://support.apple.com/102445)处理；请勿关闭 Gatekeeper 或全局降低安全设置。

Mac 构建运行器使用 macOS 15。更早系统的兼容性、不同硬件组合及浏览器下载后的首次启动体验仍需实际试用验证。

### 建立第一个 SSH 连接

1. 点击“新建 SSH”，填写主机、端口、用户名及认证方式，并保存配置。
2. 根据认证方式设置密码、私钥及口令，或配置可用的 SSH Agent。
3. 首次信任服务器前，通过可信渠道核对服务器 SHA-256 主机密钥指纹。
4. 连接后检查终端输入与显示，并使用非关键文件尝试 SFTP 上传、下载及取消。

连接会根据凭据的实际保存位置决定是否解锁 CShell 凭据库：系统存储和仅使用 Agent 的连接不要求解锁该库；目标服务器或跳板实际使用库内凭据时才解锁。旧库升级或存储索引需要重新验证时，可能要求一次解锁确认。创建库或切换保存位置不会自动迁移已有密码；库异常时也不会静默改用系统中的旧密码。

## 功能范围

- SSH 连接与配置管理；密码、私钥、Agent 和交互式认证。
- 主机密钥验证、凭据存储和主密码保护。
- 本地终端、多标签、分屏、搜索和会话日志。
- SFTP 文件浏览、上传下载与传输队列。
- 同步输入、批量任务和脚本工具；请先在测试目标上确认执行范围与结果。

各功能仍在持续验证。当前不提供 RDP、串口、TELNET 或自动更新；X11 转发也不属于本次试用范围。Linux 在源码和 CI 中继续维护，当前这轮试用交付聚焦 Windows 与 Mac。

## 已知限制与更新

服务器重装或更换主机密钥后，连接会提示指纹变化，并展示旧、新指纹。通过服务器控制台或管理员核对后，点击“确认更新并连接”即可更新这台服务器的信任记录；取消会保留原记录。也可在“连接管理 → 主机密钥”中检查指纹更新。已吊销或 CA 托管的主机密钥仍需按管理员的信任策略处理。

本轮还修复了粘贴命令时反色高亮文字不可见或只显示一部分的问题，覆盖内置深色、浅色及自定义主题，以及当前屏幕和历史回看内容。

终端复制与日志回看：双击选择单词或数字，三击选择完整逻辑行；普通单击取消旧选区，`Shift＋单击` 扩展选区。默认在松开左键时复制，可在“终端 · 鼠标与剪贴板”关闭；高级设置可启用路径、URL 和 IP 地址的智能双击。

运行 `tail -f` 等持续输出命令时，向上滚动或选中文字会暂停视图跟随，后台仍接收输出。状态栏区分“跟随最新”和“回看中”，有新输出时显示提示；点击“返回最新”或在回看时按 `End` 恢复跟随，不发送回车。`Shift＋PageUp/PageDown` 翻页并保留一行重叠；Mac 默认支持 `⌘↑/↓` 翻页、`⌘⇧↓` 返回最新，可在鼠标与剪贴板设置中关闭。查找跳转后，可用“返回查找前位置”恢复阅读位置。历史保留量有限，重要日志请另行保存。

- 原生窗口的模拟设备丢失恢复测试不等同于真实显卡驱动故障验证。
- Mac 的 Finder 启动、系统授权提示、钥匙串及替换新包后的行为仍需要实机反馈。
- 格式和严格 Clippy 检查仍有待处理的问题；个人 Mac 试用构建不代表完整公开发布门禁已通过。
- 当前通过手动替换程序更新。先正常退出桌面与相关后台，再替换整个程序目录或 app；保留已有用户数据，勿在运行中覆盖程序文件。

## 反馈问题

请在 [GitHub Issues](https://github.com/chenxujiang1/CShell/issues)提交问题，尽量包含：

- 包内源码版本或构建记录链接，以及包的 SHA-256。
- 操作系统版本、CPU 架构、显示缩放比例；显示问题附 GPU 信息。
- 可重复的操作步骤、预期结果、实际结果及复现频率。
- 必要的截图或脱敏日志；连接问题可附服务端类型及认证方式。

**不要提交密码、私钥、私钥口令、验证码或未经脱敏的会话内容。**

## 从源码构建

需要仓库指定的 Rust 工具链及所在平台的 C/C++ 编译工具。macOS 需要 Xcode Command Line Tools；Windows 使用 MSVC 工具链及 Windows SDK。

```sh
cargo build --release --locked -p cshell-gui -p cshelld -p cshell-script-worker
```

运行 `target/release` 下的桌面程序，保持三个可执行文件在同一目录。开发检查使用：

```sh
cargo check --workspace --all-targets --all-features --locked
cargo test --workspace --tests --all-features --locked
```

构建和测试复用默认 `target`。Mac 打包脚本为 `.github/scripts/package-trial-macos.sh`，详细交付说明见 [TRIAL.md](TRIAL.md)。

## 许可

项目代码采用 **MIT OR Apache-2.0**，详见 [LICENSE-MIT](LICENSE-MIT) 与 [LICENSE-APACHE](LICENSE-APACHE)。第三方组件沿用各自许可；内嵌 Noto Sans SC 字体的许可位于 [OFL.txt](apps/desktop/assets/fonts/OFL.txt)，vendored 组件保留其原有许可文件。
