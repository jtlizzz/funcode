# In-Session Model Switching

> 日期: 2026-05-12
> 状态: 待实施
> 影响文件: `src/agent.rs`, `src/tui.rs`, `src/bus.rs`

## 需求

用户在 TUI 会话中通过 `/model <name>` 命令切换模型，无需重启。会话历史保留，下一次模型调用使用新模型名。

## 参考

| 项目 | 机制 | 参考 |
|------|------|------|
| Codex CLI | `/model` 打开 picker popup，dispatch `AppEvent::UpdateModel` | `codex-rs/tui/src/chatwidget.rs:7903-7924` |
| DeepSeek-TUI | `/model <name>` 直接切换，或 `/model` 打开 picker；发 `Op::SetModel` 到 engine | `crates/tui/src/core/ops.rs:58-60` |

两者共同点：模型切换是一个 runtime op，通过 channel 发到 engine loop，session history 不动。

## 实施步骤

### 1. 添加 `Op::SetModel` 变体

文件: `src/agent.rs` — `Op` enum (line 49)

```rust
pub enum Op {
    UserTurn(String),
    SetModel(String),  // ← 新增
    Shutdown,
}
```

### 2. Agent 处理 `SetModel`

文件: `src/agent.rs` — `run_op_loop` (line 210) 和 `submit` (line 187)

`run_op_loop` 增加 match arm：

```rust
Op::SetModel(name) => {
    self.model.set_model_name(&name);
    self.bus.publish(Event::ModelChanged { model: name });
}
```

`submit` 增加 match arm（直接调用场景）：

```rust
Op::SetModel(name) => {
    self.model.set_model_name(&name);
    TurnOutcome::Completed { usage: None }
}
```

### 3. Model 增加 setter

文件: `src/model.rs` — `Model` struct (line 333)

```rust
impl Model {
    pub fn set_model_name(&mut self, model: impl Into<String>) {
        self.model = model.into();
    }
}
```

`Model.model` 字段目前是 `String`，直接赋值即可。不需要改 `provider` — provider (OpenAI client) 是无状态的，同一个 provider 实例可以发送任意模型名。

### 4. 添加 `Event::ModelChanged`

文件: `src/bus.rs` — `Event` enum (line 15)

```rust
pub enum Event {
    // ... existing variants ...
    ModelChanged { model: String },  // ← 新增
}
```

这是一个 observation event，通知 TUI 状态栏更新。

### 5. AgentHandle 增加 `set_model` 方法

文件: `src/agent.rs` — `AgentHandle` impl (line 93)

```rust
pub async fn set_model(&self, model: String) -> Result<(), AgentHandleError> {
    self.tx
        .send(Op::SetModel(model))
        .await
        .map_err(|_| AgentHandleError::Closed)
}
```

### 6. TUI 解析 `/model` 命令

文件: `src/tui.rs` — `handle_key_event` 或 Submit action 处理

在 Submit 分支中，提交前检查 input 是否以 `/model` 开头：

```rust
TuiAction::Submit(text) => {
    if let Some(name) = text.strip_prefix("/model ") {
        let name = name.trim().to_string();
        if !name.is_empty() {
            state.output_lines.push(format!("[model → {}]", name));
            let _ = handle.set_model(name).await;
        }
    } else {
        state.output_lines.push(format!("> {}", text));
        if handle.user_turn(text).await.is_err() {
            state.output_lines.push("[agent closed]".to_string());
            break;
        }
    }
}
```

### 7. TUI 处理 `ModelChanged` 事件

文件: `src/tui.rs` — `handle_agent_event`

```rust
Event::ModelChanged { model } => {
    state.model_name = model;
    state.output_lines.push(format!("[model changed to {}]", state.model_name));
}
```

状态栏自动在下一次 render 时显示新模型名。

## 约束

- **只能在 idle 状态切换**: `SetModel` 是普通 Op，进入 mpsc 队列排队。`run_op_loop` 串行消费，所以 SetModel 会在当前 UserTurn 完成后才处理。这与 Codex CLI 一致（Codex 在 task 执行期间禁用 `/model`）。不需要额外的锁或检查。
- **不做持久化**: V1 不把模型选择写回 `.env` 或配置文件。模型切换仅在当前会话有效。
- **不做 picker UI**: V1 只支持 `/model <name>` 直接指定。未来可以加交互式 picker。
- **不验证模型名**: 交给 provider 的 API 返回错误。TUI 通过 `Event::Error` 展示。

## 数据流

```
用户输入 "/model gpt-4o"
    ↓
TUI handle_key_event → TuiAction::Submit
    ↓
strip_prefix("/model ") → handle.set_model("gpt-4o")
    ↓
mpsc::Sender<Op> → Op::SetModel("gpt-4o")
    ↓
run_op_loop 收到 Op::SetModel
    ↓
self.model.set_model_name("gpt-4o")
self.bus.publish(Event::ModelChanged { model: "gpt-4o" })
    ↓
TUI handle_agent_event → DisplayState.model_name = "gpt-4o"
    ↓
render() → 状态栏显示新模型名
```
