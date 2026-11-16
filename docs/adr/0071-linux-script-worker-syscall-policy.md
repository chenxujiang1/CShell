# ADR-0071：Linux脚本worker系统调用访问策略

- 日期：2026-10-14
- 状态：代码接入；未编译、测试或实际安装过滤器，统一后续验证
- 范围：P3-060，接续ADR-0070；Windows/macOS访问隔离与live授权继续待实现

## 决策

在独立worker中增加Linux原生LP64、小端x86_64/aarch64的seccomp-BPF白名单。
资源上限仍沿用ADR-0070；访问过滤在可信输入/watchdog线程创建、Start结构/摘要
检查和资源限制之后、任何用户JS之前安装。SECCOMP_FILTER_FLAG_TSYNC必须成功
覆盖现有线程，不能只过滤解释器线程而留下输入线程无约束。

不引入依赖或升级版本，继续使用已锁libc0.2.189。BPF生成/FFI仅放在containment
Linux适配器，GUI/daemon不安装过滤器。该worker API仅供独立进程启动路径调用。

## 描述符与启动

输入线程创建前close_range(3, UINT_MAX, 0)，安装过滤器前再次关闭额外描述符。
worker运行契约只有0/1/2，没有需要保留的文件、socket、事件或后台runtime句柄。
只影响独立worker的描述符表；不是对GUI/daemon执行关闭。检查stdin/stdout为FIFO
类型并分别只读/只写，stderr为字符设备1:3（/dev/null），任一检查失败拒绝执行。
可信父进程仍用匿名管道和Stdio::null创建它们；类型检查不声称能从FIFO元数据
证明该管道无第三方端点。

依赖Linux close_range（5.9起）和内核seccomp过滤/TSYNC支持。旧内核、容器外层
策略禁止安装、描述符形状不符或其他安装错误一律失败，不枚举/proc回退，也不
转为未过滤模式。可信加载器和Start JSON解析在过滤前，不把它们宣称为已经受
这个白名单保护；用户JS始终在过滤之后。

父进程清空继承环境后仅设置固定TZ=UTC0，避免JS Date时区解析依赖系统时区文件。
不传入参数值、目录、用户环境或凭据。离线Date使用UTC时区，文件访问不会因日期
功能获得例外；必要libc内部路径调用若被拒绝也不会自动放宽策略。

## 白名单

默认未列出的系统调用返回EPERM；先检查audit architecture，不符终止整个进程。
x86_64额外终止带x32位的调用，防止ABI别名；不支持x32、32位或大端Linux。
检查标量参数高32位为零后才比较需要约束的低字，避免忽略高位别名。

| 类别 | 允许范围 |
| --- | --- |
| 读协议 | read/readv仅fd0 |
| 写协议 | write/writev仅fd1/2，fd2已检查为null设备 |
| 描述符查询/关闭 | fstat/close仅fd0/1/2；fcntl仅这些fd的F_GETFD/F_GETFL |
| 新映射 | mmap仅私有匿名、无PROT_EXEC；标志仅PRIVATE/ANONYMOUS/NORESERVE/STACK/FIXED/FIXED_NOREPLACE |
| 内存管理 | brk/munmap；mprotect不允许PROT_EXEC；mremap只接受MAYMOVE/FIXED；madvise仅NORMAL/DONTNEED/FREE |
| 时间与等待 | clock_gettime仅具名系统/自身CPU时钟，clock_nanosleep仅REALTIME/MONOTONIC/BOOTTIME，拒绝编码其他PID/设备的动态时钟；gettimeofday/nanosleep、futex、sched_yield、restart_syscall；x86_64另允许time |
| 线程内部设施 | rt_sigaction/rt_sigprocmask/rt_sigreturn、sigaltstack、set_tid_address、set_robust_list、rseq |
| 本进程信息/随机 | getpid/gettid/getppid、getrandom |
| 信号/结束 | tgkill仅本worker的tgid，由内核再检查tid归属；exit/exit_group |

没有路径open/stat/readlink、socket/connect、exec/fork/clone、io_uring、文件描述符
接收/复制、pidfd/ptrace/process_vm、凭据修改、mount、prctl或额外seccomp安装许可。
不依据路径字符串指针做检查，也不为newfstatat的AT_EMPTY_PATH开放可被非空路径
复用的例外。资源限制禁止扩大，已有PDEATHSIG/NNP保持；匿名内存仍受现有AS和
引擎预算约束。

输入/watchdog线程使用已有管道、futex及时间调用，属于同一个TSYNC策略。安装后
不再创建线程；为已经创建但仍处于libc启动阶段的线程保留rseq/robust-list等本地
设施。glibc/musl是否还需要其他调用留统一实际验证，没有运行时学习/自动扩权。

这是收缩Linux系统调用攻击面的白名单，不是内核漏洞防护、独立用户/挂载/网络
namespace，也不声称完全抵抗任意原生代码执行。允许的内存/信号调用仍操作worker
自身；已有本进程二进制映射并未擦除。过滤器/运行接口仍需后续审查和实测，当前
只显式运行自行核对的本机草稿，不能据此打开无人值守或live目标权限。

## 协议与报告

worker私有协议/固定启动参数升级v3，拒绝旧worker，无v2回退。Ready在资源限制和
访问策略成功后发送，同时包含containment与sandbox；父进程逐项检查本平台预期
策略，Linux必须linux_seccomp_v1，不能接受api_only_v1。脚本文档/计划v1、daemon
minor54和SQLite schema保持。

cshell.script-run-report v3新增可空sandbox字段：

- linux_seccomp_v1：受信Linux worker报告TSYNC安装成功。
- api_only_v1：Windows/macOS仅有既有资源限制和JS宿主API约束，OS访问过滤待实现。
- null：尚未收到访问策略就绪回执，不推断任何访问隔离已安装。

GUI显示这三种状态；不把Job/rlimit资源回执误报为文件网络访问隔离成功。它们也
不是退出证明或实际测试证明。权限计划仍不是grant；实时终端/Profile/任务/触发器
入口继续拒绝。

## 后续

Windows受限token/AppContainer、macOS访问隔离、跨进程配额/孤儿恢复、live grant/
revoke/目标API及触发器集成继续。统一验证需覆盖BPF全部跳转/边界/架构和高位、
全部默认拒绝族、FD继承/类型、TSYNC所有线程、旧内核/外层seccomp、glibc/musl
分配/时间/线程退出/中断/宿主等待及预算路径；本轮没有执行任何过滤器或JS程序。

## 一手参考

- [内核seccomp-BPF说明](https://www.kernel.org/doc/html/latest/userspace-api/seccomp_filter.html)
- [seccomp系统调用与TSYNC](https://man7.org/linux/man-pages/man2/seccomp.2.html)
- [close_range](https://man7.org/linux/man-pages/man2/close_range.2.html)
- [null设备](https://man7.org/linux/man-pages/man4/null.4.html)
