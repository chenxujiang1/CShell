# CShell 1.0.0 手动发布操作文档

本次发布将 1.0 系列统一使用软件版本 `1.0.0`、维护分支 `release/1.0`、固定标签 `v1.0.0`。此前的 `codex/upgrade-ci` 分支改名为 `release/1.0`。版本标签在核对构建通过后固定，不随 main 后续开发移动。本文不执行发布；最终由仓库维护者在 GitHub 点击发布。

## 1. 先替换此前下载的旧包

此前提交 `307ec1d` 及其更早试用包的内置版本是 `0.1.0`。这些包可继续个人试用，但不能作为本次 `1.0.0` 发布附件，也不能通过改 ZIP 文件名变为新版。请下载本次版本提交重新构建的程序；将下载目录中同名旧文件替换，避免混用。

1. 登录 GitHub，打开 [CI 工作流](https://github.com/chenxujiang1/CShell/actions/workflows/ci.yml)，选择 `release/1.0` 分支。
2. 打开全部检查成功的 `1.0.0` 构建记录。若构建失败或仍在运行，先不要发布。
3. 对照 [v1.0.0 标签](https://github.com/chenxujiang1/CShell/tree/v1.0.0)关联的提交，确认构建记录的源码提交一致。不要用其他提交的包代替。
4. 在构建记录底部 **Artifacts** 下载以下三个 artifact。下载 Actions 产物需要登录 GitHub，操作入口见 [GitHub 官方说明](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/download-workflow-artifacts?tool=webui)。

| 设备 | 下载的 artifact | 解压外层后需要上传的文件 |
| --- | --- | --- |
| Windows x64 | `cshell-trial-windows-x64` | `CShell-trial-windows-x64.zip` 和同名 `.zip.sha256` |
| Mac M 系列 | `cshell-trial-macos-arm64` | `CShell-trial-macos-arm64.zip` 和同名 `.zip.sha256` |
| Mac Intel | `cshell-trial-macos-x64` | `CShell-trial-macos-x64.zip` 和同名 `.zip.sha256` |

Actions 下载得到的外层 artifact ZIP 只是运输容器。**先解压这一层，再上传里面的程序 ZIP 和校验文件，共 6 个文件。** 保留内部文件原名，不要重压程序 ZIP，也不要只上传解压后的 EXE 或 app。GitHub 自动附带的 `Source code` 是源码，不是可运行程序。

本仓库 Actions 产物保留 14 天，请在到期前下载。发布附件由维护者上传到 Release，不能只在说明中放临时 Actions 下载链接。

## 2. 校验附件与版本

先把上述 6 个文件放在同一目录。在该目录执行校验；校验成功后保持 ZIP 内容不变。

Windows PowerShell：

```powershell
$packages = @(
    'CShell-trial-windows-x64.zip',
    'CShell-trial-macos-arm64.zip',
    'CShell-trial-macos-x64.zip'
)
foreach ($package in $packages) {
    $expected = ((Get-Content -LiteralPath "$package.sha256" -Raw).Trim() -split '\s+')[0]
    $actual = (Get-FileHash -LiteralPath $package -Algorithm SHA256).Hash
    if ($expected -notmatch '^[0-9a-fA-F]{64}$' -or $actual -ne $expected) {
        throw "校验失败：$package"
    }
    Write-Output "校验通过：$package"
}
```

Mac 终端：

```bash
shasum -a 256 -c CShell-trial-windows-x64.zip.sha256
shasum -a 256 -c CShell-trial-macos-arm64.zip.sha256
shasum -a 256 -c CShell-trial-macos-x64.zip.sha256
```

再检查程序 ZIP 中的 `README-first-run.txt`：软件版本须为 `1.0.0`，源码版本须与标签提交的前 12 位一致，且不能有 `modified workspace` 标记。校验的是程序 ZIP 的哈希，不是外层 artifact ZIP 的哈希。

在对应机器解压试运行时还可以检查：

- Windows：`(Get-Item -LiteralPath .\CShell.exe).VersionInfo | Select-Object FileVersion, ProductVersion`，两项均应显示 `1.0.0`；数值资源版本为 `1.0.0.0`。
- Mac：`/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' CShell.app/Contents/Info.plist` 应输出 `1.0.0`；同一 plist 中 `CShellSourceCommit` 应与标签提交对应。

替换本机试用程序前，正常退出 CShell 及相关后台，保留用户数据。升级和数据目录说明见 [TRIAL.md](TRIAL.md#升级与配置保留)。

## 3. 创建 GitHub Release 草稿

打开 [新建 Release 页面](https://github.com/chenxujiang1/CShell/releases/new)。GitHub 的界面操作见 [官方发布说明](https://docs.github.com/en/repositories/releasing-projects-on-github/managing-releases-in-a-repository?tool=webui)。

1. **Choose a tag**：选择已经存在的 `v1.0.0`。若该标签尚不存在，先停止，待版本核对完成；不要临时从正在移动的 main 创建同名标签。
2. **Release title**：填写 `CShell 1.0.0`。
3. **Describe this release**：复制 [docs/releases/v1.0.0.md](docs/releases/v1.0.0.md)的正文。它已包含下载选择、升级说明和已知限制。
4. 将步骤 1 中准备的 **6 个文件**拖入附件区域，等待全部上传完成。
5. 本版仍用于首次公开收集反馈，建议勾选 **This is a pre-release**，暂不标为稳定最新版。`1.0.0` 是软件版本，GitHub 的 pre-release 标记用于说明当前公开试用状态。
6. 先点击 **Save draft**，检查草稿中的标签、说明和附件。若仓库启用了不可变 Release，尤其需要在发布前补齐附件；发布后的附件修改受 GitHub 限制。

## 4. 由你完成最后发布

在草稿中逐项核对：

- 标签为 `v1.0.0`，关联本次 1.0.0 构建提交。
- 该提交的完整 CI 已通过，三个打包任务均成功。
- 附件恰好包含 Windows x64、Mac arm64、Mac x64 的 3 个程序 ZIP，以及对应 3 个 `.sha256` 文件。
- 包内版本和说明均为 `1.0.0`，没有上传此前 `0.1.0` 包、外层 artifact ZIP、本机数据或凭据。
- 发布说明如实写明公开试用、Mac ad-hoc 签名及未完成 Apple 公证等限制。

核对无误后，由你点击 **Publish release**。发布后确认公开页面的附件可下载，至少重新下载你本机对应架构的 ZIP 并核对哈希，然后把 Release 链接发给试用用户。

如果发布前发现附件错误，保留草稿并替换正确附件。若源码需要再改动并重新构建，应先重新核验版本归属；不要把不同提交的附件混入已冻结的 `v1.0.0`，也不要静默移动已发布标签。
