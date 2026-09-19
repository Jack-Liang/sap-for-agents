# 贡献指南（sap-for-agents）

[English](./CONTRIBUTING.md) | [简体中文](./CONTRIBUTING.zh-CN.md)

感谢关注！本指南覆盖参与本仓库开发的实用要点。

## 前置条件

- **Rust**（stable 工具链；MSRV 见 `Cargo.toml` 的 `rust-version`）
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
- 覆盖全部端点族，含 OpenAPI、MCP（`POST /mcp`）与连接池行为。依赖环境的
  结果（如试用系统的 where-used 索引）按结构断言，不按内容断言。
- 缺少 SAP 连接环境变量时集成测试自动跳过，不会拖挂 CI。

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

见 README §10——bump `Cargo.toml` 版本、打 `vX.Y.Z` tag、推送；CI 自动构建
五个平台的二进制并附到 GitHub Release。
