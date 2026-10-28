# CShell 图标

选用石墨底色、青绿色 C 形边框和白色 `>_` 提示符。C 对应产品名，提示符对应终端；图形轮廓简洁，在任务栏 16–32 px 下仍可辨认，与现有界面配色一致。

比较过字母 CS 组合、服务器插画及终端符号三个方向；前两者在小尺寸容易粘连或丢失细节，因此采用 C 形终端符号。图标由本项目的几何绘制生成，无外部商标或素材依赖。

- `cshell.svg`：可编辑矢量母版。
- `cshell-{16,24,32,48,64,128,256,512}.png`：通用位图。
- `cshell.ico`：Windows 多尺寸程序 / 桌面图标。
- `cshell-64.rgba`：主窗口和独立 SFTP 原生窗口图标。
- `cshell.rc`：Windows EXE 资源，构建时通过 Windows SDK 编译。
- `cshell.icns`：供未来 macOS 打包使用的资源；本轮未验证 macOS 安装包。

运行 `python scripts/generate-icons.py` 重新生成。主界面使用 `crates/cshell-ui/src/brand.rs` 中同一几何定义的原生矢量绘制，避免为界面引入位图依赖。
