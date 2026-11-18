# b-sandbox

面向 AI Agent 工作负载的沙箱服务：把不可信的 shell 命令、生成代码、文件操作放进隔离环境，通过 HTTP API 使用。每个工作负载都有独立的隔离环境、声明的资源预算和显式的网络/文件系统策略——任何 Agent 提交的东西都不会裸跑在宿主机上。

为 [b-superagent](../b-superagent) 服务的组件；隔离模型与 API 形态对齐 E2B 事实标准（详见 `docs/production-readiness.md` 的对标分析）。

## 核心能力

- **策略即数据**：文件系统/网络/exec 权限是声明式策略，附在工作负载上；执行落在每个后端的一处，绝不散落调用点。未知字段、缺失策略、歧义路径一律拒绝（fail closed）。
- **后端是接缝**：生命周期、策略、API、可观测全部与后端无关。进程级（macOS Seatbelt / Linux Bubblewrap）今天可用；microVM（libkrun）按 `docs/m3-microvm.md` 设计就绪，等 Linux + `/dev/kvm` 宿主。
- **能力位必须带执行证据**：后端声明的每个能力位，都有翻转策略输入并观测到工作负载行为变化的测试。没有证据就没有能力位。
- **可观测**：Prometheus `/metrics`（计数/直方图/gauge，路由模式归一化保证基数闭包）、结构化日志（tracing）、每沙箱 usage 账本、事件日志 + SSE 实时流（Last-Event-ID 重连）。
- **生产运维**：优雅停机（SIGTERM → 关停沙箱 → drain）、启动孤儿回收、单实例 pid 锁、panic 防护层、请求体上限、Bearer 认证（常量时间比较）、幂等创建、空闲 TTL 回收、凭据注入 egress 代理（令牌永不进入沙箱）。
- **性能有预算**：`tests/perf.rs` 的标定门槛 + `tests/bench.rs` 基准 + `scripts/bench.sh` 真实二进制测量（详见 `docs/production-report.md`）。

## 快速开始

```bash
# 质量门（fmt + clippy -D warnings + 全部测试，~190 个）
./scripts/gate.sh

# 启动（macOS：Seatbelt 后端）
cargo run -p sandbox-api -- --data-root /tmp/b-sandbox --port 8094

curl -s localhost:8094/health
curl -s -X POST localhost:8094/sandboxes -H 'content-type: application/json' -d '{}'
curl -s -X POST localhost:8094/sandboxes/<id>/exec \
  -H 'content-type: application/json' -d '{"command":"/bin/echo","args":["hi"]}'

# 生产参数（令牌走环境变量，argv 在 ps 里可见）
B_SANDBOX_AUTH_TOKEN=$(openssl rand -hex 32)
cargo run -p sandbox-api -- --data-root /data/b-sandbox --backend bwrap \
  --bind 0.0.0.0 --egress-proxy 127.0.0.1:8095 --egress-credentials /etc/b-sandbox/egress.json
```

## 仓库结构

见 [AGENTS.md](AGENTS.md) 的结构表。API 契约在 [docs/openapi.yaml](docs/openapi.yaml)（OpenAPI 3.0.3，`tests/contract.rs` 三向锁定路由↔契约↔错误码），Rust SDK 在 `crates/sandbox-sdk/`。

## 文档

- [docs/design.md](docs/design.md) — 架构设计（中文）
- [docs/production-readiness.md](docs/production-readiness.md) — 生产级标准与 4 个开源沙箱项目的对标分析（中文）
- [docs/production-report.md](docs/production-report.md) — 验证报告：实测数据与证据（中文）
- [docs/m3-microvm.md](docs/m3-microvm.md) — microVM 后端设计（英文）
- [vendor/README.md](vendor/README.md) — 参考项目溯源

## 部署

`scripts/deploy.sh`：交叉编译（musl/zig）→ scp → 服务器 docker build → compose up → 健康检查。容器只绑 `127.0.0.1`、只读 rootfs、`cap_drop: ALL`；密钥只走环境变量/文件，不进镜像与仓库。

## License

Apache-2.0
