# Ctrl+C 中断活性与键盘模式恢复

日期：2026-11-11。分支：`develop/1.1`。状态：本地修复、677 项自动化验证及 Windows 优化构建完成；Mac 实机和远程 CI 未执行。

## 已确认的问题

用户反馈 `docker logs --tail=10 -f neo` 仍存在 Ctrl+C 无效。已核对桌面 GUI 和后台 SHA-256，实际运行的是前一轮修复构建。

1. xterm 扩展键盘状态未随 RIS (`ESC c`)、无参数资源重置 (`CSI > m`)、空值重置和 ESC 重启未完成序列正确恢复。确定性回归显示：执行重置后 Ctrl+C 仍编码为扩展转义串，而不是经典 ETX。现在按重置语义恢复，保留合法的模式协商。
2. SSH 写入循环使用偏置 select 优先处理维护时钟。维护耗时超过间隔、下一次 tick 已就绪时，排队输入可能持续得不到处理。确定性回归同时准备已到期 tick 与中断，旧代码未先取中断；现优先处理已排队命令，维护和授权过期检查仍在每次循环执行。
3. 右键菜单对未匹配的键盘事件一律返回，普通 Ctrl+C/X/G/V 被吞掉。现在仅当菜单仍属于当前终端、输入未被模态或同步确认阻断时，普通控制组合键关闭菜单并继续投递；复制/粘贴组合键保持本地语义。

没有证据将用户具体那一次失败唯一归因于上述某一项；本次修复并验证了这些实际存在的阻断条件，不把普通 ping 测试等同于所有流式命令验证。

## 中断与恢复行为

普通 Ctrl+C 保留与服务器协商的编码。Kitty 的 disambiguate 等模式本来就会将 Ctrl+C 报告为 CSI-u，不能为了中断而全局破坏协议。[Kitty 官方规范](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)；xterm 的资源重置与 RIS 语义见 [xterm 官方控制序列](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html)。

新增右键“发送中断（Ctrl+Break）”及 Ctrl+Break 应急组合键。它使用现有 `InputAction::Control(Interrupt)` 发送 ETX，不受扩展键盘模式影响；只作用于捕获的当前会话，不向实时同步的其他终端广播。会话切换或输入被阻断时拒绝，不杀远程 PID、不自动断开 SSH、不绕过传输/认证代次校验，不在结果不明时重发。

输入未被桌面队列接受时显示错误提示，避免只写后台日志、用户看起来像按键无效。正常字符、编辑框快捷键、Ctrl+Shift+C/V、IME 和 Kitty/xterm 事件继续回归。

中断输入最终仍由服务端 TTY 和程序处理；程序禁用/更改 VINTR、忽略 SIGINT、处于不可中断等待等情况，不能由客户端安全保证结束。恢复入口也不承诺强杀这种进程。

## 验证范围

- 保存修复前的模式恢复和维护抢占失败日志；通过同一测试验证修复。
- 真实 Windows Docker CLI 和本机 Linux Docker 引擎运行独立日志测试容器，使用 `docker logs --tail=10 -f <测试容器>`；通过桌面输入队列、IPC、ConPTY 发 Ctrl+C，确认返回同一 shell，容器仍运行。不读取或操作用户 `neo` 容器。
- Docker 测试镜像只从本机 OS 的 shell、sleep 和依赖库制作，不下载镜像。容器无网络、无主机目录挂载、只读文件系统，资源与日志有界；测试后仅依据 ID、固定名称和所有权标签清理自建容器/镜像。
- 隔离 OpenSSH 测试验证 sleep、tail -f、连续输出并产生终端查询的前台循环；终端模式恢复后再次中断，以及应急 ETX。受管理 B 经 A 的真实 OpenSSH 通道也验证停止前台 tail 后 B 会话仍可使用。
- Windows ConPTY 中的 ping、WSL nano、菜单、目标变化、复制粘贴、扩展键盘编码，以及桌面/后台完整相关回归和严格 Clippy。

真实 Docker 案例使用 Windows CLI + 本机 Linux 引擎；SSH 案例验证实际 Linux TTY 与 A/B 通道。二者不冒充已在用户 SSH 服务器直接操作 Linux Docker CLI。用户当前 GUI 不注入按键、不强制退出，配置、凭据、已有容器均保留。

固定结果：`artifacts/ui-refresh-20261015/logs/control-interrupt-verification.md`。本地程序仅更新现有 `artifacts/ui-refresh-20261015/bin` 一套，不新建 ZIP、版本备份、target 目录；提交推送仍需用户授权。
