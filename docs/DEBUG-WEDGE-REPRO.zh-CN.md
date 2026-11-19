# Bug 移交:客户端打断 bp 操作 → bp_chan 被长期占住(3/3 复现,机制未解)

状态:**✅ 已定位并修复**(2026-11-20 00:30 记录 / 同晚结案,见 §8)|移交对象:debug 面开发会话|相关:`src/debug.rs`(`with_bp_chan!` / `bp_channel`)、`src/debug_session.rs`

## 0. 一句话

用 curl 在 2s 时打断一个 `POST /api/debug/breakpoints`,之后**该系统所有断点操作(bp_list/bp_set)挂 30s+ 甚至数分钟**,远超任何单请求 15s 上限的组合——怀疑 `Slot.bp_chan`(tokio `OwnedMutexGuard<Option<DebugSession>>`)被一个永不完成的持有者占住。

## 1. 复现步骤(3/3 稳定)

前置条件(**缺一不可**,见 §5 干扰警告):

```bash
# ① 网关刚重启(SLOTS 空)、无活动调试会话
curl http://127.0.0.1:3000/api/s/a4h/debug/sessions        # 无活动会话
# ② SAP ICM 健康(直连 <200ms)
curl -s -m 5 -o /dev/null -w "%{http_code} %{time_total}s\n" \
  -u DEVELOPER:'<pwd>' "http://localhost:50000/sap/bc/adt/discovery" -H "Accept: application/atomsvc+xml"
# ③ 单一驱动者:确保没有其他会话/agent 在同时打网关(今晚的噪音源)
```

```bash
# ④ 打断实验:2s 时客户端断连
curl -s -m 2 -X POST http://127.0.0.1:3000/api/s/a4h/debug/breakpoints \
  -H "Content-Type: application/json" \
  -d '{"targets":[{"kind":"class","name":"ZCL_SFA_DBG_T","line":11}]}'
# curl 退出码 28(超时打断)

# ⑤ 立即探测:应快速返回,实测挂 25s/30s+/数分钟(000 = curl 侧超时)
curl -s -m 30 -o /dev/null -w "%{http_code} %{time_total}s\n" \
  http://127.0.0.1:3000/api/s/a4h/debug/breakpoints

# ⑥ 清场:重启网关即恢复(bp_list 立回 94ms)
```

测试类(若已删,先建):

```bash
curl -X POST http://127.0.0.1:3000/api/s/a4h/objects/class/ZCL_SFA_DBG_T/create \
  -H "Content-Type: application/json" -d '{"description":"wedge repro",
  "source":"CLASS zcl_sfa_dbg_t DEFINITION PUBLIC FINAL CREATE PUBLIC.\n PUBLIC SECTION.\n  INTERFACES if_oo_adt_classrun.\nENDCLASS.\nCLASS zcl_sfa_dbg_t IMPLEMENTATION.\n  METHOD if_oo_adt_classrun~main.\n    out->write( |ok| ).\n  ENDMETHOD.\nENDCLASS."}'
```

## 2. 今晚的三次复现(2026-11-20 00:15–00:24)

| # | 打断时刻 | 后续 bp_list | 进程 | 备注 |
|---|---|---|---|---|
| 1 | ~00:15:20(2s 打断) | 挂 >20s(curl -m 20 截断) | 00:16:15 死亡 | 死亡判为并发会话重启(见 §5),非实验 |
| 2 | 00:19:08(2s 打断) | 挂 >25s(截断) | 00:19:46 重生 | 同上,死亡非实验 |
| 3 | 00:21:33(2s 打断) | 挂 >30s,**数分钟后仍挂** | **存活**(pid 39649) | 楔死与进程死亡无关,实锤 |

重启后 bp_list 立即恢复 94ms——楔死只在网关内存态(SLOTS/bp_chan),重启即清。

## 3. 已排除(黑洞对照实验,2026-11-20 00:04–00:11)

- **不是"无超时"**:`adt.rs` client builder 有全局 `.timeout(60s)`(经 `build_adt` 贯穿所有 ADT 请求);且本次已加 `DEBUG_REQ_TIMEOUT=15s` 兜底(`debug_session.rs` send_once,显式值优先:listen=listen_secs+20、teardown=10s)。黑洞端点(accept 永不响应)实测精确 15.0s → 502 诚实报错。
- **不是 panic**:全 log 无 "panicked at";Cargo.toml 明确 panic=unwind(handler panic 由 tokio 任务隔离)。
- **不是 drain 互锁**:SIGTERM 时挂起 in-flight,58s 有界退出。
- **不是 SAP 死**:楔死期间直连 ICM discovery 66–88ms 全绿、RFC_PING 2ms。
- **不是黑洞路径**:bhole 对照 15s 恒定,行为正确。

## 4. 矛盾点与怀疑方向

所有 DebugSession 请求都已 15s 有界 → 打断的处理器最坏 ~45s(discovery+csrf+POST 各 15s)就该排空队列。实际挂数分钟 ⇒ **有一个永不完成的持有者占着 `slot.bp_chan`**。候选机制:

1. **hyper/axum 客户端断连 × handler future 生命周期**:HTTP/1.1 下断连后 handler 是被 drop(OwnedMutexGuard 同步释放,应无害)还是继续跑完?若继续跑完则必有界——与观察矛盾。
2. **ICF 对 stateful 会话上断点 POST 的服务端长占**:缓存通道的 ABAP 上下文收到断连请求后卡在某个 debug 等待态;后续请求复用同一通道……但客户端 15s 超时是无视服务端的墙钟——仍应报错。**除非卡点不在 chan.request 而在别处**。
3. **tokio Mutex 等待链**:多个被打断的处理器排队,`lock_owned().await` 级联等待——但首持有者必有界,推不出永久占住。
4. **未审计的 await**:DebugSession 之外,bp 链路是否还有未加超时的等待点(如 `auth_header`/`plugin_extras`/pool checkout)。

