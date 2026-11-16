# ADR-0069：独立 QuickJS 离线 worker 与逐次宿主检查

- 日期：2026-10-14
- 状态：代码接入；编译、测试、实际 JS 和原生 UI 验证统一后续完成
- 范围：P3-060 离线执行切片，接续 ADR-0068；不开放实时 daemon 能力

## 决策

新增 cshell-script-runtime（封闭协议/父进程监督）及 cshell-script-worker
（独立 QuickJS 可执行文件）。桌面和 CLI 只链接监督库，JavaScript 引擎不放进
daemon 或 GUI 进程。锁定 rquickjs/core/sys 0.14.0 与 relative-path 2.0.2；只开
std，不开 loader、dyn-load、rust-alloc、allocator、futures 或调试 dump 功能。
现有依赖未升级，SQLite schema/IPC minor54 保持。

离线计划仍不是 grant。启动前必须显式确认，将完整文档、重新计算相等的计划、
完整且名称精确匹配的临时参数冻结成本次内存请求。所有 requested 必须 selected；
离线模式仅接受空集合或 ui_notify。terminal_read/write、profile_read、task_submit
在进程创建前拒绝。没有持久 grant、自动运行、重试、启动恢复或触发器启动通路。

## 进程与协议

父进程只定位 current_exe 相邻的 cshell-script-worker(.exe)，不查 PATH 或环境
覆盖；清空环境、使用新临时工作目录、关闭 stderr、仅传私有固定参数。Windows
设置 CREATE_NO_WINDOW。发行/开发运行需另行构建并把 worker 放在相邻目录，未找到
明确失败；本轮没有构建/打包或启动它。

stdin/stdout 为四字节大端长度加 JSON，单帧最多384 KiB，分配前检查；拒绝未知
字段/枚举、错误 run_id、序号、文档摘要和追加最终帧后的字节。启动帧没有参数值，
参数保留在父进程，按声明名称逐次读取。RPC 白名单：parameters.require(name)、
clock.sleep(milliseconds)、ui.notify(message)。宿主每次检查身份、序号、名称/
能力、形状和预算；其他调用默认拒绝并结束本次运行。

QuickJS 上下文仅标准内建对象和冻结的 cshell API；没有安装文件、网络、系统
进程、模块加载器、console、daemon 凭据或 Vault API。原生桥接函数初始化后从
全局删除，闭包捕获内建 JSON 函数。宿主等待是 worker 的同步 RPC，API 向 JS
返回 Promise；推荐 source 返回 async IIFE Promise，监督该 Promise 的完成。
没有返回的异步任务不会被当作独立后台任务继续执行。返回值和异常不转字符串，
不提供用户脚本日志；通知只验证/计数，不展示正文。

## 预算、取消与退出证明

- 每个监督进程最多两个 worker，超额立即 Busy，不排队；不是全机器配额。
- 引擎设置内存和栈上限；父进程独立总期限、操作次数及累计宿主结果编码字节上限。
- 中断处理检查总墙钟和扣除宿主同步等待后的活动墙钟，循环 Promise 作业间也检查。
  cpu_time_ms 在此是保守活动墙钟预算，不是 OS CPU 计量；引擎内存不是整个进程 RSS。
- 取消/超时/拒绝/协议失败关闭管道并杀死 worker，最多等5秒退出。正常 Completed
  最终帧/EOF 后额外允许250ms自然退出，避免把正常退出竞态误报失败；非零退出不算成功。
- 最终帧和 EOF 不是退出证明，必须 wait/try_wait 确认。仍不明返回 Unknown，禁止
  自动重放；reaper 保留进程槽和临时目录。监督 future/runtime 被丢弃且未获证明时，
  永久消耗当前进程槽并保留目录，不能提前释放孤儿配额。CLI 退出后无跨进程恢复账本。

这是独立进程和受限 API 的边界，**尚无 OS 沙箱、Job/AppContainer/seccomp 或 OS
进程资源限制**，不能宣称能够安全执行恶意原生引擎利用代码。当前仅显式运行自行
核对的本机离线草稿。OS 级限制和目标绑定的 live broker/grant/revoke 完成前，
实时终端/Profile/任务和后台自动化入口继续关闭。

## 临时值及界面

参数文档为 {"values":{"name":"value"}}，最多32项、每项8192字节、总32 KiB，
名称与声明完全相同，不含 NUL。父进程参数容器、协议缓冲及部分字符串释放时擦除；
JS 引擎和 serde/系统管道仍可能保留临时副本，不能声称完全擦除内存。

GUI 参数输入遮盖显示且不导出，启动冻结后清空。草稿/参数/选择变化清除确认，
活动期间禁用草稿编辑；独立可见提示提供取消。关闭工具窗口继续运行，面板释放
发取消；通道断开和未确认退出阻止本面板新运行。结果显示绑定运行 ID/文档 SHA，
不把结果应用为当前文档保存成功。CLI 参数来自有界文件，不放 argv/env；--confirm
是本次显式离线确认，Ctrl-C 发取消并继续等待退出回执。

报告仅含运行 ID、文档 SHA、状态、操作/通知计数与退出确认，无 source、参数、
通知正文、返回值、异常、字节数或耗时。没有遥测发送或写 daemon 运行记录。

## 后续

OS 级沙箱/资源与父进程失联退出、实际 CPU、跨进程孤儿恢复、live 会话/路由/认证
代次绑定、一次性授权/撤销、终端等待和任务确认、触发器/快捷命令集成继续待编码。
依赖许可证/安全审计、MSRV、编译/测试、畸形协议、超时/取消/退出竞态、平台及
实际 JS/GUI 验证统一后续完成。本轮只依赖解析/获取源码、源码阅读和 cargo fmt。
