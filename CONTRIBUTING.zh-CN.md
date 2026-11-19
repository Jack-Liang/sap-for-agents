# 贡献指南（sap-for-agents）

[English](./CONTRIBUTING.md) | [简体中文](./CONTRIBUTING.zh-CN.md)

感谢关注！本指南覆盖参与本仓库开发的实用要点。

## 前置条件

- **Rust**——`rust-toolchain.toml` 锁定确切工具链版本，rustup 自动选用。升级是
  主动动作：改 `channel` 一行 → `rustup update` → 本地全量
  `cargo clippy --all-targets -- -D warnings` 过一遍 → 再推。（浮动 stable 曾让
  CI 侧新增的 clippy lint 本地不可见，推送即红。）MSRV 见 `Cargo.toml` 的
  `rust-version`。
- **SAP NWRFC SDK**——SAP 专有 C 库，从 SAP Support Portal 下载（受版权限制
  不能随仓库分发）。把对应平台的 zip 放进 `nwrfcsdk/lib/<任意目录>/`，
  `./start.sh` 会自动解压；详见 README §Quick Start。
- 可选：**本地 SAP 试用系统**（如 ABAP Cloud Developer Trial docker 镜像），
  用于跑集成测试。

## 构建与测试

```bash
# 编译 + 单元测试（不需要 SAP——CI 跑的就是这套）
cargo test

# 配置好真实 SAP（.env）后，再跑集成测试套件
#（tests/*.rs 标了 #[ignore]；每个测试会拉起真实服务进程）：
DYLD_LIBRARY_PATH=./nwrfcsdk/lib/darwin-aarch64 cargo test -- --ignored   # macOS 示例
# Linux: LD_LIBRARY_PATH=./nwrfcsdk/lib/linux-x86_64

# 质量门禁（CI 以 -D warnings 运行）
cargo clippy --all-targets -- -D warnings
```

集成测试说明：

- 每个测试在独立端口拉起真实 HTTP 服务，通过 HTTP 端到端验证。
- 覆盖全部端点族，含 OpenAPI、MCP（`POST /mcp`）、连接池行为与远程 ABAP
  调试面。依赖环境的结果（如试用系统的 where-used 索引）按结构断言，
  不按内容断言。
- 缺少 SAP 连接环境变量时集成测试自动跳过，不会拖挂 CI。
- **试用系统上请分批串行跑**（`--test-threads=1`、按前缀分组、批间静默
  一分钟）：马拉松式连跑会让写入量压垮试用系统 ICM——write-heavy 用例
  集体报 ICM "Service cannot be reached" 页。单个失败用例单独重跑转绿，
  即说明是基础设施而非你的代码。
- **容器/系统重启后有 ~30 分钟稳态窗口**：RFC 探针（`/ready`、discovery）
  几乎立即转绿，但走 ADT lock 的写入在该窗口内报 ICM 400
  "Service cannot be reached"——不要误判为代码回归。完整运行策略见
  `tests/sap_integration.rs` 头注释。

## 设计约定

- **务实手写优先于框架魔法**：OpenAPI 规范是数据驱动 JSON 字面量
  （`src/openapi.rs`），MCP 服务器是手写 JSON-RPC 子集（`src/mcp.rs`）——
  不引 utoipa/rmcp。除非有充分理由，请保持同一风格。
- **文档随功能走**：README.md / README.zh-CN.md / AGENTS.md / AGENTS.zh-CN.md /
  首页（`src/index*.html`）在端点或行为变更时必须双语同步。
- **缺口可见**：子结果取不到时（动态规范里某个 BAPI 的元数据、源码前言里
  某个依赖），就地暴露错误，不静默丢弃。
- **测试把守发布**：新端点要配集成测试；纯逻辑要配单测。提交前本地
  `cargo test` 全绿（SAP 可达时含 `--ignored`）。

## 提交

1. 从 `main` 拉 fork / 分支。
2. `cargo test && cargo clippy --all-targets -- -D warnings` 必须通过。
3. PR 附清晰说明；涉及用户可见行为时包含中英双语文档更新。

## 发版（维护者）

见 README §10——bump `Cargo.toml` 版本、提交（`chore(release): vX.Y.Z`）、打
`vX.Y.Z` tag、推送；CI 自动构建五个平台的二进制并附到 GitHub Release。
流水线坑位：

- Release workflow 有 **tag == `Cargo.toml` 版本** 一致性闸门（只打 tag 不
  bump 会挂在检查步）。
- 观察用 `gh run watch <id> --exit-status`——管道接它的输出会吃掉退出码。
- tag 打错：`gh release delete vX.Y.Z --cleanup-tag` 清掉后修正重打（重新
  推送同名 tag 会重触发 workflow）。
- 发布后用 `gh release edit vX.Y.Z --notes-file notes.md` 附面向用户的
  release notes。
