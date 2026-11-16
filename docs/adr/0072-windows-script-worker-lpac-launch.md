# ADR-0072：Windows脚本worker低权限AppContainer启动

- 日期：2026-10-14
- 状态：代码接入；未编译、测试、创建容器或实际执行worker，统一后续验证
- 范围：P3-060，接续ADR-0071；macOS访问隔离和live授权继续待实现

## 决策

Windows离线worker改由原生CreateProcessW/STARTUPINFOEX适配器启动，每次运行创建
独立LPAC（Less Privileged AppContainer）身份，零能力声明，原Job资源限制在
创建时关联。JavaScript仍只使用声明参数、暂停和通知计数；实时目标入口保持拒绝。

继续使用已锁windows-sys0.61.2，增加Authorization/Isolation/FileSystem/Pipes/
SystemInformation功能边，无新包或版本升级。不使用Rust nightly进程属性API，
Win32指针/句柄/ACL操作封闭在独立适配器；GUI/daemon进程的令牌、权限和ACL不变。
此项为运行时实现代码，本轮没有通过工具实际调用这些Win32变更接口。

## 容器与可执行文件

本次ScriptRunId派生唯一、最多64字符的CShell.Script.<UUID> moniker，只调用创建
新Profile；已有Profile明确失败，不采用/清空它。SID复制到对齐Rust缓冲并释放API
分配，父进程资源所有权可安全随监督/reaper移动，没有跨await裸指针。

只读取current_exe旁的固定worker文件，最多128 MiB，复制到本次新TempDir的固定
exe文件，检查普通文件/长度、独占新文件并sync。只对这个新目录和exe设置保护DACL：
SYSTEM和父用户完全访问，本次容器SID只读/执行。没有对原exe、项目、用户目录或
系统DLL目录扩大权限，不复制关联配置/凭据或扫描DLL。容器当前目录为此只读阶段
目录，加载系统库仍依赖系统已有允许范围与部署环境。

这些检查不构成exe签名/可信摘要证明，也不保证同长度并发修改后的源码副本是
一个原子版本；可信worker部署/打包与实际加载行为继续列入统一门禁。可执行文件
超限、DLL不可用、ACL或容器失败都拒绝，不执行普通权限worker作为备用。

容器仍可能访问系统对该身份开放的资源及其独立私有空间，具体范围由OS访问检查
与显式SID授权共同决定；这里不声明整个文件系统为空，也不提供JS文件接口。

## 原生启动与父进程核对

STARTUPINFOEX属性包含：

- SECURITY_CAPABILITIES：本次Package SID，CapabilityCount=0，不传能力列表。
- ALL_APPLICATION_PACKAGES_POLICY：opt-out=1，要求LPAC，拒绝普通AppContainer降级。
- HANDLE_LIST：只有stdin读端、stdout写端、NUL写句柄；父管道端不继承，Job/令牌/
  进程句柄不在列表中。本启动列表不声称能约束宿主其他原生创建进程的继承规则。
- JOB_LIST：既有单活动进程/CPU/提交内存/关闭终止Job，随创建原子关联。
- MITIGATION_POLICY：禁止Win32k系统调用、扩展点及动态代码。

属性缓冲保证16字节对齐，所有属性值活到列表销毁之后。启动命令行仅含被引号
包裹的阶段exe路径与固定私有参数/父PID，不经shell；UTF-16拒绝NUL和路径引号。
子环境只有OS接口查询出的SystemRoot和固定TZ=UTC0，不继承用户环境或值。
CREATE_NO_WINDOW，不使用挂起/恢复窗口，也不让用户JS进入未隔离启动阶段。

新process/thread句柄立即取得RAII所有权，关闭启动线程句柄，保留进程句柄和
两条父管道。启动成功之后没有可返回错误的初始化步骤；启动尝试期间lease不
假设“没有进程”，以防异常丢弃时提前删除容器或释放配额。明确返回创建失败才
恢复无child证明并清理。

