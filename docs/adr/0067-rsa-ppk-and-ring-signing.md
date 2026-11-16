# ADR-0067：RSA PPK 与 ring 签名路径

- 日期：2026-10-14
- 状态：代码接入；编译、测试、审计与真实密钥/SSH验证统一后续完成
- 范围：接续 ADR-0064 的 RSA 缺口，P3-050

## 决策

为有界 PPK v2/v3 增加 RSA，接受2048～4096位模数、65537～2^33−1内的奇数
公有指数，所有整数须为正且有界。DSA/Ed448、PPK v1、其他范围继续拒绝。
不运行 PuTTYgen，不输出转换后的私钥，不提取外部会话密码。

固定库的 PPK RSA 分支被其 rsa feature 关闭；直接启用会引入新的 RSA 依赖。
本切片使用已锁定 ring 0.17.14 作为私钥运算后端，保持上游 rsa feature 关闭。
复用已锁定 argon2、sha1/sha2、hmac、ssh-cipher、ssh-encoding 和 crypto-bigint，
不新增包或升级版本。新增 workspace/SSH 依赖边，lock 只调整 cshell-ssh 的列表。

## RSA PPK 格式接入

沿用64 KiB、行/组件、精确头部顺序、KDF内存/次数/乘积与每进程双槽预检。
Ed25519/ECDSA 继续使用固定 SSH 库的现有解码器；RSA 在相同预检/预算内走单独
的格式适配器，避免把未知算法交给没有启用相应功能的库。

RSA 分支通过库 Base64 解码公/私块；公钥重新编码必须与原二进制一致，拒绝尾随
记录或非规范编码。公钥类型/模数/指数在 KDF 前核对。解密调用 ssh-cipher AES-CBC，
KDF 调用现有 Argon2/SHA 库；没有实现 AES、RSA 模幂或 Argon2 算法。
v3 一次派生80字节的 cipher/IV/MAC材料；v2按格式规则派生SHA-1材料，IV全零。
未加密文件不派生 cipher key，按相应版本生成文件 MAC。

按五个 SSH string 流式计算 MAC，包含完整解密后的随机填充；调用 HMAC库的
verify_slice 比较。MAC 成功后才解析 d、p、q、qInv，各 mpint 至多513编码字节，
正数字节至多512。未加密块须恰好结束；加密块只允许末尾不足16字节的填充。
原注释参与 MAC，但不复制到返回密钥或检查结果。

输入/编码拼接/解密块/KDF材料/独立 mpint 和应用层 CRT 临时整数有 zeroizing
所有权，错误不包含私钥/口令/注释。结构化 RsaPrivateKey 沿用库的清理生命周期。
不声称 ring、HMAC、算术/KDF 库内部所有临时值或堆分配都已审计/清零。

## ring 组件与配对

从受检 d/p/q 计算 d mod (p−1)、d mod (q−1)，使用 crypto-bigint 的固定4096位
精度及非 vartime 求余入口。拒绝 p/q=1，不自行实现大整数或素性测试。
将原 n/e/d/p/q/qInv 与派生 CRT 指数交给 ring::rsa::KeyPair::from_components。
保持来源 CRT 逆元，不计算替代系数以掩盖损坏输入。

构造后用随机32字节加 CShell 专用上下文前缀做 SHA-256 和 SHA-512 双签名，
各自以声明公钥验证；失败拒绝。d参与两个CRT指数的派生，不能仅检查某个来源
私钥对象是否能被构造。该流程不是完整素性证明；ring 的构造检查和签名核对
边界仍按该版本API定义，不能据此声称通用RSA数学审计完成。

同一转换也接到既有非PPK解码成功的RSA对象：能被旧解码器读取的OpenSSH RSA
可使用相同 ring 后端、参数范围和配对规则；不会据此新增 PKCS#1/PKCS#8 容器
解析器。非 RSA 密钥流程保持。密钥对象共享 Arc 签名材料，不在每次签名时重新
从来源读取文件、口令或改变公钥。

## SSH 签名与后台边界

普通公钥和用户证书认证通过 russh 的外部 Signer 接口。每个尝试新建一次性签名器，
绑定完整公钥/完整证书及选择的SHA-2；拒绝不同身份、不同摘要、超128 KiB数据或
第二次签名请求，不回退到其他私钥后端或 SHA-1。证书仍先核对对应私钥，既有
证书/主机信任、目录/凭据代次、期限与取消边界继续作用。

复用现有 RFC8332 摘要选择：有扩展选其SHA-2；无扩展优先SHA-512；明确只支持
旧RSA/SHA-1时拒绝。RSA PPK头部的 ssh-rsa 是密钥类型，不等于允许SHA-1签名。
v2文件MAC中的SHA-1也不改变SSH策略。

ring签名后再次用公钥验证，再追加标准SSH签名blob。签名在 spawn_blocking 中
运行；RSA材料构造/本地配对与运行签名共用第二个每进程双槽预算。Busy不排队
或自动重试；取消中的 blocking 工作仍持有槽与密钥直到结束。原PPK双槽覆盖
解码/KDF/配对，不能因上层超时提前释放并启动更多解码。

交互、多跳、任务和专用连接共用密钥工厂/认证函数；Vault及凭据包沿用已有校验/
绑定/加密保存。Vault将PPK或RSA预算耗尽映射Busy，明确RSA构造错误不触发后备
口令读取。CLI inspect-ppk维持已有命令与公共元数据结构，无新wire或schema，
协议仍minor54。桌面说明补RSA范围与SHA-2，源文件保留且私钥不经过GUI。

## 参考与统一后续验证

- [PuTTY 官方 PPK Appendix C](https://the.earth.li/~sgtatham/putty/0.85/htmldoc/AppendixC.html)：RSA组件顺序、v2/v3派生、填充和MAC字段。
- 本地固定版本的 ring / crypto-bigint / ssh-key / russh 源码API：组件构造、求余、外部SignRequest和签名编码；未打开私钥样例或运行解码。
- [ring KeyPair API](https://docs.rs/ring/0.17.14/ring/rsa/struct.KeyPair.html)：组件检查与本地配对边界。
- [RustSec RSA advisory](https://rustsec.org/advisories/RUSTSEC-2023-0071.html)：采用已存在ring私钥路径的依赖决策背景；本切片不引入RustCrypto rsa包，也不据此宣称整体SSH审计通过。

本轮只有源码、依赖API/公开格式阅读与cargo fmt。没有编译/测试、实际KDF/
签名/密钥解码、审计/MSRV、Vault/GUI或真实SSH运行。
统一门禁需覆盖v2/v3加密/未加密、BOM/EOL/MAC/注释/填充/整数规范、错误口令、
KDF资源与公共参数边界、损坏d/p/q/qInv及伪造配对、RSA签名帧、双摘要与SHA-1拒绝、
完整证书/身份/一次性绑定、配额/取消/迟到任务、Vault与凭据代次，以及真实平台/服务器。

四种厂商的元数据子集与RSA/Ed25519/ECDSA PPK工作流均已有代码；更广容器/版本/
迁移范围和P3-060脚本继续，P3-050统一验证与验收尚未关闭。
