# Agent Run Loop 设计讨论记录

> 日期: 2026-04-28
> 分支: feat-run-loop
> 状态: **倾向方案 B'：Codex-lite / Arc<Session> / spawned turn task**

## 问题

`Agent::submit(&mut self, op: Op)` 直接 `await run_turn()`，导致：

1. `run_turn` 执行期间（模型生成、工具调用）无法接收新的 `Op`
2. `Op::Interrupt` 无法及时处理

核心矛盾：**把“外部可控制的运行中实例”和“独占业务状态的执行器”混在了同一个 `Agent` 对象里**。

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

共同点不是“取消信号都不走队列”，而是：

1. **控制面不能被 turn 执行阻塞**
   - Codex: `submission_loop` 继续消费 `Op::Interrupt`
   - Claude: control request 入队，同时直接 abort 当前 controller
2. **每个 turn 都有独立取消句柄**
   - Codex: `ActiveTurn` 持有 per-turn `CancellationToken`
   - Claude: 当前 turn 持有 `AbortController`
3. **队列里的 Interrupt 负责状态收敛，取消句柄负责即时打断**
   - Codex 的取消句柄由 `submission_loop` 收到 `Interrupt` 后触发
   - Claude 的取消句柄可被外部控制路径直接触发

因此 funcode 不应该继续用可重置的 `watch<bool>` 模拟 per-turn cancel。更自然的模型是：
`Interrupt` 回到 `Op`，当前 turn 的 `CancellationToken` 保存在 `ActiveTurn`，由主循环收到
`Interrupt` 后触发。

## funcode 的讨论方向

### 方案 A: 消息队列 + 独立取消通道（已实施，用户不满意）

```
调用方 -> AgentSender.submit(op) -> mpsc channel -> Agent.run() 顺序消费
调用方 -> AgentSender.cancel()   -> watch channel -> run_turn 内 select! 感知
```

- `Op` 枚举只保留 `UserTurn`（排队操作）
- 取消走 `watch::channel(false)`，每轮 turn 重置
- `run(&mut self)` 循环消费队列，每轮创建新 `CancellationToken`
- **优点**: `&mut self` 就够，不需要 `Arc<Mutex<...>>`
- **缺点**:
  - 调用方需要持有额外的 `AgentSender`
  - `watch<bool>` 需要每轮 reset，存在 stale value / race condition 风险
  - `CancellationToken` 本身是 per-turn 对象，不适合被一个可重置 bool 信号间接驱动
  - `run_turn` 同时承担模型循环和取消桥接，职责不够清晰

### 方案 B: 粗粒度 Arc 共享状态（不推荐）

- 直接把整个 Agent 或 Session 包成 `Arc<Mutex<...>>`
- `Interrupt` 放回 `Op` 枚举
- `submission_loop` 不阻塞在 `run_turn` 上
- **优点**: 最容易实现 spawned turn task
- **缺点**:
  - 容易变成 `Arc<Mutex<Agent>>`，把编排、会话历史、模型调用、工具执行锁成一坨
  - 容易持锁跨 `.await`
  - 领域边界不清晰，后续扩展 approval / tool / realtime 时会放大复杂度

### 方案 B': Codex-lite（推荐）

采用 Codex 的生命周期模型，但保持 funcode 的领域边界：

```
Agent(handle)
  ├─ tx_sub: mpsc::Sender<Submission>
  ├─ bus: Bus
  ├─ session: Arc<Session>
  └─ loop_done: Shared<BoxFuture<'static, ()>>

submission_loop(rx_sub, Arc<Session>, Model, ToolRegistry, Bus)
  ├─ Op::UserTurn(text) -> spawn turn task
  ├─ Op::Interrupt      -> session.abort_active_turn(Interrupted)
  └─ Op::Shutdown       -> abort active turn, cleanup, break

Session(domain object)
  ├─ state: Mutex<SessionState>
  └─ active_turn: Mutex<Option<ActiveTurn>>
```

