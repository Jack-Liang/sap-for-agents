# ADT Debugger REST 契约（P0 实测版）

来源：[oisee/vibing-steampunk](https://github.com/oisee/vibing-steampunk)（MIT）源码契约 + 2026-11-18 在 A4H（ABAP Platform 816，client 001，user DEVELOPER）**纯 HTTPS 全链路实测**。本文件是 P1/P2 的实现依据；每条标注 ✅=A4H 实测，📜=vsp 源码契约（未单独复测）。

## 0. 最重要的发现：`X-sap-adt-sessiontype: stateful`

✅ **每个调试器请求都必须带 `X-sap-adt-sessiontype: stateful` 头**。没有它，ICF 把请求按无状态处理——请求结束即回收上下文（roll area 没了），LISTEN 建立的调试器会话活不到 STACK，后续所有调用报 `noSessionAttached`。这就是 Eclipse 能走纯 HTTPS、无状态客户端"看起来不可能"的全部原因。

✅ 次级推论：**CSRF fetch 也必须带该头、在同一个会话上做**（vsp http.go:547：无 stateful 标记的 fetch "is answered on a different ADT context and the stateful one is retired"）。

✅ 调试器会话选定方式 = `sap-contextid` cookie + stateful 头，跨 LISTEN→ATTACH→STACK→步进全程不变；`set-cookie` 更新要回写 cookie jar。

## 1. 通用规则

- ✅ `Accept: */*` 恒用：资源/媒体类型不匹配在 ADT 读作 **404 "No suitable resource found"** 而非 406——写死媒体类型会把"版本差异"伪装成"资源不存在"（📜 vsp 7.50 教训）。
- ✅ 错误契约：非 2xx → `exc:exception` XML，`properties/entry[@key='...subType']` 携带机器语义（`noSessionAttached` / `invalidDebuggee` / `debuggeeEnded`），`message` 携带人话。**500 + subType=debuggeeEnded 是 continue 的正常结束形态**（不是故障）。
- ✅ 一个有状态会话同时只服务一个请求；其余排队。
- 📜 调试器会话是 vsp/网关侧最长寿的对象：cookie/凭据失效要能在会话中途重认证（SSO 场景过期应答是 200+登录页，不是 401）。
- ⚠️ **网关现有 `/api/adt/**` 透明代理跑不了调试**（实测）：GET 不带 cookie（`adt.rs` cookie 只随写方法的 CSRF 附加）、代理不回写 `set-cookie`、无 stateful 头——三缺口即 P1 `DebugSession` 的需求清单。

## 2. 断点 `POST/GET /sap/bc/adt/debugger/breakpoints`

✅ **POST = 整集合替换**（per `requestUser+ideId+terminalId+scope`），加/删一个都是"GET 当前集合→改→POST 全量"。
✅ **GET 常 回 200+空体**（两次实测）——断点集合的真实记录在客户端（网关记账本），GET 只是"万一版本肯答"的复核。

请求体（✅ A4H 验证）：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<dbg:breakpoints xmlns:dbg="http://www.sap.com/adt/debugger"
    xmlns:adtcore="http://www.sap.com/adt/core"
    debuggingMode="user" scope="external"
    requestUser="DEVELOPER" terminalId="<32位稳定hex>" ideId="sfa">
  <breakpoint kind="line" enabled="true"
      adtcore:uri="/sap/bc/adt/oo/classes/ZFOO/source/main#start=42"/>
  <!-- 可选 condition="ABAP逻辑表达式"；kind 另有 exception(exceptionClass)/statement(statement)/message(msgId,msgNo,msgTy) -->
  <!-- 根属性可加 systemDebugging="true"：不开时标准码断点【被接受但永不触发】(📜 A4H 曾实测) -->
</dbg:breakpoints>
```

响应（✅）：`<dbg:breakpoints><breakpoint kind id uri adtcore:name …/></dbg:breakpoints>`；**放不下的行回 `errorMessage` 且无 id**（声明行/注释行无语句——批量投放后"要的 60 个、粘住的 6 个"是正常结果，拒绝项必须显式上报）。
✅ 断点 id 形如 `KIND=0.SOURCETYPE=ABAP.MAIN_PROGRAM=…CP.INCLUDE=…CM002.LINE_NR=3`——**LINE_NR 是 include 内行号**，与 `#start=源码行` 不同（class 方法 include 第 1 行是 `METHOD.`）。

- 📜 `ideId`（如 `sfa`）+ `terminalId`（32 字符，**稳定**——断点按它注册，换了就看不见自己的断点）。
- 📜 条件校验：`POST /breakpoints/conditions`，`<dbg:condition>…</dbg:condition>` → `valid/message` 属性。

## 3. Listener `POST /sap/bc/adt/debugger/listeners?debuggingMode=user&requestUser=U&ideId=I&terminalId=T&timeout=N`

✅ **阻塞式**：debuggee 停下即返回，等满 `timeout` 无人则 **200+空体**。
✅ 响应：`asx:abap/values/DATA/STPDA_DEBUGGEE`（数组）：`DEBUGGEE_ID`、`DEBUGGEE_USER`、`PRG_CURR`、`INCL_CURR`、`LINE_CURR`、`URI`（含 `#start=行`）、`DBGEE_KIND`、`NAME`、`TYPE`、`IS_ATTACH_IMPOSSIBLE`。
✅ `requestUser` 必须非空（空 = 永远等满超时且与"断点没触发"无法区分）。
✅ **debuggee 的等待窗口 ≈ listener 的 timeout**（实测 30s：无 debugger 附加的 debuggee 等 30s 后自行放行，触发方 `duration_ms=30144`）。
✅ 外部断点**没有 listener 等待时直接跳过、不挂死**（实测触发方正常完成）。
✅ 注册按 **user 单键**（ABDBG_LISTENER：IDE_ID=user、TERMINAL_ID='%_USER'）——残留行会静默吞掉下一个 debuggee；从**任意另一条连接** `DELETE /listeners?debuggingMode=user&requestUser=U` 即可摘除（可用于取消卡住的 listen）。
✅ **DELETE listener = 结束该用户的外部调试 + 连带删除该用户全部外部断点**（同一动作，SAP 语义非选项；两次实测断点被清空）。→ 网关清场就是这一刀；会话收尾先 detach debuggee 再 DELETE listener。

## 4. Attach `POST /sap/bc/adt/debugger?method=attach&debuggeeId=X&debuggingMode=user&requestUser=U&dynproDebugging=true`

✅ 200 → `<dbg:attach … debugSessionId processId isRfc="false" isSameSystem reachedBreakpoints>…`。
✅ **竞态窗口真实存在**：debuggee 只在等待期可 attach（超窗/抢丢 → `invalidDebuggee`）——listen→attach→stack 必须背靠背（vsp 的 ADTCatch 语义，网关后台任务一次做完）。

## 5. 栈

- ✅ 资源形态（816 可用）：`GET /sap/bc/adt/debugger/stack?method=getStack&emode=_&semanticURIs=true`。
- 📜 7.50 无该资源（404）→ 回落 `POST /sap/bc/adt/debugger?method=getStack&emode=_&semanticURIs=true`；两种形态文档同构。网关实现：先资源形态，404 一次即记忆回落（会后固定）。
- ✅ 响应 `<stack debugCursorStackIndex stackEntry…>`：每帧 `stackPosition/programName/includeName/line/eventType/eventName/uri`。
- 📜 帧切换：`PUT <帧的 uri>`（形如 `/sap/bc/adt/debugger/stack/type/ABAP/position/3`）。

## 6. 变量

- ✅ 枚举层次：`@ROOT → @LOCALS`（816）；📜 版本差异：有的版本是 `@GLOBALS`+`@PARAMETERS`——**先问 @ROOT 的孩子，读到什么根用什么根**（网关不得写死）。
- ✅ 展开：`POST /sap/bc/adt/debugger?method=getChildVariables`，Content-Type `…dataname=com.sap.adt.debugger.ChildVariables`，body：
  `<asx:abap …><asx:values><DATA><HIERARCHIES><STPDA_ADT_VARIABLE_HIERARCHY><PARENT_ID>@ROOT</PARENT_ID></STPDA_ADT_VARIABLE_HIERARCHY></HIERARCHIES></DATA></asx:values></asx:abap>`
  响应直接给 `<CHILD_ID>@LOCALS</CHILD_ID><CHILD_NAME>Locals</CHILD_NAME>`；再对 `@LOCALS` 展开得到变量数组：`ID/NAME/DECLARED_TYPE_NAME/ACTUAL_TYPE_NAME/KIND/VALUE/LENGTH/TABLE_LINES/IS_VALUE_INCOMPLETE…`（✅ LV_X 读回 `1`）。
- 📜 直读：`POST ?method=getVariables`（dataname `…debugger.Variables`，body `<STPDA_ADT_VARIABLE><ID>名字</ID>…`）——✅ 对具名变量可用（改值后读回）；@ROOT 直读回空体（枚举靠 childVariables）。
- ✅ **写值**：`POST ?method=setVariableValue&variableName=LV_X`，body = 裸值串（实测发 `99`，步过后读回 `100`——写入真实生效）。

## 7. 步进 `POST /sap/bc/adt/debugger?method=stepInto|stepOver|stepReturn|stepContinue`

✅ 200（步过有响应文档）；**continue 后 debuggee 结束 → 500 + `subType=debuggeeEnded`（= 正常结束）**；未附加 → `noSessionAttached`。

## 8. Detach `POST /sap/bc/adt/debugger?method=detach`

📜 A4H 可能回 400 "Unknown method" → 回落 `stepContinue`：答 `debuggeeEnded`/`noSessionAttached` = 已干净。
✅ HTTPS 路线实测：debuggee 在无 detach 时**随 DELETE listener 一起放行**（classrun 正常完成、输出无缺）。
⚠️ 📜 HTTPS 路线不 detach 就退出的后果：debuggee 悬挂在工作进程直到触发方自己的超时——网关关停顺序必须是"detach → DELETE listener"。

## 9. 实测闭环记录（2026-11-18 A4H）

```
设断点(class 源码行12→include CM002 行3) → abap/run 触发(新会话,同用户)
→ listener 2s 内返回 debuggee → attach 200 → stack 200(帧0=断点行)
→ @ROOT→@LOCALS→LV_X=1 → setVariableValue 99 → stepOver → 读回 LV_X=100
→ stepContinue → debuggeeEnded → 触发方 abap/run 完成
→ DELETE listener → 断点连带清空（GET 200 空体）
```

- RFC 触发（`/api/rfc` 直调 FM）路径未测——HTTP(classrun) 触发已证可用，RFC 触发为**残余未知**（不阻塞实现；文档先写"触发用 /api/abap/run 或 /api/rfc，后者未在所有版本验证"）。
- 调试器与 debuggee **必须分属两个会话**（实测：同会话时 classrun 请求占住上下文，调试器操作全部落空）——网关的 `abap/run` 每次强制新会话，正好是天然触发通道。
