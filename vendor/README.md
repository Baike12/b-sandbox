# vendor/ — 参考项目（只读）

本目录存放开源沙箱参考项目，全部为浅克隆（`--depth 1`），仅作设计阅读用：

- **不参与构建**：不被任何 crate 引用、编译或链接；不复制其代码进产品（如需借鉴需先核对上游 License）。
- **不进 Git**：`vendor/*` 在 `.gitignore` 中；本表即溯源记录，重新拉取以表中 upstream + commit 为准。

| 目录 | 上游 | 钉住 commit | 拉取日期 | 定位 |
| --- | --- | --- | --- | --- |
| `cubesandbox/` | https://github.com/TencentCloud/cubesandbox | `b5c96b9` (2026-11-13) | 2026-11-15 | 腾讯云面向 AI Agent 的沙箱服务，E2B 兼容 API，硬件级隔离 + 高密度部署，工程化最完整 |
| `mxc/` | https://github.com/microsoft/mxc | `c66201` (2026-11-14) | 2026-11-15 | Microsoft eXecution Container：统一隔离模型 + 多后端（ProcessContainer/LXC/Bubblewrap/Seatbelt/MicroVM 等），Rust/.NET/Node SDK |
| `e2b/` | https://github.com/e2b-dev/e2b | `6dc8d5a` (2026-11-13) | 2026-11-15 | AI Agent 沙箱事实标准：Firecracker microVM 云运行时 + 多语言 SDK，API 形态的业界参照 |
| `microsandbox/` | https://github.com/microsandbox/microsandbox | `28bdced` (2026-11-12) | 2026-11-15 | Rust + microVM（libkrun）的自托管沙箱，与本仓库技术栈最接近的实现参考 |
| `awesome-sandbox/` | https://github.com/restyler/awesome-sandbox | `35385f4` (2026-08-12) | 2026-11-15 | AI 代码沙箱方案清单，用于持续发现新方案 |

刷新方式：`cd vendor/<name> && git fetch --depth 1 origin && git reset --hard origin/HEAD`（或删除目录后按上表重新浅克隆）。
