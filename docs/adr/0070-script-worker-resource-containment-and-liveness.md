# ADR-0070：脚本 worker 资源限制与父进程失联退出

- 日期：2026-10-14
- 状态：代码接入；编译、测试、实际JS/原生平台验证统一后续完成
- 范围：P3-060，接续ADR-0069；不开放实时目标API

## 决策

把进程资源限制、父进程存活检测和限制就绪握手接进现有离线worker。使用已锁定
的windows-sys0.61.2和libc0.2.189，只新增平台依赖边与Win32 JobObjects/Security
功能，不升级包版本。FFI仅在独立containment平台适配器内允许unsafe；不改GUI/
daemon自身资源限制。

这些限制不构成完整的文件/网络/系统调用访问沙箱，也不把离线权限计划变成live
grant。仍只运行显式核对的本机草稿；终端、Profile、任务与后台触发器入口保持拒绝。

## Windows

父进程创建无名称、不继承的Job，设置KILL_ON_JOB_CLOSE、ACTIVE_PROCESS=1、
PROCESS_MEMORY、PROCESS_TIME和DIE_ON_UNHANDLED_EXCEPTION，不启用breakaway。
进程提交内存上限为声明引擎内存加64 MiB（不是RSS）；用户态CPU时间按声明毫秒
换算成100ns单位。OS CPU统计覆盖worker线程，和现有引擎活动墙钟限制同时生效。

worker启动时先等待私有输入；父进程在发送Start前AssignProcessToJobObject并查询
该Job成员身份。任何设置/关联失败都清理子进程并返回失败，不降级无Job运行。
不使用CREATE_SUSPENDED：受信worker在收到Start前不执行用户JS，父进程的Job关联
在Start之前完成；加载器和受信启动代码不在这个前置约束保证内。

worker收到Start后查询当前Job的限制，检查必须标志、无breakaway、单活动进程、
正CPU/内存上限且不比本次声明派生值宽松，通过后才发Ready并初始化JS。父进程
Job句柄由运行lease/reaper持有；正常退出证明后释放。监督future/runtime提前丢弃
会关闭Job句柄并触发终止，但这不是wait退出证明，原Unknown槽保留规则仍生效。
父进程退出会关闭其最后一个Job句柄；worker不继承该句柄。

嵌套Job或宿主限制不兼容时允许明确失败，不提供breakaway绕过。Job不负责文件/
网络访问权限，AppContainer/受限token等仍待后续编码。

## Linux/macOS

解释器创建前降低当前worker的软/硬资源上限，绝不提高继承的限制：

| 平台 | OS资源限制 | 父进程/失联处理 |
| --- | --- | --- |
| Linux | CORE=0、FSIZE=0、NOFILE≤32、CPU向上取整至整秒、地址空间≤引擎内存+256 MiB | PDEATHSIG=SIGKILL、安装前后getppid核对、独立输入/心跳watchdog |
| macOS | CORE=0、FSIZE=0、NOFILE≤32、CPU向上取整至整秒 | 启动getppid核对、独立输入/心跳watchdog |

Linux在创建输入/watchdog线程前设置no_new_privs和不可dump，线程继承前者。
PDEATHSIG的父对象是创建worker的线程；该线程提前退出也会终止worker。安装后
再次核对实际父PID，覆盖父进程已在安装前退出的竞态。macOS没有在此声称提供
可靠进程VM/RSS上限，继续保留引擎自身堆/栈限制。整秒CPU预算较毫秒声明更粗，
引擎活动墙钟中断仍维持原有较细预算。

资源限制失败时不运行JS；不在GUI/daemon中调用setrlimit，也不放宽已继承限制。
FSIZE不是文件读取/创建权限控制，NOFILE不是继承描述符清理，no_new_privs不是
系统调用白名单；seccomp/文件网络沙箱与macOS系统访问隔离仍待编码。OS因资源
限制直接杀死worker可能没有最终帧，报告协议/进程失败，不臆测成精确资源耗尽原因。

## 私有协议v2与独立存活检测

仅worker私有协议升级至v2，固定参数变为--private-stdio-v2 --parent-pid <本进程PID>。
旧worker明确失败，无v1回退。脚本文档/权限计划仍v1，daemon IPC仍minor54，SQLite
schema不变。Start/Reply/Heartbeat统一为封闭HostFrame，帧上限仍384 KiB；序列化
借用现有文档/参数容器，不为包装帧额外复制正文。

父进程每250ms发一次绑定run_id的Heartbeat，和Start/Reply使用同一异步互斥写入器，
不交错帧；暂停等宿主等待期间心跳独立推进。它不是JS宿主操作，不占操作/结果预算。
取消/超时/协议失败仍由原监督select关闭管道、终止并等待进程，心跳无独立脱离任务。

worker只有一个线程读取stdin，单帧检查后把Reply放入最多一个元素的通道。错误帧、
重复Start、错误run_id、通道满或EOF立即使整个进程静默退出；没有无界回复队列。
独立watchdog线程每100ms检查心跳，连续5秒未收到绑定心跳就退出，包含等待首个
Start/部分帧的阶段。它不依赖JS中断、主线程或宿主RPC返回；输入读取不会因JS
忙循环/同步宿主等待而停止。正常帧到达不能替代Heartbeat刷新存活时间。

首次Start只建立初始存活时间；其后仅正确Heartbeat刷新。全进程暂停、系统睡眠或
父进程调度停顿超过5秒可能导致本次运行失败；不自动恢复/重放。watchdog线程不是
抵抗任意原生代码执行的安全边界。

worker先安装/核对资源限制，发Ready(run_id, sequence=0, containment)，然后开始JS。
父进程拒绝重复Ready、错误平台或Ready之前的Call/Finished。正常调用序号仍从1
连续递增。cshell.script-run-report升级v2，增加可空containment；无Ready时为null，
Ready只证明受信worker报告限制安装，不替代实际平台验证或进程退出证明。GUI只
显示是否收到资源就绪回执，原临时参数/确认/取消/Unknown停新运行行为保持。

## 后续与验证

系统访问沙箱、跨进程配额/孤儿恢复、live授权/撤销及目标API、触发器/任务集成
继续待编码。现有资源/退出代码未实际执行，本轮只读源码/一手API参考、更新依赖
边和格式化。统一后续验证包含Win Job嵌套/CPU/提交内存、Unix继承限制/CPU/VM、
创建线程失效、父线程/进程死亡、部分帧、sleep时心跳、系统睡眠、Ready顺序和
取消/EOF/退出竞态；不关闭P3-060验收门禁。

## API参考

- [Windows Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects)
- [Job资源限制字段](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ns-winnt-jobobject_basic_limit_information)
- [Job限制查询](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-queryinformationjobobject)
- [Linux PDEATHSIG](https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html)
- [Linux no_new_privs](https://man7.org/linux/man-pages/man2/PR_SET_NO_NEW_PRIVS.2const.html)
- [Linux资源限制](https://man7.org/linux/man-pages/man2/getrlimit.2.html)
- [Apple资源限制手册](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/setrlimit.2.html)
