# Agent Run Loop 设计讨论记录

> 日期: 2026-04-28
> 分支: feat-run-loop
> 状态: **已实施：方案 C — spawned Agent + shared CancellationToken**

## 问题

`Agent::submit(&mut self, op: Op)` 直接 `await run_turn()`，导致：

1. `run_turn` 执行期间（模型生成、工具调用）无法接收新的 `Op`
2. `Op::Interrupt` 无法及时处理

核心矛盾：**把”外部可控制的运行中实例”和”独占业务状态的执行器”混在了同一个 `Agent` 对象里**。

如果 `Agent::submit(&mut self, op)` 在同一个调用栈里 `await run_turn(&mut self)`，那么 `run_turn`
期间外部无法再拿到同一个 `Agent` 的可变引用来提交 `Interrupt`。这不是单纯的 Rust 限制，而是
对象职责边界不清晰：运行句柄、消息队列、会话状态、turn task 生命周期需要拆开。

## 三个参考项目的对比

### Codex CLI (Rust)

- `Interrupt` 和 `UserTurn` 在同一个 `Op` 枚举里，走同一个 channel
- `submission_loop` 用 `Arc<Session>` 共享状态，turn 作为 spawned task 运行（非阻塞）
- 所以 loop 能立刻接收下一个 `Op`（包括 `Interrupt`）
- `Interrupt` 本身并不是独立通道；它依然经过 `Submission` 队列。即时性来自于 `submission_loop` 不阻塞在 turn task 上
- `Session` 不是简单的 `Arc<Mutex<Session>>` 粗粒度大锁，而是 `Arc<Session>` + 内部细粒度锁：
  - `state: Mutex<SessionState>`
  - `active_turn: Mutex<Option<ActiveTurn>>`
  - 其他服务对象按各自职责持有
- **代价**: 引入内部可变性和运行时锁，需要严格避免持锁跨 `.await` 的复杂操作

参考：`/home/acer/project/rust_project/codex-main/codex-rs/core/src/codex.rs`

### Claude Code (TypeScript)

- Interrupt 作为 control request 进队列（和用户操作同一个 FIFO）
- **同时**直接调当前 turn 的 `abortController.abort()` 立即生效
- JS 没有所有权问题，`AbortController` 可以任意传递
- 队列里的 Interrupt 用于主循环/状态机收敛；直接 abort 用于立即打断正在进行的模型或工具流程
- **代价**: JS 没有 Rust 的借用约束，但仍然要处理取消与状态清理的双路径一致性

参考：`/home/acer/project/node_project/claude-code/src/`

### 共同点

1. **控制面不能被 turn 执行阻塞**
2. **每个 turn 都有独立取消句柄**
3. **取消句柄负责即时打断，不经过消息队列排队**

## 历史方案回顾

### 方案 A: 消息队列 + watch 取消通道（已废弃）

```
调用方 -> AgentSender.submit(op) -> mpsc channel -> Agent.run() 顺序消费
调用方 -> AgentSender.cancel()   -> watch channel -> run_turn 内 select! 感知
```

- `watch<bool>` 每轮 reset，存在 stale value / race condition 风险
- `AgentSender` 是额外的类型，调用方需要多持有一个东西

### 方案 B: 粗粒度 Arc 共享状态（不推荐）

- `Arc<Mutex<Agent>>` 把编排、会话、模型调用、工具执行锁成一坨
- 容易持锁跨 `.await`

### 方案 B': Codex-lite（部分采纳）

- `Arc<Session>` + 内部细粒度锁 + spawned turn task
- 完整实施需要大量重构 Session，当前阶段过重

## 当前实施方案：方案 C — spawned Agent + shared CancellationToken

借鉴 Claude Code 的双路径取消思路（取消不走队列、直接触发），但保持 funcode 的简单结构：
Agent 整体 `spawn` 进后台 task，独占所有业务状态（`&mut self`），外部通过 `AgentHandle` 控制。

### 架构

