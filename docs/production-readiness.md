# 生产级沙箱服务标准与对标分析

> 本文档回答一个问题：**"生产级可用"对沙箱服务到底意味着什么？** 结论来自两个证据源：
> （1）对 4 个成熟开源沙箱项目的本地代码审读（`vendor/`，只读）；（2）对沙箱应用场景与监控/性能标准的网络检索（browseros，Google）。每条标准都给出参考项目里的真实文件证据，以及 b-sandbox 的落地状态。验证报告见 `docs/production-report.md`。

## 1. 应用场景（为什么需要沙箱服务）

| 场景 | 说明 | 来源 |
| --- | --- | --- |
| AI Agent 代码执行 | Agent 生成的代码在隔离环境运行（代码解释器、数据分析），代码不可信（Veracode：45% 的 LLM 生成代码含安全缺陷） | [K8s Agent Sandbox SIG](https://agent-sandbox.sigs.k8s.io/docs/use-cases/code-execution/)、[Blaxel](https://blaxel.ai/blog/remote-code-execution-sandboxes) |
| 无人值守运维/诊断 Agent | Agent 连续执行诊断命令，命令延迟决定 MTTR（50 ms vs 1 s 每命令的差距决定收敛还是漂移） | [The New Stack](https://thenewstack.io/how-to-measure-sandbox-performance-for-ai-driven-development/) |
| 生产事件响应/支持复现 | 一次性环境复现客户问题，环境启动速度影响首因体验，命令延迟决定解决时长 | 同上 |
| 短时分析任务 | pandas/matplotlib 类生成代码，跑完即销毁 | K8s Agent Sandbox SIG |

隔离架构谱系（Blaxel 总结 + vendor 代码佐证）：**microVM**（独立内核，<5 MB 额外开销，Firecracker/-libkrun）> **gVisor**（用户态 syscall 拦截）> **容器**（共享内核，NIST SP 800-190 指出攻击面更大）> **进程级沙箱**（Seatbelt/bwrap，同内核但强制访问控制）。b-sandbox 当前是进程级沙箱层，通过 backend trait 预留 microVM 升级路径（`docs/m3-microvm.md`）。

## 2. 生产级标准清单

从参考项目的真实工程实践提炼，共 7 类 24 条。**加粗**为本轮补齐项，❗为已知边界。

### S. 稳定性

| # | 标准 | 参考证据 | b-sandbox 状态 |
| --- | --- | --- | --- |
| S1 | **优雅停机**：SIGTERM → 停止接流 → 清理在途工作 → 退出 | CubeSandbox `CubeAPI/src/main.rs:241` `with_graceful_shutdown`；CubeMaster `graceFullStop()` 120 s 上限 | ✅ `main.rs::shutdown_signal`：SIGTERM/SIGINT → 关停全部沙箱（单一终止路径）→ SSE 收口 → axum drain。真实二进制冒烟验证 |
| S2 | **崩溃恢复**：启动时回收上次进程遗留的环境 | CubeSandbox `Cubelet/.../startup_recover.go`、microsandbox `maintenance.rs` 250 ms 预算扫描 | ✅ `reaper::recover_orphans`：启动即清理 `data_root/sandboxes/*`，模板不动，计数进 /metrics |
| S3 | **单实例保护**：同一 data_root 禁止双实例（否则互相把对方沙箱当孤儿回收） | microsandbox SQLite 租约 + pidfd 存活校验 | ✅ `state::ServiceLock`：flock(LOCK_EX\|LOCK_NB)，内核随持有者 fd 消亡自动释放——容器内 pid 恒为 1 的 stale-pid crash loop 从模型上根除；锁文件内容仅诊断 |
| S4 | panic 不得杀死连接 | （tower-http 是 Rust 生态标准做法） | ✅ `CatchPanicLayer` → 单连接降级为 internal 信封 500 |
| S5 | 失败可分类：timeout ≠ backend 失败；错误码闭包 + 契约锁定 | mxc `MxcErrorCode` 12 变体含 `stale_id`；CubeSandbox `ReasonForTaskExit` | ✅ 闭包错误枚举 + `tests/contract.rs` 双向锁定；❗ `stale_id` 式"资源已消失"码未单列（404 已覆盖语义） |
| S6 | 终止要证明，而非假设 | microsandbox `stop.rs:219-290` 循环重取生命周期锁 | ✅（进程后端：stop+deprovision 返回即清理完成；❗ microVM 级"证明终止"随 M3 落地） |

### P. 性能

| # | 标准 | 参考证据 | b-sandbox 状态 |
| --- | --- | --- | --- |
| P1 | **命令执行延迟是核心指标**（p50/p95），启动时间只是首因成本 | [The New Stack](https://thenewstack.io/how-to-measure-sandbox-performance-for-ai-driven-development/)：agent 场景数百命令/任务，每命令延迟复利 | ✅ `bsandbox_exec_duration_seconds` 直方图 + ledger 每沙箱 wall ms；实测 p50 8.3 ms / p99 19.5 ms（Seatbelt echo，release） |
| P2 | HTTP 层开销有预算门槛（calibrated，防 10x 回归） | CubeSandbox `cubebench.sh` + `reflink_ops.rs` 基准 | ✅ `tests/perf.rs`：单周期 <2 ms（基线 0.195 ms）、并行吞吐 >300 cycles/s（基线 ~2700/s）、Seatbelt mean <50 ms（基线 5.2 ms） |
| P3 | 快照/clone 冷启动加速 | CubeCoW FICLONE；microsandbox COW + 快照恢复 <100 ms；E2B snapshot resume | ✅ APFS clonefile / FICLONE 模板物化（M4）；❗ 运行中 VM 的 pause-snapshot-fork 属 M3 |
| P4 | 并发吞吐可测量 | CubeSandbox cubebench 512/s @50 并发 | ✅ `tests/bench.rs`（32 并发 2792 cycles/s，debug 构建）；`scripts/bench.sh`（真实二进制） |
| P5 | 预热池 | K8s Agent Sandbox `SandboxWarmPool`；Cubelet TAP 池预热 | ❗ 未做（进程后端 spawn 5-8 ms，收益有限；microVM 阶段必须做，已写入 m3-microvm.md §6.5） |

### R. 资源

| # | 标准 | 参考证据 | b-sandbox 状态 |
| --- | --- | --- | --- |
| R1 | 配额是声明式策略位，拒绝优于降级 | mxc `validator.rs` 每个能力位独立拒绝消息；b-sandbox 自有模型 | ✅ 9 个能力位 + `check_policy_support`；macOS 无 RLIMIT_AS 即拒绝 BUDGET_MEMORY（fail-closed）。预算字段**存在即意图**（`Option` 模型）：显式设默认值（如 `memoryMb:2048`）同样需要后端可执行该上限，堵死"显式请求被当缺省"的绕过；rlimit 以硬上限写入（rlim_cur=rlim_max，工作负载无法自行抬回） |
| R2 | 输出/输入有上限，防淹没 | microsandbox `ExecLimits`（1 MiB output）；E2B 流上限 | ✅ exec 输出字节帽 + truncated 标志 + 8 MiB 请求体上限 |
| R3 | 服务自身资源占用可测量 | CubeSandbox 基准记录 18.7→24.6 MiB/VM @100→500 | ✅ `scripts/bench.sh` 输出 RSS（idle/20 沙箱） |
| R4 | 空闲回收 | microsandbox `idle_timeout`；CubeMaster CLM 5 s 扫描 | ✅ idle-TTL reaper（15 s 扫描，7 天 TTL 上限，activity 由数据面驱动） |

### O. 可观测

| # | 标准 | 参考证据 | b-sandbox 状态 |
| --- | --- | --- | --- |
| O1 | metrics：计数 + 直方图 + gauge，基数闭包 | CubeSandbox `scheduler/metrics.go`："Cardinality is intentionally kept small and closed"；microsandbox 把 run_id/pid 设为默认关闭的高基数标签 | ✅ route-pattern 归一化（`normalize_route`）保证 HTTP 标签基数 = 路由表大小 |
| O2 | 结构化日志（tracing），级别可配 | microsandbox `tracing` + EnvFilter（并显式静默泄密的 oci_client）；CubeAPI NDJSON 文件日志 | ✅ tracing + EnvFilter（`B_SANDBOX_LOG`），stderr 输出；❗ NDJSON 落盘/外发未做（部署侧采集） |
| O3 | 生命周期事件流（可重放） | CubeMaster Redis Stream（maxlen 100000）；E2B events stream + retention 标记 | ✅ 每沙箱 EventLog + REST 分页 + SSE（Last-Event-ID 重连）；沙箱终止/服务关停时流确定收口 |
| O4 | 遥测覆盖完整流量（含 401/404/413） | Northflank 四层遥测（任务/策略/运行时/平台） | ✅ observe 中间件在最外层，全部请求计入 http_requests_total |
| O5 | 在途/健康 gauges | Cubelet `cubesandbox_node_sandbox_count` | ✅ active/in-flight/execs_active/event_streams 四个 gauge |
| O6 | usage 账本（per-sandbox） | E2B `SandboxMetric` 作为 API 资源 | ✅ `ledger.rs`：exec 计数/耗时/输出字节/file ops/活动时间，暴露在 detail 视图 |

### M. 可管理

| # | 标准 | 参考证据 | b-sandbox 状态 |
| --- | --- | --- | --- |
| M1 | 完整生命周期 API（create/list/get/delete/pause/resume） | E2B spec；CubeSandbox `lifecycle.md` | ✅ + commit（rootfs→模板）|
| M2 | 幂等创建 | E2B `Idempotency-Key` + 409 族 | ✅ Idempotency-Key 头，重放返回原沙箱 |
| M3 | 模板/快照不可变 | CubeMaster restore_compat 拒绝不兼容恢复 | ✅ 模板只读（已存在即拒绝覆盖），clonefile/FICLONE 物化 |
| M4 | API 契约版本化 + 契约测试 | E2B "契约即产品"（spec 驱动双 SDK 生成） | ✅ OpenAPI 3.0.3 v0.3.0 + `tests/contract.rs` 三向锁定（路由↔spec↔错误码） |
| M5 | 版本可查 | （ops 基本需求） | ✅ /health 返回 `version` |
| M6 | SDK | E2B 双语言 SDK 从 spec 生成 | ✅ Rust typed SDK，真实 TCP 端到端测试 |

### Sec. 安全

| # | 标准 | 参考证据 | b-sandbox 状态 |
| --- | --- | --- | --- |
| Sec1 | 默认拒绝 + 显式放行 | mxc `(deny default)` 头 + 尾部 deny 规则 | ✅ Seatbelt deny-default profile；net 默认拒绝，loopback 单孔 |
| Sec2 | egress 凭据不进沙箱 + 目的地过滤 | CubeEgress `secret_ref` 注入；microsandbox 默认封禁 RFC1918/ULA/loopback/169.254.169.254 并做 DNS 解析校验 | ✅ 凭据注入代理（配置主机替换 Authorization，其余透传）+ **EgressPolicy::DenyPrivate**：CONNECT/转发一律拒绝私网/环回/链路本地/CGNAT，DNS 名解析出被禁地址即整体拒绝（防 rebinding），连接 5s 超时，拒绝可观测（403/502 信封） |
| Sec3 | 路径逃逸=攻击（fail closed） | mxc `filesystem_resolve.rs` 最严 wins | ✅ canonical + 逃逸即 400；对抗测试覆盖 traversal/symlink |
| Sec4 | 认证：常量时间比较；探测端点免认证 | CubeAPI 三模式 auth（回调不可达=内部错误，绝不放行） | ✅ Bearer + ct_eq；/health 恒开 |
| Sec5 | 双实例/越权防护 | microsandbox 目录 0700、symlink 拒绝 | ✅ ServiceLock；❗ data_root 权限 0700 约定未在代码里强制（部署文档约定） |

### T. 测试

| # | 标准 | 参考证据 | b-sandbox 状态 |
| --- | --- | --- | --- |
| T1 | 对抗测试是产品 | mxc 5431 测试 + fuzz 目标；microsandbox 每测试独立 HOME | ✅ traversal/symlink/悬空 symlink/洪泛/fork bomb（差分证据：上限 2048 完成 vs 上限 64 EAGAIN）/profile 注入/终止后进程组回收/egress 私网拒绝/body 走私（backend + proxy 测试套件） |
| T2 | 契约测试锁 API 面 | E2B 契约即产品 | ✅ 见 M4 |
| T3 | 性能预算即测试（标定而非拍脑袋） | CubeSandbox cubebench 公布测量方法 | ✅ perf.rs 预算注释含测量日期与基线 |
| T4 | 每个能力位有执行证据 | mxc 每 backend 能力位 pin by test | ✅ 项目第一原则（AGENTS.md） |

## 3. 参考项目为"生产级"做的核心工作（浓缩）

- **CubeSandbox（腾讯云）**：控制面/数据面分离且无状态（Redis/MySQL）；生命周期状态机 + 分布式空闲回收器（Redis SETNX 认领、未知结果不标记健康）；GC 隔离区（60 次重试后转入 10 min 慢重试，永不静默丢弃）；恢复兼容性校验（cpuid/内核版本不匹配拒绝恢复）；按路由分级的 HTTP 超时预算（30s/120s/240s/1560s）；发布实测基准（41 ms 冷启动 @1 并发，512/s @50）与 Grafana 面板（明确声明 Alerting 不在范围）。
- **E2B**：契约即产品——`Should-Retry` 头（显式 false 也权威）、`Idempotency-Key`、`x-next-token` 分页、`retentionExpired` 标记全部写进 OpenAPI 并生成双 SDK；连接失败仅重试建连类错误（ECONNRESET 排除："服务端可能已收到请求"）；上传强制 HTTP/1.1（h2 会话内存爆窗，两个独立 SDK 都带同一 workaround=生产发现）；envd 仅经代理可达（`x-internal: true` 机器检查）。
- **mxc（微软）**：trait 对齐的多后端（ProcessContainer/LXC/bwrap/Seatbelt/Hyperlight）；有界等待（1→50 ms 退避轮询）、先杀组长再杀 leader、reap 后 kill 是 no-op（PID 复用防御）、daemon 记录 `(pid, pid_creation_time)`；审计日志闭包枚举且"不含路径/命令行/配置值"（日志会附在 bug report 里）；诚实文档：stop/deprovision 失败后的状态被明确标记为"歧义"。
- **microsandbox**：维护扫描（250 ms 预算、250 行/阶段、10 s 租约）；PID 存活验证用 pidfd + 锁文件 inode（PID 复用是头号敌人）；root-last 原子发布（崩溃不暴露半发布检查点）；心跳写专用阻塞线程（防 async 饥饿饿死心跳——"正是这个饥饿曾让宿主杀死忙碌但健康的沙箱"）；指标放 POSIX 共享内存 seqlock，不占 DB 热路径；目录 0700 + symlink 拒绝。

## 4. b-sandbox 本轮补齐的工程（对应上表加粗项）

1. 结构化日志（tracing + EnvFilter）+ 请求级观测中间件（日志一行/请求）。
2. /metrics 扩展：HTTP 计数（method×route-pattern×status）+ 每路由延迟直方图 + exec 延迟直方图 + 4 个 gauge + reaper/孤儿计数；route-pattern 归一化保证基数闭包。
3. SSE 流生命周期收口：沙箱终止/服务关停时确定结束（终止事件先于状态翻转推送，闭合投递竞态），每个流占一个 gauge 位。
4. 优雅停机（SIGTERM/SIGINT → 关停全部沙箱 → SSE 收口 → drain）、启动孤儿回收、单实例 pid 锁（死锁自动抢占）。
5. panic 防护层（单连接降级为 internal 信封 500）+ 8 MiB 请求体上限（Content-Length 前置 413 信封 + 提取器回退同信封）。
6. 基准套件 `tests/bench.rs`（6 项，`#[ignore]` 显式运行）+ `scripts/bench.sh`（真实 release 二进制的 HTTP/RSS 测量）+ 并行吞吐标定门槛。
7. 契约升级：/health 增加版本字段、payload_too_large 错误码、OpenAPI 0.3.0。

修复的真实 bug：`main.rs` 在 tokio runtime 建立前调用 `reaper::spawn`（内部 `tokio::spawn`），二进制启动即 panic——该路径从未被真实执行过，本轮冒烟测试首次覆盖启动路径时发现并修复。

## 5. 已知边界（诚实清单）

- 隔离层级是进程级（Seatbelt/bwrap）；microVM 层按 `docs/m3-microvm.md` 设计就绪，等待 Linux + /dev/kvm 宿主（本机 macOS 无法编译/验证 libkrun 代码，按"能力声明需要执行证据"原则不交付不可验证代码）。
- 重启持久性：沙箱注册表在内存中，服务重启=沙箱消失（孤儿目录被回收）。这是明确语义（E2B 同语义），不是遗漏；跨重启持久化需要先有 microVM 快照层。
- 单租户：单一 Bearer token，无 per-key 限流（CubeAPI 的 governor 限流是后续增强项）。
- 日志外发（NDJSON/OTLP）未实现；部署侧用 stderr 采集（nginx/容器标准做法）。
- 请求体上限 8 MiB（≈6 MiB 文件写入），大文件走分片是调用方责任。

## 6. 检索来源

- [Code Execution | Kubernetes Agent Sandbox SIG](https://agent-sandbox.sigs.k8s.io/docs/use-cases/code-execution/)
- [How to Measure Sandbox Performance for AI-Driven Development (The New Stack)](https://thenewstack.io/how-to-measure-sandbox-performance-for-ai-driven-development/)
- [How to monitor and debug AI-agent sandboxes in production (Northflank)](https://northflank.com/blog/monitor-debug-ai-agent-sandboxes)
- [Remote Code Execution Sandboxes: Architecture, Security, and Trade-Offs (Blaxel)](https://blaxel.ai/blog/remote-code-execution-sandboxes)
- [E2B Sandbox Snapshots](https://docs.e2b.dev/sandbox/snapshots) · [e2b-dev/runtime ARCHITECTURE](https://github.com/e2b-dev/runtime/blob/main/docs/ARCHITECTURE.md)
- 本地克隆（文件级证据见 §3）：`vendor/cubesandbox`、`vendor/e2b`、`vendor/mxc`、`vendor/microsandbox`（溯源见 `vendor/README.md`）