关键点：

- 外部 `Agent` 是简单 handle，不再是独占执行器
- `Session` 是领域对象，内部自行管理锁，不向外暴露 `MutexGuard`
- `run_turn` 作为后台 task 运行，`submission_loop` 不会被模型流或工具调用阻塞
- `Interrupt` 回到 `Op`，调用方 API 统一
- cancel 使用 per-turn `CancellationToken`，不 reset、不复用
- 同一 session 同时最多一个 active turn；新 `UserTurn` 会 replace 当前 turn

外部 API 目标：

```rust
let agent = Agent::spawn(model, session, registry, bus, max_turns).await?;
agent.submit(Op::UserTurn("hello".into())).await?;
agent.interrupt().await?;
agent.shutdown_and_wait().await?;
```

内部 `Session` 方法示例：

```rust
impl Session {
    async fn start_turn(&self, active: ActiveTurn);
    async fn abort_active_turn(&self, reason: TurnAbortReason);
    async fn build_request(&self, tools: &[ToolSpec]) -> ModelRequest;
    async fn record_item(&self, item: Item);
    async fn record_usage(&self, usage: TokenUsage);
}
```

锁使用规则：

1. 不持有 `SessionState` 锁跨模型请求、工具执行、事件等待
2. `Session` 暴露领域方法，不暴露内部锁
3. `run_turn` 每次只短暂锁 session：构造 request、记录 item、记录 usage
4. turn 完成写回时校验 `turn_id`，防止旧 turn 被 interrupt/replaced 后晚到写入
5. `CancellationToken` 永远 per-turn 创建，不 reset、不复用

## 已完成的代码改动（feat-run-loop 分支）

以下是**已实施但用户不满意**的方案 A 的改动，可 `git diff` 查看：

### 改动文件: `src/agent.rs`

1. `Op` 枚举移除 `Interrupt`，只保留 `UserTurn`
2. 新增 `AgentSender` 结构体（`op_tx` + `cancel_tx: watch::Sender<bool>` + `bus`）
3. `Agent::new` 返回 `(Self, AgentSender)`
4. 新增 `Agent::run(mut self) -> Self` 主循环
5. 删除 `Agent::submit`
6. `run_turn` 内部：
   - 每轮重置 `watch::send(false)` + 创建新 `CancellationToken`
   - 流式消费循环用 `tokio::select!` 同时监听 `stream.next()` 和 `cancel_rx.changed()`
7. 测试全部适配（9 个测试通过）

### 用户不满意的原因

- `AgentSender` 是额外的类型，调用方需要多持有一个东西
- watch 重置有时序问题（stale permit / race condition）
- 整体感觉过度工程，不够简洁

## 关键约束

1. 对外接口保持简单：调用方只持有 `Agent`
2. cancel 只取消当前 turn，不影响后续 turn
3. 内部可以使用 `Arc` / `Mutex`，但要保持领域边界清晰
4. 不使用可 reset 的 `watch<bool>` 表达 per-turn cancel
5. `CancellationToken` 一旦 cancel 就不能 un-cancel，因此必须 per-turn 创建
6. `submission_loop` 不能阻塞在 `run_turn` 上，否则 `Interrupt` 仍然无法及时处理

## 结论

方案 A 解决了 interrupt 即时性，但 API 和取消语义都不理想。

下一步应转向方案 B'：

- `Agent` 改为运行中实例的 handle
- `Op::Interrupt` 放回统一 `Op`
- `Session` 改为 `Arc<Session>` + 内部细粒度锁
- active turn 显式建模为 `ActiveTurn`
- turn task spawned，`submission_loop` 只负责调度和生命周期管理

这不是简单照搬 Codex，而是保留 funcode 的 Domain-Driven 边界：`Session` 仍然拥有历史和 token
预算逻辑，`Agent` 只负责编排生命周期，`Model` 只负责响应生成，`ToolRegistry` 只负责工具执行。
