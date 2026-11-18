# b-sandbox 生产级验证报告

> 本报告的目标是**可验证**：每一条结论都对应一个可以自己跑出来的命令或一份留在仓库里的测试/脚本，不依赖任何"我跑完了"的口头声明。标准依据见 `docs/production-readiness.md`（对 4 个开源沙箱项目的文件级对标）。
>
> 测量环境：macOS 15 (Apple Silicon)，Rust debug/test 构建除注明 release 外均为默认；日期 2026-11-19。数值随机器浮动，门槛（budget）取实测基线的 ~10x，只防数量级回归。

## 1. 结论总表

| 维度 | 结论 | 证据（可复现） |
| --- | --- | --- |
| 质量门 | **23 个测试套件 / 197 个测试全部通过，0 失败** | `./scripts/gate.sh`（fmt --check + clippy -D warnings + test --workspace） |
| 隔离执行证据 | 每个能力位都有翻转测试：路径逃逸拒绝、symlink 逃逸拒绝、输出洪泛截断、fork bomb 受限、网络默认拒绝、egress 凭据注入正向+负向、僵尸回收 | `cargo test -p sandbox-backend-seatbelt`（含 `proxy_mode_egress_goesthrough_the_credential_proxy` 真实 curl 经代理 → 上游看到注入头） |
| 契约锁定 | 路由 ↔ OpenAPI ↔ 错误码三向契约测试通过；spec v0.3.0 | `cargo test -p sandbox-api --test contract` |
| 性能 | API 单周期 0.195 ms（门槛 2 ms）；并行 ~2 700 cycles/s（门槛 300）；Seatbelt echo p50 8.3 ms / p99 19.5 ms（门槛 mean 50 ms） | `cargo test -p sandbox-api --test perf` + `cargo test --workspace --test bench -- --ignored --nocapture` |
| 资源占用 | 服务 RSS **空闲 3.1 MB**，**20 个活跃沙箱 4.5 MB** | `scripts/bench.sh`（release 构建 + 真实 Seatbelt） |
| 运维行为 | SIGTERM → 全部沙箱经单一终止路径回收 → SSE 收口 → drain 退出；启动孤儿回收；单实例锁；panic 降级 500 信封 | `cargo test -p sandbox-api --test api`（sse_stream_ends_after_its_sandbox_terminates / sse_stream_ends_when_the_server_drains / handler_panic_returns_the_internal_envelope / oversized_body_is_a_413_envelope 等）+ 本报告 §5 的二进制冒烟记录 |
| 观测完整性 | 401/404/413/panic 全部计入 http_requests_total；直方图 + 4 个 gauge + reaper/孤儿计数；基数被路由表封顶 | `http_metrics_follow_the_full_traffic_picture` 测试 |

## 2. 质量门明细

命令：`./scripts/gate.sh`（= `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`）

最近一次全绿运行：**exit 0，23 个 `test result: ok` 行，197 passed / 0 failed**。套件构成：

| crate | 套件 | 覆盖 |
| --- | --- | --- |
| sandbox-core | lib（policy/lifecycle/budget/events/backend） | 策略归一化、状态机转换表、能力位、事件游标 |
| sandbox-backend-seatbelt | lib + integration + perf | profile 生成、FFI、真实进程执行证据、延迟门槛 |
| sandbox-backend-bwrap | lib + integration（Linux 门控） | argv 生成、宿主无 bwrap 时跳过 |
| sandbox-api | lib + api + contract + perf + bench(ignored) | 路由/账本/reaper/metrics/鉴权/模板/SSE/停机 |
| sandbox-egress-proxy | lib + proxy tests | 注入/透传/CONNECT/400 |
| sandbox-sdk | lib + live | 真实 TCP 全生命周期 + 错误信封映射 |

## 3. 安全对抗测试（隔离证据）

`cargo test -p sandbox-backend-seatbelt --test integration`（真实进程，非模拟）：

