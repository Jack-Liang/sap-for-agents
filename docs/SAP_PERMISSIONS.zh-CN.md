# SAP 权限配置（L1：网关账号最小权限）

[English](./SAP_PERMISSIONS.md) | [简体中文](./SAP_PERMISSIONS.zh-CN.md)

## 为什么

sap-for-agents 网关用 `.env` 里的 `SAP_USER` 连 SAP。**网关能调什么 = 这个账号有什么权限**。

默认用 `DEVELOPER`（开发账号，权限很宽）= 任何能访问网关的人都能调 `DEVELOPER` 能调的函数（读主数据、改订单、建用户…）。

**正确做法**：建一个**专用受限账号**，只授业务需要的 Function Group 权限。这样即使网关被滥用或 token 泄露，影响面 = 该账号权限（最小）。

> 这比在网关层维护函数白名单更有效——SAP 的权限系统（PFCG / `S_RFC`）就是为此设计，工具齐全且更完善。

## 配置步骤（Basis 操作）

### 1. 建专用服务账号（SU01）

- 事务码 `SU01`，新建用户（如 `SAP_RFC_GW`）
- 用户类型：**System**（系统用户，不能交互登录，专供 RFC）
- 初始密码：设强密码（记下，填进 `.env`）

### 2. 创建角色（PFCG）

- 事务码 `PFCG`，新建角色（如 `Z_RFC_GW_READONLY`）
- 在「权限」标签页加权限对象 **`S_RFC`**：
  - `ACTVT` = `16`（执行 RFC）
  - `RFC_TYPE` = `FUNC`（Function Group）
  - `RFC_NAME` = <允许的 function group，如 `BAPI_USER_BANK`、`SRFC`、自开发的 `Z*` 组>
  - **不要用 `*`**（= 全授权，等同 DEVELOPER）

> `RFC_NAME` 可多值。按业务**最小授权**：只列实际要调的 function group。
>
> 常见只读组（按需）：`SZRP`（RFC 元数据，网关元数据端点需要）、`SUPI`/`SUSR`（用户相关 BAPI）等。

### 3. 给账号分配角色

- `SU01` → 给 `SAP_RFC_GW` 分配 `Z_RFC_GW_READONLY` 角色
- `PFCG` → 角色生成 profile

### 4. 改 `.env`

```env
SAP_USER=SAP_RFC_GW
SAP_PASSWD=<新账号密码>
```

重启网关生效。

### 5. 验证

- 调**有授权**的函数（如 `BAPI_USER_GETLIST`，若在授权组）→ `200`
- 调**未授权**的函数（如某财务 BAPI，不在授权组）→ `403 {"error":{"code":403,"key":"RFC_AUTHORIZATION_FAILURE"}}`

未授权 → `403`，说明权限边界生效（白名单的活，SAP 替你干了）。

## 为什么不在网关层做白名单

- SAP 有上万函数，单函数白名单维护爆炸 + 永远漏
- SAP 权限系统（`S_RFC`）按 **function group** 聚合（几十条规则 vs 万个函数），粒度和"业务能力"对齐
- 网关层白名单是可选的"第二道闸"（L2，按 group 前缀过滤），L1（SAP 权限）是**第一道且最有效**

> 注：当前版本未实现网关层 L2 过滤，权限边界完全由 SAP 侧授权承担。
