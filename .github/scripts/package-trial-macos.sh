#!/usr/bin/env bash
# Run on a native macOS builder after building all three release executables.
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "Usage: bash $0 <repository-relative-binary-directory> <arm64|x64>" >&2
    exit 1
fi
if [[ $(uname -s) != Darwin ]]; then
    echo 'macOS packaging requires a native macOS builder.' >&2
    exit 1
fi
case "$2" in
    arm64) architecture=arm64 ;;
    x64) architecture=x86_64 ;;
    *) echo 'Expected arm64 or x64.' >&2; exit 1 ;;
esac

repository=$(cd -- "$(dirname -- "$0")/../.." && pwd -P)
binary_directory=$(cd -- "$repository/$1" && pwd -P)
case "$binary_directory/" in
    "$repository/"*) ;;
    *) echo 'The binary directory must stay inside the repository.' >&2; exit 1 ;;
esac
for executable in cshell-gui cshelld cshell-script-worker; do
    test -x "$binary_directory/$executable"
    lipo "$binary_directory/$executable" -verify_arch "$architecture"
done
# Never overwrite an application while its desktop or companion is running.
if pgrep -x CShell >/dev/null || pgrep -x cshelld >/dev/null || pgrep -x cshell-script-worker >/dev/null; then
    echo 'Exit CShell and its companion normally before updating the trial app.' >&2
    exit 1
fi
commit=$(git -C "$repository" rev-parse --short=12 HEAD)
[[ $commit =~ ^[0-9a-f]{12}$ ]]
source_status=$(git -C "$repository" status --porcelain --untracked-files=normal)
if [[ -n $source_status ]]; then commit="$commit (modified workspace)"; fi
version=$(python3 - "$repository/Cargo.toml" <<'PY'
import pathlib, re, sys
text = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
section = text.split("[workspace.package]", 1)[1].split("\n[", 1)[0]
print(re.search(r'^version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"', section, re.M)[1])
PY
)
output_directory="$repository/artifacts/ui-refresh-20261015/bin"
app="$output_directory/macos-$2/CShell.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/Licenses"
install -m 644 "$repository/apps/desktop/assets/icons/cshell.icns" "$app/Contents/Resources/cshell.icns"
install -m 644 "$repository/apps/desktop/assets/fonts/OFL.txt" "$app/Contents/Resources/Licenses/NotoSansSC-OFL.txt"
install -m 644 "$repository/LICENSE-MIT" "$app/Contents/Resources/Licenses/LICENSE-MIT"
install -m 644 "$repository/LICENSE-APACHE" "$app/Contents/Resources/Licenses/LICENSE-APACHE"
install -m 755 "$binary_directory/cshell-gui" "$app/Contents/MacOS/CShell"
install -m 755 "$binary_directory/cshelld" "$app/Contents/MacOS/cshelld"
install -m 755 "$binary_directory/cshell-script-worker" "$app/Contents/MacOS/cshell-script-worker"
python3 - "$app/Contents/Info.plist" "$version" "$commit" <<'PY'
import plistlib, sys
with open(sys.argv[1], "wb") as output:
    plistlib.dump({
        "CFBundleDevelopmentRegion": "en",
        "CFBundleExecutable": "CShell",
        "CFBundleIdentifier": "io.github.chenxujiang1.cshell",
        "CFBundleInfoDictionaryVersion": "6.0",
        "CFBundleIconFile": "cshell.icns",
        "CFBundleName": "CShell",
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": sys.argv[2],
        "CFBundleVersion": sys.argv[2],
        "CShellSourceCommit": sys.argv[3],
        "NSHighResolutionCapable": True,
    }, output)
PY
plutil -lint "$app/Contents/Info.plist"
cat > "$app/Contents/Resources/README-first-run.txt" <<EOF
CShell 早期测试版 — macOS $2
源码版本：$commit

1. 解压后将 CShell.app 拖入“应用程序”，再双击打开。后台程序已包含在 app 内。
2. 本包只有本地 ad-hoc 签名，没有 Developer ID 签名或 Apple 公证。
   若系统阻止打开，请核对下载来源与 SHA-256，并按 Apple 的说明处理：
   https://support.apple.com/102445
   请勿关闭系统 Gatekeeper 或全局降低安全设置。
3. 新建 SSH 连接，保存配置和凭据；先通过可信渠道核对服务器 SHA256 指纹。
4. 建议先使用测试服务器和非关键文件。更新前请正常退出程序及其后台。
5. 反馈请附源码版本、芯片类型、macOS 版本、复现步骤和脱敏日志；不要发送密码、私钥或验证码。

请在 Apple Silicon 上选择 arm64 包，在 Intel 上选择 x64 包。

本次更新：
- 服务器重装或主机密钥变化时，展示旧、新指纹；核实后可“确认更新并连接”。取消保留原信任记录。
- 修复粘贴命令时反色高亮文字不可见或只显示一部分的问题。
- 更新程序前正常退出旧版，保留原有用户数据；替换 app 不需要重新创建连接配置。

日志回看与复制：
- 双击选择单词或数字，三击整行；普通单击取消旧选区，Shift+单击扩展。
- tail -f 持续输出时，向上滚动或选择文字会暂停视图跟随，后台继续接收。
- 有新输出时显示提示；点击“返回最新”恢复跟随，不需要发送回车。
- Shift+PageUp/PageDown 或 ⌘↑/↓ 翻页，⌘⇧↓ 返回最新；Mac 快捷键可在鼠标与剪贴板设置中关闭。
- 查找后可点击“返回查找前位置”。历史有保留上限，重要日志请另存。
EOF
# The companion stays beside CFBundleExecutable to match start_companion_daemon.
# Sign nested executable first, then the bundle; this is not Developer ID signing.
codesign --force --sign - "$app/Contents/MacOS/cshelld"
codesign --force --sign - "$app/Contents/MacOS/cshell-script-worker"
codesign --force --sign - --identifier io.github.chenxujiang1.cshell "$app"
codesign --verify --deep --strict --verbose=2 "$app"
archive_name="CShell-trial-macos-$2.zip"
archive="$output_directory/$archive_name"
ditto -c -k --sequesterRsrc --keepParent "$app" "$archive"
# Keep the opening instructions readable before Gatekeeper permits app launch.
zip -j "$archive" "$app/Contents/Resources/README-first-run.txt"
(
    cd -- "$output_directory"
    shasum -a 256 "$archive_name" > "$archive_name.sha256"
)
echo "Trial archive: $archive"