- `denied_subtree_still_blocks` / fs 读写策略翻转 → 沙箱内行为变化
- 路径遍历（`../`）与 symlink 逃逸 → 400 policy_validation（`file_paths_reject_traversal_attacks`、`symlink_escape_is_denied`）
- 输出洪泛 → 截断标志 + 字节帽（truncated 计数）
- fork bomb 受 pids 预算限制
- 网络默认拒绝 + net.proxy 单孔模式：正向（curl 经代理 → 上游收到 `Authorization: Bearer tok-123`）与负向（NO_PROXY 直连 → 非零退出、上游未见任何请求）
- egress 代理：配置主机注入、非配置主机透传不丢头、CONNECT 隧道、畸形目标 400
- **egress 目的地策略**（`deny_private_blocks_connect_to_loopback` / `deny_private_blocks_absolute_form_to_loopback` / `deny_private_blocks_names_resolving_to_private_addresses`）：私网/环回/链路本地/CGNAT 目标在 CONNECT 与转发两条路径都被 403 拒绝；解析到被禁地址的 DNS 名整体拒绝；策略单元测试覆盖每个地址段（含 100.64/10、198.18/15 这类 std `is_private` 不覆盖的段）；AllowAll 显式选择后内网目标可达（部署自有内部服务用）

## 4. 性能实测

### 4.1 标定门槛（gated，防回归）

| 门槛 | 实测基线 | 预算 | 文件 |
| --- | --- | --- | --- |
| API create+exec+detail+delete 单周期 | 0.195 ms | < 2 ms | `sandbox-api/tests/perf.rs` |
| API 并行生命周期吞吐（16 worker） | ~2 700 cycles/s (32 worker) | > 300 cycles/s | 同上 |
| Seatbelt echo 执行均值 | 5.2 ms | < 50 ms | `sandbox-backend-seatbelt/tests/perf.rs` |

### 4.2 基准数字（非门槛，`cargo test -p sandbox-api --test bench -- --ignored --nocapture`）

| 项 | 数值（debug 构建） |
| --- | --- |
| API 生命周期周期（顺序） | 880.7 µs/cycle（本次运行，机器有并发负载时在 0.2–0.9 ms 波动） |
| API 生命周期周期（32 并发） | 2 792 cycles/s |
| /metrics 抓取 | 98.7 µs/次，8.3 KB body |
| 文件数据面（1 KiB） | write 613.9 µs/op，read 330.6 µs/op |
| Seatbelt echo 直方图（30 次） | p50 8.3 ms · p90 18.4 ms · p99 19.5 ms |
| Seatbelt 并发执行（8 并发） | 119 exec/s |

The New Stack 的结论（命令执行延迟而非启动时间是 agent 场景的核心指标）直接对应这里的 exec 直方图与 ledger 计时：两者都在 `/metrics` 与 detail 视图可见。

### 4.3 真实二进制（`scripts/bench.sh`，release 构建 + 真实 Seatbelt 隔离）

| 指标 | 数值 |
| --- | --- |
| RSS 空闲 | **3 152 KB（3.1 MB）** |
| RSS 携 20 个活跃沙箱 | **4 528 KB（4.4 MB）**（边际 ~69 KB/沙箱，进程级后端） |
| /health 延迟 | 0.44 ms |
| create（20 次均值） | 0.8 ms |
| exec /bin/echo（5 次均值） | 8.8 ms |
| 文件写 1 KiB | 1.5 ms |
| /metrics 抓取 | 0.33 ms，9 444 B body |
| DELETE（终止+回收） | 0.6 ms |

对照组：CubeSandbox 公布的 per-VM 驻留开销 18.7–24.6 MiB（microVM 层）；b-sandbox 进程级服务整体 <5 MB 含 20 个沙箱——量纲不同，仅供感知资源纪律，不构成同层对比。

## 5. 运维行为验证（真实二进制冒烟记录）

2026-11-19，`./target/debug/sandbox-api --data-root <tmp> --port 18094`：

```
INFO b-sandbox starting backend="seatbelt" version="0.1.0" ...
INFO listening bind=127.0.0.1 port=18094
（curl /health → 200；POST /sandboxes → 201；GET /metrics → 计数=1）
kill -TERM <pid>
INFO shutdown signal received; terminating sandboxes and draining
INFO all live sandboxes terminated count=1
INFO server drained; exiting      ← 进程退出（graceful drain 完成）
```