父进程在发送Start前检查本次Job成员身份，以及主令牌：IsAppContainer=1、
IsLessPrivilegedAppContainer=1、Capabilities计数=0、Package SID等于由run_id
派生的SID、完整性标签为Low或更低。令牌信息缓冲最多16 KiB、SID指针范围/形状
检查后复制；不读取越界灵活数组。worker收到Start后再次检查同样身份，连同
既有Job限制核对通过才发送Ready，然后创建JS上下文。

任何属性、身份、SID/能力/完整性、Job或系统版本不兼容都明确失败，无API-only
Windows回退。需要支持LPAC及Job-list的Windows环境，旧系统/宿主嵌套限制允许失败。
主令牌的隔离覆盖之后创建的输入/watchdog线程，未使用可撤回的线程模拟令牌。

## 监督与资源清理

WindowsChild只拥有本次原生进程句柄，零超时WaitForSingleObject查询是否已退出，
确认后读取退出码；wait使用25ms异步暂停，不占阻塞线程等待进程。取消/Drop终止
此进程，退出证明仍来自wait；Job关闭不是退出证明。异步管道通过Tokio文件阻塞
适配器接现有有界协议，保持每次运行双槽、独立心跳及5秒退出等待。

quota/TempDir lease在创建Profile之前建立。成功启动后Profile标记活动，未获得
退出证明不删除。确认退出后先关闭进程/Job句柄，再调用DeleteAppContainerProfile；
只尝试清理本次确实创建的新Profile，结果缓存，不自动重复尝试。创建前错误或
明确创建失败也可清理本次新Profile。进程不明或Profile删除未确认时保留配额与
临时目录，防止无界泄漏/新运行；父进程崩溃留下的Profile不被重启自动采用/删除。

Profile清理回执表示API确认注销/删除本次隔离资源，不是安全擦除内存/磁盘或
所有临时目录物理删除证明。TempDir原有尽力删除保持。阶段文件复制、ACL/Profile
接口为同步调用，5秒上限仅用于退出等待，不承诺整个准备/清理阶段总耗时。
GUI监督runtime使用1秒shutdown_timeout，防止不明阻塞IO/丢弃reaper掩盖已经返回的
报告；丢弃的无证明lease仍保留配额，继续发终止而不认定已退出。

## 私有协议与界面

worker私有协议/固定参数与cshell.script-run-report升级v4，旧worker明确拒绝。
Windows Ready要求windows_lpac_v1；Linux保持linux_seccomp_v1，macOS仍api_only_v1。
脚本/计划v1、daemon minor54、SQLite schema保持。

报告新增sandbox_cleanup_confirmed。Completed只表达JS正常完成；只有正常状态、
已退出且隔离资源清理已确认时CLI才返回成功。GUI分别显示资源/访问策略/清理状态，
任一退出或清理未确认即停止本面板新运行，不自动重放。source、参数、异常、SID、
Profile路径和通知正文不进入报告。首次启动前已取消且无资源时清理回执为true。

## 后续与验证

macOS访问隔离/进程内存、跨进程孤儿Profile/配额恢复、准备/清理时限与故障处理、
live grant/revoke/终端与任务API、触发器集成继续。统一验证必须覆盖Win10/11与
嵌套Job、容器SID/能力/完整性、所有失败路径、保护DACL/阶段加载、DLL与系统资源
可用性、禁止网络/越界文件/跨进程访问、标准句柄继承、Profile退出后删除/删除失败、
阻塞管道/取消/心跳/清理竞态与预算。代码完成不等于这些隔离效果已证明。

## 一手参考

- [Microsoft LPAC启动说明](https://learn.microsoft.com/en-us/windows/win32/secauthz/implementing-an-appcontainer)
- [进程扩展属性及Job/句柄列表](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute)
- [令牌信息类别](https://learn.microsoft.com/en-us/windows/win32/api/winnt/ne-winnt-token_information_class)
- [Profile删除](https://learn.microsoft.com/en-us/windows/win32/api/userenv/nf-userenv-deleteappcontainerprofile)
