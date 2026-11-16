# ADR-0068：脚本草稿、权限声明计划与脱敏录制

- 日期：2026-10-14
- 状态：基础代码接入；编译/测试/原生UI统一后续验证
- 范围：P3-060首个切片；QuickJS独立执行器与实际授权随后接续

## 决策

在引入运行时之前，先固定可检查的脚本/录制文档与默认拒绝的能力声明。
应用层负责形状/资源上限、文档摘要、离线权限计划、录制及确定生成；桌面/CLI
可读写、编辑和核对。没有执行IPC、运行令牌或隐式授权，不在daemon执行JavaScript。
本切片也没有引入QuickJS包、线程内eval或外部Python适配器。

这为后续独立worker提供输入契约；声明的预算现在仅作结构校验，实际CPU/内存/
中断/超时限制必须由后续worker落实后才能开放运行。JS source在此不作语法编译、
API存在性、行为或安全证明。任意手写source仍可能含用户手工嵌入的敏感信息，
不能把录制器的无载荷保证推广到手写程序。

## 文档与资源声明

`cshell.script` v1包含script_id、revision、名称、source、requested能力集合、
仅有名称/类别的参数定义以及limits。ID采用既有触发器可引用的64字节ASCII
字母/数字/下划线/连字符；revision非零，名称最长128字节，无控制字符。
脚本JSON最多128 KiB，source最多64 KiB，最多32个参数。参数名须以ASCII字母
开头；拒绝constructor/prototype。封闭serde结构和能力枚举拒绝未知字段/权限。

能力为terminal_read、terminal_write、profile_read、task_submit、ui_notify。
不存在默认Vault、任意文件、原始网络、进程、凭据或主机信任管理能力；选择这些
能力也不会绕过未来宿主API的目标/任务确认与凭据代次检查。

预算声明限制：内存8～64 MiB、栈64 KiB～1 MiB、总期限100～30000 ms、CPU
100～10000 ms且不超过总期限、1～256次宿主操作、结果1～64 KiB。默认内存
16 MiB、栈256 KiB、总期限10秒、CPU1秒、64次操作、结果16 KiB。

SHA-256绑定经过完整结构校验的整份规范JSON，含source、能力、参数与预算。
`cshell.script-permission-plan` v1只包含身份/版本/完整文档摘要、声明/选择/缺失
能力、参数定义和预算；不含source或参数值。选择必须是请求的子集，默认空。
这是离线差异计划，不能用作运行授权。后续运行必须重新核对文档及实时会话/
Profile/路由/认证代次并独立签发有期限、可撤销、一次性的执行许可。

## 脱敏录制

`cshell.script-recording` v1最多256步；只允许以下结构：

- SendParameter：参数名和是否追加换行。没有输入文本字段。
- WaitForParameter：字面模式参数名与100～30000 ms等待期限。没有终端输出字段。
- Sleep：1～30000 ms声明式暂停。
- Notify：用户明确填写的公开通知，最多128字节；不从终端自动提取。
- Omitted：终端输入、认证、KBI、广播或会话变化的固定原因标记，无载荷。

录制器不接收InputAction、字节切片、口令或输出正文。桌面仅在当前窗口
submit_terminal_action返回已提交后，传入SessionId和是否广播，追加省略标记。
它记录提交意图，不证明输入送达或执行。连续省略合并，不保存每次按键数量/
长度/时间，也不录真实等待的墙钟间隔。

绑定仅在内存中保存当前SessionId；不随文档导出。会话选择变化或消失时留下
SessionChanged标记并停止；达到步数上限设置truncated并停止。其他窗口/任务/
后台触发器和直接认证RPC不在这个观察入口内，不能宣称完整行为录制。认证/KBI
枚举是无载荷契约，没有接入读取回答的钩子。

生成前必须停止并人工编辑所有omitted/truncated缺口。未处理的省略或截断拒绝
生成；不会自动删除标记后继续。发送通过Secret类别的参数引用生成，等待通过
LiteralPattern参数引用生成，同名参数有冲突类别时拒绝；参数值不在文档中。
通知文本用JSON字符串转义并补Unicode行分隔符转义，不能注入生成代码。

生成代码返回async IIFE Promise，采用API v1草稿调用：parameters.require、
terminal.sendText、terminal.waitFor、clock.sleep、ui.notify。能力由已知步骤
推导，等待/暂停期限累计加1秒开销，并计算require与宿主操作数；超范围拒绝。
这不是执行回放，实际session由未来运行授权绑定，不能从录制恢复进程或认证。

## 桌面与CLI

终端工具增加「脚本草稿」页：开始/停止、声明式步骤、录制JSON编辑、生成、脚本
JSON编辑、默认全不选的能力checkbox与完整摘要核对。录制期间独立可见停止提示；
关闭工具窗口继续录制当前会话，选择变化自动停止，旧窗口/程序退出不自动恢复。
复用终端工具已有输入隔离，不把表单键盘输入发送到会话。

文件读取/导出在每窗口单个后台线程任务进行，普通文件、有界读取、结构校验、
临时文件sync与不覆盖发布。导出冻结点击时文档，迟到导入通过epoch拒绝覆盖
更新后的草稿。只保存用户明确选择的文件，不自动写私有目录/脚本库或默认授权。
文件线程的I/O没有硬期限保证，但不在GUI渲染线程中执行。

CLI增加离线script inspect / permissions / recording-new / recording-inspect /
recording-render；不会发现daemon或连接。inspect/permissions只打印计划，不打印
source或参数值。输出必须新路径并sync，不覆盖原录制；省略、截断、资源/参数
冲突拒绝生成。用法见[SCRIPT_AUTHORING](../phase3/SCRIPT_AUTHORING.md)。

没有新包/依赖版本、IPC/schema/日志格式变化，协议仍minor54。既有RunScript
快捷任务仍是远端任务模板，不被解释成当前自动化JS；触发器Script仍报告需要
后续capability执行器，没有在本切片自动运行。

## 后续与统一验证

QuickJS独立进程/OS边界、宿主broker、CPU/内存/超时/中断、实时grant/revoke/
取消/审计状态、参数秘密生命周期、精确目标API、触发器授权和实际任务接续仍待
编码。不得仅因存在离线权限计划就开放执行。

本轮只源码编辑和cargo fmt；没有编译、测试、JavaScript eval或native GUI运行。
统一门禁补文档/预算边界、未知字段、摘要漂移、权限子集/缺失、secret值不被
录制、连续输入合并、会话变化/广播提交范围、截断、冲突参数、代码转义、文件
晚回复/no-clobber及GUI焦点/IME/多窗口。P3-060与全部统一验收保持开放。