对应单元/集成证据：孤儿回收 `recover_orphans_removes_sandbox_dirs_but_not_templates`、服务锁三连测 `service_lock_*`、SSE 生命周期收口两条 `sse_stream_ends_*`、panic 降级与 413 信封两条、观测完整性 `http_metrics_follow_the_full_traffic_picture`。

## 6. 本轮发现并修复的真实缺陷

| 缺陷 | 级别 | 发现方式 | 修复 |
| --- | --- | --- | --- |
| `main.rs` 在 tokio runtime 建立前调用 `reaper::spawn`（内部 `tokio::spawn`）→ 二进制启动必 panic | P0 | 首次对启动路径做真实二进制冒烟 | runtime 建立后再 spawn（commit 1f2ff3d） |
| SSE 终止事件投递竞态：tail 先查终止态再冲刷事件 → 客户端可能永远收不到 terminated | P1 | 评审自身设计（drain-then-check 需要事件先于状态翻转） | `remove_sandbox` 先推事件后翻状态 + tail 先冲刷后检查（commit 3661a48） |
| 观测中间件在 auth 内层 → 401 不可见 | P1 | Northflank 四层遥测标准对标 | 层序调整：observe 最外（commit 3661a48） |
| 超限请求体返回 axum 纯文本 413，破坏错误信封契约 | P1 | E2B"契约即产品"对标 | 8 MiB 上限 + `payload_too_large` 信封（commit daa7c67） |
| `/var/folders` vs `/private/var/folders`：bench 后端 rootfs 未 canonicalize → 文件面全部 400 | P2（测试） | bench 首跑 | canonicalize（tests/bench.rs） |
| scripts/bench.sh 末尾 `wait` 在 trap 发 SIGTERM 前执行 → 死等 | P2（脚本） | 首跑挂起 | 删除该行（scripts/bench.sh） |
| egress 代理目的地不设限：net.proxy 沙箱可借 CONNECT 隧道打宿主机 loopback 服务/169.254.169.254/RFC1918（代理进程不受沙箱约束） | **P0（安全）** | 对标 microsandbox 网络隔离文档自查 | `EgressPolicy::DenyPrivate` 默认 + DNS 解析校验 + 403/502 可观测拒绝（commit 93d5358） |

## 7. 复现清单

```bash
# 全量质量门（含契约/性能/对抗/运维测试）
./scripts/gate.sh

# 性能基准（打印数字，不断言）
cargo test -p sandbox-api --test bench -- --ignored --nocapture

# 真实二进制资源/延迟表（release 构建约 3-5 分钟）
scripts/bench.sh

# 单独跑安全对抗证据
cargo test -p sandbox-backend-seatbelt --test integration -- --nocapture

# SDK 全生命周期（真实 TCP）
cargo test -p sandbox-sdk --test live
```

## 8. 与开源参考项目的定位对比

| | b-sandbox | E2B | CubeSandbox | microsandbox |
| --- | --- | --- | --- | --- |
| 隔离层 | 进程（Seatbelt/bwrap），microVM 设计就绪 | Firecracker microVM | microVM（KVM）+ 容器 | microVM（libkrun） |
| 部署形态 | 单二进制，单机单实例 | 云服务（编排面私有） | K8s 多节点控制面 | 自托管单机 |
| 指标 | Prometheus 文本（手写，零依赖） | API 资源 + 云侧 | Prometheus 双 exporter | OTLP（共享内存采集） |
| 冷启动 | 进程 spawn 5-8 ms | 快照恢复（毫秒级） | 41 ms 实测 | <100 ms |
| 适用 | 同机自托管、低密度、强审计 | 托管多租户 | 集群多租户 | 单机高密度 |

## 9. 遗留边界（不是达标项，是诚实清单）

见 `docs/production-readiness.md` §5：进程级隔离边界、重启不持久（孤儿回收语义）、单租户 token、日志外发由部署侧采集、8 MiB 请求体上限、microVM 层待 Linux 宿主。
