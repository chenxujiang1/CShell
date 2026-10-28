# Windows 与 macOS 早期试用

目标为 Windows x64、macOS Apple Silicon（arm64）和 macOS Intel（x64）。每个平台单独分发 ZIP，不依赖用户安装 Rust。macOS 运行器采用 macOS 15；更早系统尚需实机验收，不能仅按 CPU 架构推断兼容性。

## 生成试用包

冻结并提交准备分发的源码后，运行 GitHub Actions 的 `ci` 工作流。工作流支持手动触发；提交或 PR 也会触发。`trial-package` 依赖质量、平台、原生资源、窗口恢复和互通门禁，任一依赖失败就不会生成可分发产物。目前本地格式和严格 Clippy 仍未通过，需要先处理；新增打包配置本身不表示这些门禁已经通过。

成功后下载对应 artifact：

| 用户设备 | Artifact | 内部试用包 |
| --- | --- | --- |
| Windows x64 | `cshell-trial-windows-x64` | `CShell-trial-windows-x64.zip` |
| Mac Apple Silicon | `cshell-trial-macos-arm64` | `CShell-trial-macos-arm64.zip` |
| Mac Intel | `cshell-trial-macos-x64` | `CShell-trial-macos-x64.zip` |

每包附 SHA-256；保留包内源码版本用于反馈关联。本地未提交修改的包会标记 `modified workspace`，不能仅凭 HEAD 哈希视为可复现版本。工作流只上传 CI artifact，不创建公开 Release，也不自动发布给用户。

另有 `macOS personal trial` 工作流，向 `codex/macos-trial` 分支推送后构建两个 Mac 架构，执行 SSH、凭据库、存储、本地 PTY、脚本运行时回归及原生钥匙串检查，并验证解压 app 的窗口运行。它用于用户本人早期试用，不代表完整公开发布门禁已经通过；完整 `ci` 的质量检查仍独立运行，不会隐藏其失败结果。向公开仓库推送源码快照前应确认快照内容可以公开。

Windows 包含 `CShell.exe`、`cshelld.exe`、`cshell-script-worker.exe` 和首次使用说明。macOS 包含 `CShell.app` 和外部可读的首次使用说明；三个可执行文件都位于 `Contents/MacOS`，保证桌面程序能找到后台、后台能找到脚本工作进程。不要单独分发 GUI。

本地手动打包方式（在对应原生平台构建后执行）：

```powershell
# Windows：与 CI 一致，构建前设置静态 CRT。
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --locked -p cshell-gui -p cshelld -p cshell-script-worker
.github/scripts/package-trial-windows.ps1 -BinaryDirectory target/release
```

```bash
# Mac：使用对应架构的原生机器。Intel 将 target 和参数改为 x86_64-apple-darwin / x64。
cargo build --release --locked --target aarch64-apple-darwin -p cshell-gui -p cshelld -p cshell-script-worker
bash .github/scripts/package-trial-macos.sh target/aarch64-apple-darwin/release arm64
```

打包产物复用 `artifacts/ui-refresh-20261015/bin` 中固定名称，不生成逐轮历史副本；Mac app 按架构保存在该目录内。更新前正常退出相关程序，保留既有用户数据。macOS 脚本在检测到相关进程时拒绝覆盖。

## macOS 签名限制

当前脚本使用本地 ad-hoc 签名以验证 app 结构，没有 Developer ID 身份和 Apple 公证。它不能提供开发者身份信任，也不能保证浏览器下载后的首次打开会通过 Gatekeeper。

面向普通用户公开分发时，建议补齐 Developer ID 签名、公证及票据装订，流程见 [Apple Developer](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)。当前包更适合愿意手动确认的小范围试用；首次打开应按 [Apple 支持说明](https://support.apple.com/102445) 处理，不要求用户关闭 Gatekeeper 或全局降低系统安全设置。

## 分发前人工验收

CI 新增的包测试验证解压、文件/执行权限、Mac bundle 签名完整性及从解压程序运行真实窗口门禁。此门禁使用离线固定数据，不覆盖首次启动后台、远程 SSH、操作系统凭据库和 Finder 启动，也不等同于无开发环境实机验收。

在 Windows 普通用户环境、Apple Silicon Mac、Intel Mac 各完成一次下列操作，并记录包哈希、源码版本及系统版本：

1. 通过浏览器下载实际交付 ZIP，解压；Mac 将 app 拖入“应用程序”，从 Finder 打开，记录系统提示。
2. 新建 SSH 连接，核对主机指纹，保存凭据并连接；测试密码及加密私钥。
3. 验证中文输入法、实际系统剪贴板、多行粘贴、常用快捷键、vim/tmux、窗口缩放和多屏。
4. 上传下载非关键文件并核对哈希；测试取消和断网重连，确认无命令重放或误发。
5. 执行一个不访问敏感数据的最小脚本，确认工作进程包含在包中且平台隔离可用。
6. 正常退出、重开、重启机器后再连接；验证保存的配置和凭据。Mac 还需检查替换新包后的凭据可用性与授权提示。

没有完成某个架构的实际验收时，暂不向该架构用户提供已验证兼容的承诺。首轮建议邀请少量用户使用测试服务器和非关键文件，反馈版本、系统、复现步骤和脱敏日志；不要收集密码、私钥或验证码。
