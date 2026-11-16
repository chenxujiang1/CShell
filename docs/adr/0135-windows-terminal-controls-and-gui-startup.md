# Windows 终端控制键与无控制台启动

日期：2026-11-10。分支：`develop/1.1`。

## 问题与处理

现有 Windows `CShell.exe`/`cshell-gui.exe` 的 PE 子系统为 3（Console）。GUI 入口改用 Windows 子系统；测试二进制继续保留控制台。伴随后台仍通过已有 `CREATE_NO_WINDOW` 启动，不用隐藏主窗口来掩盖控制台。

终端获得键盘输入时，Tab 和 Ctrl 字符组合键先于 egui-winit 处理。该库会把 Ctrl+C/X/V 转换成 Copy/Cut/Paste 事件；终端不应把这些控制键同时交给界面剪贴板逻辑。配置框、搜索框和命令栏持有焦点时仍由界面处理。终端复制/粘贴仍为 Ctrl+Shift+C/V 或 Ctrl+Insert/Shift+Insert；裸 Ctrl+C/V 保留远端控制键含义。

点击终端正文显式交接焦点，不要求窗口一定存在同步输入控制器；同步确认阻断仍保留。点击覆盖在终端上的返回按钮、滚动条或下方命令栏不会抢走其焦点，模态窗口打开时不交接。

控制键继续走已有 InputAction、IPC 和终端编码器；不全局强制写入裸控制码，不更改服务端已协商的 Kitty/xterm 键盘模式，不通过结束会话或杀死命令来实现中断。

## 验证与交付

- 检查终端键盘拥有权、编辑框保护、IME Process/死键保留、26 个 Ctrl 字母、复制粘贴约定和既有键盘模式。
- Windows 真正的 ConPTY 中运行 `ping -t 127.0.0.1`，通过桌面输入队列和 IPC 发送 Ctrl+C，再确认同一个 shell 可以执行后续命令；不是检查输入回显。
- 显式测试通过 Windows ConPTY 运行 WSL Ubuntu 的真实 nano，验证 Ctrl+G 帮助、Ctrl+X 返回、Ctrl+C 位置提示、Ctrl+X 正常退出。夹具需要应答 ConPTY 光标查询，且忽略用户 nano 配置；不编辑或保存文件。无 Ubuntu/nano 的平台默认忽略该项，不能计为通过。
- 运行桌面完整回归、IPC/终端编码测试、严格 Clippy，构建后检查 PE 子系统必须为 2（Windows GUI）。这是输入路径与可执行文件验证，不能替代所有输入法、键盘布局和服务器的手工按键验收。

固定产物仅覆盖 `artifacts/ui-refresh-20261015/bin` 中一套 Windows GUI、后台及脚本工作进程；运行中的文件不得覆盖。不自动结束用户当前的桌面试用包或 SSH 会话，不写入配置、凭据和会话数据，不提交推送或新建压缩包。

原公开 Windows 包以 `CShell.exe` 启动。更新时先正常退出旧程序，将修复版 `cshell-gui.exe` 复制到原包并命名为 `CShell.exe`，同时替换同目录的 `cshelld.exe`、`cshell-script-worker.exe`，保持原配置目录和原启动方式。直接启动项目内部的历史试用目录可能选中其独立测试数据库，不能据此判断原连接丢失。

最终检查结果、产物校验和及未覆盖运行程序的状态记录在 `artifacts/ui-refresh-20261015/logs/windows-input-startup-verification.md`。