```
外部调用方
  │
  ├─ AgentHandle::user_turn(text)  ──> mpsc::Sender<Op> ──> run_op_loop
  ├─ AgentHandle::interrupt()      ──> Arc<Mutex<CancellationToken>>.cancel() （直接触发）
  ├─ AgentHandle::shutdown()       ──> mpsc::Sender<Op::Shutdown> ──> run_op_loop
  └─ AgentHandle::subscribe()      ──> Bus::subscribe()

Agent（owned by spawned task, 独占 &mut self）
  ├─ model: Model
  ├─ session: Session
  ├─ registry: ToolRegistry
  ├─ bus: Bus
  ├─ cancel: Arc<Mutex<CancellationToken>>    ← 与 Handle 共享
  └─ max_turns: usize

run_op_loop(rx) {
    while let Some(op) = rx.recv().await {
        match op {
            Op::UserTurn(text) => { session.push(user); self.run_turn().await; }
            Op::Shutdown => { cancel_current_turn(); break; }
        }
    }
}

run_turn() {
    self.reset_cancel();   // 每轮替换为全新 CancellationToken
    for turn in 0..max_turns {
        stream = model.stream(request, self.cancel());
        // stream 内部用 tokio::select! 同时监听 cancel.cancelled() 和 SSE
    }
}
```

### 关键设计决策

1. **Agent 整体 spawn，不拆分 Session**
   - `run_op_loop` 持有 `Agent` 的全部所有权，所有状态修改通过 `&mut self`
   - 不需要 `Arc<Session>` 或内部锁，保持领域对象的简单性
   - 代价：`run_op_loop` 在 `run_turn()` 期间阻塞在同一个 task 里，无法同时消费新 Op

2. **取消走独立路径，不经消息队列**
   - `AgentHandle::interrupt()` 直接操作 `Arc<Mutex<CancellationToken>>`
   - 即使 `run_op_loop` 正在 `await run_turn()`，取消也能立即生效
   - `CancellationToken` 在 `run_turn()` 开头通过 `reset_cancel()` 替换为全新实例
   - 前一轮的取消不会泄漏到后续 turn

3. **`Op` 枚举只保留 `UserTurn` 和 `Shutdown`**
   - `Interrupt` 不在 `Op` 里，因为队列阻塞时无法及时处理
   - 这与 Claude Code 的双路径模型一致：abort 直接触发，不走 FIFO

4. **`Session` 保持纯同步领域对象**
   - `push()` / `build_request()` / `record_usage()` 都是 `&mut self`
   - 不引入内部可变性，不需要运行时锁
   - 当需要拆分 turn task 时再引入 `Arc<Session>`

### 与方案 B' 的关系

方案 C 是 B' 的简化过渡：

- 相同点：外部 handle + 后台 loop + per-turn CancellationToken + Bus 事件
- 不同点：Agent 整体 spawn（而非拆分 Session 为 Arc）、`run_op_loop` 串行消费（而非 spawned turn task）
- 方案 C 的 `reset_cancel()` 用 `Arc<Mutex<CancellationToken>>` 的原地替换替代了 B' 的 per-turn token
- 当出现并发 turn 需求（如 sub-agent）时，再向 B' 演进

### 外部 API

```rust
let handle = Agent::spawn(agent, 16);

handle.user_turn(“hello”.to_string()).await?;  // 提交用户输入
handle.interrupt().await?;                      // 立即取消当前 turn
handle.shutdown().await?;                       // 关闭后台 loop

let mut sub = handle.subscribe();               // 订阅事件流
```

### 取消语义保证

1. `interrupt()` 调用后，正在进行的 `model.stream()` 内部 `tokio::select!` 立即感知取消
2. `run_turn()` 收到 `Cancelled` 后发出 `Event::TurnInterrupted`，不 push 任何 assistant item
3. `reset_cancel()` 在下一次 `run_turn()` 开头执行，确保取消不泄漏
4. `shutdown()` 先 cancel 当前 turn，再退出 loop
