# ADR-0074：脚本单次实时授权与可恢复资源监督

- 日期：2026-10-14
- 状态：功能实现代码接入；未编译、测试或实际运行，统一后续验证
- 范围：P3-060，接续ADR-0068～0073

## 单次授权与宿主API

daemon增加独立SCRIPT_CONTROL能力（bit59）、minor55及envelope63/64。请求为
有界封闭JSON，最多256 KiB，操作与JSON种类必须一致；source/临时参数请求的
Debug与Drop分别脱敏/擦除，拒绝只返回固定状态，不带异常或参数正文。

Preview验证完整文档与全部声明选择，冻结唯一会话实例、Profile目录版本、认证
代次、秘密参数名称白名单、最多8个已创建任务的名称/ID/版本、可选触发事件及
首个可读输出序号。返回文档SHA/权限计划/目标摘要/30秒有效期；仍不生成grant。
最多16份RAM预览，过期清除；不持久化source、权限或参数，不恢复旧授权。

Start必须明确确认且消费预览ID一次，重新检查认证、目录、会话实例、参数和任务。
创建仅本次有效的宿主ScriptGrant，摘要/能力必须匹配，最多执行期限+10秒；
worker不获得grant、daemon凭据或秘密值。最多2个活动运行，保留最多64份RAM
固定状态回执。丢失Start回复用review ID从List定位，禁止自动重放Start。

宿主逐次检查能力、实例、认证代次、目录版本及操作/结果预算；50ms检查失权，
计算循环或sleep期间同样撤销。关闭/替换绑定会话、锁屏、认证代次变化、过期、
显式Revoke和运行结束均不把权限转给新会话/新脚本。撤销不依赖Vault解锁。

API v1接续为：

- terminal.sendText({text,append_newline})：最多8192字节，向固定实例提交一次输入意图。
- terminal.sendParameter({name,append_newline})：宿主按名称解析值；Secret必须在本次目标白名单，值不送入JS。录制生成此接口，不再先require秘密值。
- terminal.waitFor({text,timeout_ms}) / waitForParameter({name,timeout_ms})：固定实例的授权后输出、字面匹配，参数方式仅LiteralPattern。复用有界ANSI/UTF-8匹配器与8 KiB上下文，每次最多扫描256 KiB，返回bool，不返回输出；必须启用会话输出记录。每次等待最多30秒，超时正常返回false。
- parameters.require(name)：实时执行拒绝Secret；Text/LiteralPattern仍受声明与结果字节上限约束。离线语义保持。
- profiles.read()：只返回绑定Profile的ID、名称、kind，不返回连接、路径、认证或凭据。
- tasks.submit(binding)：只能启动预览冻结的既有任务，每个名称最多一次；重新检查ID/版本，复用既有任务确认/调度与权限，不创建任意任务或修改定义。调度由daemon主runtime持有。
- ui.notify(message)：校验并计数，桌面显示计数；正文不进入运行报告、轮询或日志。

输入回执只确认本地意图入队，不证明远端命令结果。已被任务系统接受的任务有其
独立生命周期，脚本结束/撤销不自动取消它；可在既有任务管理中停止。不允许JS
提交任意文件/网络/系统进程/Vault/主机信任操作。

非幂等任务派发前标记在途，确定回执前不清除；取消、异常或丢失回执保留
effects_confirmed=false并报告Unknown。退出证明和资源清理不证明远端效果。
没有自动重试、任务重启或旧授权恢复。

## 触发器与桌面/CLI

Script触发动作只产生带会话、触发运行、rule、输出序号和script_id的待核对事件。
用户选择事件、提供匹配文档并重新预览/确认；当前启用代次和事件必须仍存在。
事件最多领取一次；停止/重新启用触发器、事件淘汰或失权停止该运行后续调用。
事件队列512项，领取集合随淘汰收缩。匹配不隐式继承旧grant，不把匹配正文当参数。

也可在scope设置trigger_on_next_match=true，并先启用含该script_id的触发器：
Preview绑定当前触发运行，Start明确签发Armed单次授权，最多等待30秒下一次匹配。
同会话/script_id最多一份待触发授权，和直接执行共享2个活动运行上限。128槽广播
只传事件身份；下一次匹配领取事件一次后启动已冻结代码，更新事件为已消费。
广播丢失、等待过期、触发器停止/重启、撤销或代次变化均取消，不跳过丢失事件
重新找一次。等待期不创建worker；grant仅为等待增加最多30秒，执行预算保持。
该模式与trigger_event互斥，结束后不自动重新授权/启用，重复匹配不重复运行。

桌面脚本页增加实时范围、完整预览、遮盖参数、明确确认、查询/撤销和独立浮动
状态；切换当前会话请求撤销。源/范围/选择变化使预览失效，参数变化取消确认。
关闭工具窗口继续运行；退出面板实例尽力撤销已知运行，未明启动按review查询。
CLI接live-preview/live-start/live-get/live-list/live-revoke及recover --confirm。
各客户端验证完整回执的种类、身份、摘要、预算与版本，无旧daemon能力回退。

