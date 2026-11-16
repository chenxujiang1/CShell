# ADR-0073：macOS脚本worker固定Seatbelt访问策略

- 日期：2026-10-14
- 状态：代码接入；未编译、测试、安装策略或实际执行worker，统一后续验证
- 范围：P3-060，接续ADR-0072；进程内存、恢复和live授权继续

## 决策与启动顺序

macOS专用worker在父PID检查之后、创建输入/watchdog线程之前，调用系统
sandbox_init安装内嵌自定义SBPL。使用flags=0，不选择内置命名profile，不读取
外部策略、参数或用户路径，不启动sandbox-exec子命令，不改变GUI/daemon沙箱。
同一进程只允许一次成功安装。之后创建的线程处于该进程策略下。

固定策略默认拒绝，显式收紧process-info、sysctl和signal后，仅允许本进程信号、
自身pidinfo/pidfdinfo/setcontrol、/dev/null字符设备数据读写，以及hw.ncpu、
hw.activecpu、hw.pagesize三个只读硬件查询。不放行用户文件、临时目录、系统库
目录、网络、外部进程、服务发现、IOKit或sandbox extension。系统库须在可信启动
阶段完成所需加载；延迟加载或新增系统资源需求必须经原生验证后单独审查策略。

所有拒绝关闭策略日志，原生编译诊断只用sandbox_free_error释放，不读取或输出。
安装返回值必须为0，sandbox_check(getpid(),NULL,0)必须为1；负值不是成功。
资源限制仍在收到Start后设置，Ready之前再同时检查本模块安装标记与系统沙箱状态，
然后才创建QuickJS上下文。仅有系统“已沙箱化”状态不被当作本策略安装证明。
任一步失败立即结束，不降级为API-only，不尝试另一条宽松策略。

## 继承描述符

安装前使用proc_pidinfo/PROC_PIDLISTFDS枚举自身描述符，上限32768项，检查返回
字节数、对齐和负FD；缓冲填满视作可能截断并拒绝，不按RLIMIT推断最大FD。
在没有应用线程的阶段关闭所有fd>2，再枚举要求恰好只剩0/1/2。关闭失败不重试
复用FD。固定标准输入/输出必须分别为只读/只写匿名管道；stderr必须是只写
字符设备，并与/dev/null的设备、inode和rdev一致。

策略安装后再次清除并核对额外描述符，防止原生profile编译器遗留文件句柄。
这不是Mach port/系统运行时全部继承状态的清单，也不声称清空了进程启动前
已加载的库或系统初始化状态。worker依赖链、继承Mach权利和平台打包继续审查。

## 协议和界面

worker固定启动参数和私有协议、cshell.script-run-report升级v5；Windows原生
启动器和Unix启动器同步使用v5，旧worker或api_only_v1回执明确拒绝。
macOS要求macos_seatbelt_v1，仍分别核对macos_rlimit_v1资源回执。GUI显示
macOS访问策略就绪及“效果待统一验证”，CLI保留现有状态/退出/清理成功条件。
不新增依赖包、SQLite schema或daemon IPC，仍minor54；脚本/计划仍v1。

macOS的CPU/core/文件长度/描述符限制保持，QuickJS heap/stack限制不能当作
OS进程RSS/VM上限。本切片不解决跨进程配额/孤儿恢复、准备/清理总期限、
实际grant/revoke、秘密参数目标API、实时会话代次、触发器和任务执行集成。

## 后续统一验证

需在支持的macOS arm64/x64版本实际验证SDK链接和已弃用API可用性、SBPL编译、
process-info过滤和自身FD枚举、标准管道/空设备、额外及高位FD清理、初始化失败、
引擎创建/线程/匿名内存/时间/随机源、CPU/取消/断管/心跳、禁止文件/网络/进程/
Mach服务及IOKit访问，以及source/参数/异常/尝试路径不进入应用或OS策略日志。
系统API弃用或资源需求变化必须明确失败，不以扩大允许范围自动兼容。

本轮仅源码、一手接口参考及cargo fmt；这些隔离效果和兼容性尚未证明。

## 一手参考

- [Chromium Seatbelt适配器](https://raw.githubusercontent.com/chromium/chromium/main/sandbox/mac/seatbelt.cc)：系统函数签名、原生错误释放和状态查询。
- [Chromium macOS沙箱设计](https://raw.githubusercontent.com/chromium/chromium/main/sandbox/mac/seatbelt_sandbox_design.md)：进程启动时应用自定义策略及提前获取资源的边界。
- [Chromium公共策略](https://raw.githubusercontent.com/chromium/chromium/main/sandbox/policy/mac/common.sb)：默认拒绝、关闭拒绝日志及自身目标过滤语法。
- [Apple WebKit策略](https://raw.githubusercontent.com/WebKit/WebKit/main/Source/WebKit/WebProcess/com.apple.WebProcess.sb.in)：显式收紧process-info/sysctl及自身控制范围。
- [Apple XNU proc_info](https://raw.githubusercontent.com/apple-oss-distributions/xnu/main/bsd/kern/proc_info.c)：自身FD枚举结果的字节数与缓冲截断语义。
