# 0096 — 终端空格、编辑键与 Tab 路由

原因：winit 将空格报告为 `Key::Named(NamedKey::Space)`，原桌面只处理 `Key::Character` 和部分命名键，因此空格在到达 IPC/SSH 之前被丢弃。Home、End、Insert、Delete、PageUp、PageDown 同样缺失。除此之外，egui-winit 会无条件消费 Tab，即使终端有输入焦点，也会开始工具栏焦点导航。

修复：

- 将命名空格映射为逻辑字符键 `Character(" ")`，保留 Ctrl/Alt/Shift、按下/释放和重复信息；普通终端得到空格，Ctrl+Space 得到 NUL，增强协议由目标终端模式编码。
- 在领域按键、IPC 和终端编码器中补齐六个编辑/导航键。IPC 在现有 0～9 值后追加 10～15，不修改旧编号。同步输入白名单也接受这些键。
- Home/End 处理普通与应用光标模式，Insert/Delete/PageUp/PageDown 使用 tilde 序列；修饰键和 kitty 重复/释放遵循已有编码方式。
- 仅当当前终端可输入且 UI 不需要键盘焦点时，Tab 绕过 egui 的事件处理，防止按键既补全又转移控件焦点。配置框、搜索和其他工具仍沿用 UI Tab 导航。

编码依据：[xterm 控制序列](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html)、[kitty 键盘协议](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)。winit 本地 Windows 源码已经过滤 AltGr 的合成 Ctrl/Alt，保留其逻辑文本处理。

验证：桌面字符矩阵覆盖全部 95 个可打印 ASCII、Unicode/输入法提交文本、Ctrl+A～Z、空格组合键与重复、Enter/Tab/Backspace/方向键/功能键、六个编辑键、IPC 二进制往返和多种目标终端编码模式。真实本地 SSH 服务器在认证和 PTY 建立后捕获接收字节，核对空格、NUL、Unicode 和编辑键的顺序及完整性。同步输入验证覆盖旧式、应用光标和 kitty 目标。

GUI 和后台需成对更新以启用新增 IPC 按键；无需修改保存的连接或用户输入配置。验证使用本地合成服务器，不连接用户的远端服务器；字符矩阵不等同于逐一测试所有物理键盘布局和第三方输入法。