## 准备、退出与清理时限

每个运行由独立所有者线程持有子进程、Job/Profile、临时目录和配额；同步原生
调用不会堵住GUI/daemon调用方executor。准备5秒；执行采用声明期限，调用方
另留250ms观察停止阶段；退出等待仍最多5秒，监督停止阶段5.5秒；清理5秒。
watch阶段只传固定报告，不传source/参数；运行时退出保留1秒有界shutdown。

任何阶段超时请求取消并返回Unknown；原生调用不能被安全强行中断，因此所有者
继续持有资源/锁，不把超时当退出/删除证明，不复制调用清理接口。清理阶段可
保留已确认的exit_confirmed，但sandbox_cleanup_confirmed仍为false。
丢弃调用future会取消；原生调用若永久阻塞，所属槽继续占用，不无限创建所有者。
输出读的blocking closure另持2槽许可，取消后的阻塞读也不能无限积累线程。

## 跨进程双槽与恢复

GUI离线、CLI离线及daemon实时监督共用当前RuntimePaths/script-workers下两份
文件锁/有界记录。默认路径按用户隔离；显式CSHELL_RUNTIME_DIR定义独立配额域，
合作进程须使用同一路径。记录只含版本、run ID、阶段目录文件名、启动阶段、
PID/出生标记和退出证明，不含代码/秘密/权限。普通启动不会采用非空遗留记录。

启动前先同步Launching，取得子进程PID后同步出生标记，之后才可发送Start。
Windows使用创建FILETIME，Linux使用/proc/<pid>/stat的starttime，macOS使用
proc_bsdinfo起始秒/微秒；PID重用不会把新进程当旧worker杀掉。恢复从不发信号。
获得文件锁后，仅已有退出证明、明确未尝试启动或原进程已不存在/出生标记不同
时清理。Windows删除本次run派生的Profile；只移除记录中经过名称、类型、直接
父目录与canonical核对的阶段目录，最后同步清空槽。清理失败保留记录。

启动到记录PID之间崩溃、记录损坏、权限不足或存活身份相同均保留unresolved，
不能从“父进程不在了”推断worker退出。这些情形需要外部证明/人工处理，代码
不会提供无证明的“强制释放”。recover调用方最多等5秒，恢复所有者及其锁继续
持有；同进程最多一个恢复所有者。恢复只修复资源，不复活脚本/会话/远端任务。

桌面恢复后只有槽均已确认清空且用户另行核对未明终端/任务结果，才允许新的
预览；旧Start不重放。CLI打印busy/unresolved/recovered计数，无内部路径/身份。

## macOS补充

固定Seatbelt安装后清空注册Mach端口stash及task bootstrap特殊端口，同时清空
并释放libSystem缓存的bootstrap send right；失败拒绝启动。保留自身task/thread/
host等运行时权利，不宣称系统运行时所有Mach权利均已被删除；实际可达资源与
系统初始化依赖仍须按支持的OS版本审查。

父进程每50ms读取自身child的PROC_PIDTASKINFO，RSS超过引擎声明+128 MiB时
终止，读取失败也停止；这是采样监督，允许采样间超调，不是内核RSS/VM硬上限。
QuickJS heap/stack和既有rlimit继续生效。不以该实现宣称macOS具备Windows Job
同等的进程内存强约束；更强隔离属于平台部署方案，不能通过扩大策略自动补齐。

## 版本与统一验证

worker私有协议/固定启动参数、run report为v6；Start区分offline/live，旧worker
明确拒绝。报告新增effects_confirmed，Completed只表示JS结束，CLI离线成功仍需
退出、清理和效果回执完整。脚本/录制/计划、live review及资源记录仍各自v1。
daemon minor55，无SQLite schema变动；仅增加已有workspace依赖边，锁定0个新包，
无现有依赖版本升级。本轮仅源码/格式化及离线依赖图更新。

P3-060所列草稿/录制、权限UI、离线/实时QuickJS执行、平台限制、撤销/宿主API、
触发请求及资源监督/恢复的实现入口已接通。编译、测试、MSRV/审计、原生GUI、
实际JS/SSH/任务、平台隔离/签名部署及复合故障/老化矩阵均未完成，验收不关闭。
必须覆盖代次/实例替换、预览过期/重复/并发Start、秘密参数流向、在途撤销、
匹配分片/边界/日志禁用、任务派发不明、所有阶段阻塞、PID重用/跨重启/损坏记录、
槽/目录/Profile故障恢复、Mac采样超调和继承Mach权利、旧协议拒绝等。

## 一手接口参考

- [Apple Mach task接口](https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/osfmk/mach/task.defs)及[特殊端口定义](https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/osfmk/mach/task_special_ports.h)：注册stash与bootstrap端口清理。
- [Apple进程信息实现](https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/bsd/kern/proc_info.c)：出生标记及task信息查询。
- [Microsoft DeleteAppContainerProfile](https://learn.microsoft.com/en-us/windows/win32/api/userenv/nf-userenv-deleteappcontainerprofile)：不存在的Profile视作成功、失败状态不明需继续清理。
