# Windows 与 macOS 早期试用

当前系列为 1.0，软件版本为 `1.0.0`。发布分支为 `release/1.0`，标签为 `v1.0.0`；GitHub Release 由维护者按 [发布操作文档](RELEASE.md)手动发布。此前 `307ec1d` 等提交生成的包内置版本为 `0.1.0`，不能通过改文件名变成 `1.0.0`，请改用版本标签对应源码的新构建。

目标为 Windows x64、macOS Apple Silicon（arm64）和 macOS Intel（x64）。每个平台单独分发 ZIP，不依赖用户安装 Rust。macOS 运行器采用 macOS 15；更早系统尚需实机验收，不能仅按 CPU 架构推断兼容性。

## 生成试用包

冻结并提交准备分发的源码后，运行 GitHub Actions 的 `ci` 工作流。工作流支持手动触发；提交或 PR 也会触发。`trial-package` 依赖质量、平台、原生资源、窗口恢复和互通门禁，任一依赖失败就不会生成可分发产物。以对应源码提交的完整 CI 结果为准；新增打包配置本身不表示这些门禁已经通过。

成功后下载对应 artifact：

| 用户设备 | Artifact | 内部试用包 |
| --- | --- | --- |
| Windows x64 | `cshell-trial-windows-x64` | `CShell-trial-windows-x64.zip` |
| Mac Apple Silicon | `cshell-trial-macos-arm64` | `CShell-trial-macos-arm64.zip` |
| Mac Intel | `cshell-trial-macos-x64` | `CShell-trial-macos-x64.zip` |

每包附 SHA-256；保留包内源码版本用于反馈关联。本地未提交修改的包会标记 `modified workspace`，不能仅凭 HEAD 哈希视为可复现版本。工作流只上传 CI artifact，不创建公开 Release，也不自动发布给用户。

另有 `macOS personal trial` 工作流，向 `codex/macos-trial` 分支推送后构建两个 Mac 架构，执行 SSH、凭据库、存储、本地 PTY、脚本运行时单元与集成测试、原生钥匙串检查、完整桌面交互单元测试，并验证解压 app 的窗口运行。该分支只运行这套专用 Mac 流程，避免普通 CI 的其他平台任务重复占用 Mac 运行器；其他分支及 PR 继续运行完整 `ci`。它用于用户本人早期试用，不代表完整公开发布门禁已经通过。向公开仓库推送源码快照前应确认快照内容可以公开。

托管 Mac CI 将明确识别的 Apple Paravirtual 虚拟 GPU 与物理 GPU 分开验收：仅在 `CSHELL_ALLOW_VIRTUAL_GPU=1` 时执行功能与资源门禁并报告全部回显延迟，不据此认证物理 Mac 性能。真实 Mac 仍须运行 `cargo xtask echo-latency-bench` 按原 P95/P99 阈值另行验收；物理 GPU 与 CPU 回退阈值均保持不变。

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

## 升级与配置保留

更新前正常退出桌面程序及后台，再替换整套程序文件或 `CShell.app`。不要删除用户数据目录：

- Windows 普通分发版：`%LOCALAPPDATA%\CShell`。
- macOS 普通分发版：`~/Library/Application Support/CShell`；替换或移动 app 不改变该目录。
- 早期 Windows 本地试用目录：保留 `Start-Trial.cmd`、`data` 和 `runtime`。新版在原 `bin` 中直接启动也会识别该布局；复制便携目录时须带上数据，而不能只复制 EXE。

数据路径由桌面和后台共用同一套解析：显式 `CSHELL_PROFILE_DB` 优先于 `CSHELL_DATA_DIR`，其次识别旧试用布局，最后使用系统用户目录。自定义存储使用对应的独立后台目录；显式 `CSHELL_RUNTIME_DIR` 仍优先。路径解析不会扫描其他磁盘，不按文件大小或修改时间猜测数据位置，也不自动复制、覆盖或合并不同数据库。

数据库 schema 升级沿用原有的升级前备份和失败回滚机制。配置 ID 保持稳定，以保留凭据绑定；凭据库、主机信任与显示设置沿用原位置。较新且不支持的 schema 会拒绝打开，不能通过创建空库掩盖失败。系统钥匙串仍属于原操作系统账户，替换签名发生变化的试用程序时可能再次出现系统授权提示。

升级回归检查在临时目录中验证：旧布局直接启动、显式路径优先级、普通应用移动、多个数据库后台隔离、缺失数据时拒绝回退、旧 schema 升级、配置和文件夹保留、旁路文件不变、重开不重复迁移，以及解锁后原凭据仍可读取。人工验收还须使用实际下载包完成一次旧版覆盖升级。

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
7. 使用持续输出的测试日志运行 tail -f，验证向上滚动、翻页和选择文字时视图固定、复制内容不随输出变化；确认新输出提示、返回最新、查找后返回原位置以及鼠标在窗口外松开后的行为。分别检查触控板惯性滚动与鼠标滚轮，并观察长时间运行的内存；历史上限生效时不能把已回收历史冒充为完整日志。

没有完成某个架构的实际验收时，暂不向该架构用户提供已验证兼容的承诺。首轮建议邀请少量用户使用测试服务器和非关键文件，反馈版本、系统、复现步骤和脱敏日志；不要收集密码、私钥或验证码。