## 5. 实验干扰警告(重要)

今晚 00:12–00:24 有**并发会话同时开发 debug 面**(debug.rs 00:17 仍在落盘)且多次重启网关:实验中二进制被换、进程两次死亡(00:16:15 / 00:19:46,均无 panic 无崩溃报告 = 外部 SIGTERM/SIGKILL)。上表 #1/#2 的"进程死亡"**不是本 bug 导致**。重新定位请在**单一会话、安静窗**进行,并先用 `ps -o lstart= -p $(pgrep -f release/sap_for_agents)` 锁定二进制代次。

## 6. 建议定位步骤(按成本递增)

1. **埋点**(最小成本):`bp_channel` 入口/出口、`drop_bp_channel`、`with_bp_chan!` body 进出各加一行 tracing(who/when/耗时)。复现一次,看 guard 持有者卡在哪一步。
2. **sample + lsof**:楔死时 `sample <pid> 3` 看 tokio worker 栈;`lsof -p <pid>` 看到 ICM 的 TCP 连接状态(是否有 ESTABLISHED 半开/发送队列堆积)。
3. **mock 单测**(区分网关/SAP 责任):用 `debug_session.rs` 的 `spawn_mock` 基建,mock 收到 `POST breakpoints` 后**hold 住连接永不响应**(模拟 SAP 长占形态),断言:15s 超时后 guard 释放、后续请求恢复。若单测不复现 ⇒ SAP 侧行为是关键差异,重心转向 ICF 会话态。
4. **止血选项**(如需先解燃眉):
   - `with_bp_chan!` body 整体包 `tokio::time::timeout(60s)`,超时强制 `drop_bp_channel`——牺牲等待换永不永久占住;
   - 或 bp 操作退回 temp_channel(每次新开,断连即弃)——注意会重新撞"会话风暴红线"(§6.5 设计稿)。

## 8. 结案:根因与修复(2026-11-20 深夜)

**根因:`bp_channel` 快速路径的 tokio Mutex 自死锁——与"客户端打断"无关,打断是红鲱鱼。**

移交会话的复现配方里,`bp_set` 实际 **0.14s 就完成了**(curl exit 0,根本不存在 2s 打断;其 "-m 2" 从未生效)。真正的触发序列是:

1. 网关重启后首个 bp 操作(`bp_set`)走**慢路径**(bp_chan 为 None → DebugSession::open → 填充缓存)——正常;
2. 之后的**任何** bp 操作走**快速路径**(缓存命中)——旧代码:

```rust
{
    let g = slot.bp_chan.lock().await;          // g 持锁
    if g.is_some() {
        return Ok(slot.bp_chan.clone().lock_owned().await);  // ← g 还活着就再锁同一把
    }
}
```

   tokio Mutex **不可重入**:`lock_owned().await` 等锁释放,而锁在 `g` 手里,`g` 要等 return 完成才 drop → **永久自死锁**。守卫被死等任务永久占住,之后所有 bp 操作在 `bp_channel` 入口排队,永不返回——任何请求级超时(15s/60s)都救不了,因为卡点不在 `chan.request` 而在锁获取(这正是 §4 疑点 2"卡点不在 chan.request"的答案)。

**为什么此前从未踩中**:集成测试 step 14 的 bp_list 之前有 `session_close` → `remove_slot` 把槽整个删掉,重查 `slot_for` 建的是新槽(bp_chan=None)→ 走慢路径;此前的手动验证也从未在同一槽上连续跑两次 bp 操作。移交会话的"2s 打断"实验恰好构造了 bp_set→bp_list 紧邻序列,于是 3/3"稳定"。

**修复(`src/debug.rs`)**:

1. `bp_channel` 改**单锁设计**:整段持一个 `MutexGuard`(慢路径 `DebugSession::open` 期间持锁串行化,顺带消除并发双开通道的浪费;tokio MutexGuard 可跨 await);绝不在持锁时对同一把锁再 lock;
2. `with_bp_chan!` 错误路径同理:`drop(g)` 之后才 `drop_bp_channel`(持锁嵌套同款错误);
3. 排查了全部同类模式:`teardown` 的 via 块本就有显式 `drop(g)`,无恙。

**验证(2026-11-20 00:45–00:50,A4H)**:修复后 §1 原配方 ×3——bp_set 33–123ms、bp_list **7–8ms**;bp_list 连打 10 次全部 ~7ms;单测 273 绿、集成 `debug_session_full_loop` PASS(17.36s)。

**§6 建议的定位步骤实际走了**:1(埋点)被静态审读跳过——重读代码即命中;2(sample)因进程在楔死观察期被外部重启而未拿到栈;3(mock hold 测试)不再必要——根因是网关侧死锁,与 SAP 行为无关(§4 疑点 1/2/3 全部排除,疑点 4 命中变体:卡点确实在 DebugSession 之外的锁获取)。

## 9. 相关工作树状态(移交时点)

- `src/debug_session.rs`:本次已加 `DEBUG_REQ_TIMEOUT=15s` 兜底(黑洞实证 60→15s,273 单测绿)——与定位无关但请知悉。
- `src/mcp.rs`:已修 2 个分发 bug(`_ =>` 臂顺序、bp_set 未剥 `action` 键)。
- 其余 debug 面 WIP 为并发会话所有。
- 记忆 [[v022-debugger-plan]] 已同步本 bug 与复现配方。
