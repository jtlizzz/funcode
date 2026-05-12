# TUI Architecture

> 日期: 2026-05-12
> 分支: feat-tool
> 文件: `src/tui.rs`, `src/app.rs`, `src/config.rs`, `src/main.rs`

## 整体架构

```
main.rs  →  app::run()  →  tui::run_tui(handle, model_name)
                │                    │
                │                    ├── tokio tasks merge two event sources into one mpsc channel:
                │                    │   ├── crossterm EventStream (keyboard, resize)
                │                    │   └── AgentHandle::subscribe() (agent events)
                │                    │
                │                    └── process event → update DisplayState → render
                │
                └── wires: Config → OpenAIProvider → Model → Session + Tools → Agent → AgentHandle
```

TUI 是纯消费者：它不持有任何领域状态，只通过 `AgentHandle` 的两个方法与 Agent 交互：

- `user_turn(text)` — 提交用户输入
- `interrupt()` — 取消当前 turn

所有可视化信息来自 `Bus::subscribe()` 的 `Event` 流。

## 模块职责

### `config.rs`

从环境变量加载配置。调用 `dotenv::dotenv().ok()` 静默加载 `.env`，读取三个变量：

| 变量 | 必须 | 默认值 |
|------|------|--------|
| `OPENAI_API_KEY` | 是 | — |
| `OPENAI_BASE_URL` | 是 | — |
| `OPENAI_MODEL` | 是 | — |

### `app.rs`

组装层。按依赖顺序创建所有领域对象，启动 Agent，调起 TUI：

```
Config → OpenAIProvider → Model
                       → Session (system prompt + token budget)
                       → ToolRegistry (bash, file_read, file_edit, file_write, glob)
                       → Bus
                       → Agent::spawn() → AgentHandle
                       → tui::run_tui()
```

### `main.rs`

创建 `tokio::runtime::Runtime`，调用 `app::run()`，打印错误退出。

## TUI 内部结构 (`tui.rs`)

### 事件合并：AppEvent

```rust
enum AppEvent {
    Key(KeyEvent),       // crossterm 键盘事件
    Resize(u16, u16),    // 终端窗口大小变化
    AgentEvent(Event),   // Bus 广播的 Agent 事件
    AgentGone,           // Agent 后台 task 结束
}
```

两个异步事件源（crossterm `EventStream` + Bus `Subscriber`）各自通过一个 spawned task 转发到同一个 `mpsc::channel<AppEvent>`。主循环只需从一个 channel 消费，避免了 `tokio::select!` 分支间的状态竞争。

```
crossterm EventStream ──> spawned task ──> mpsc::Sender ──┐
                                                          ├──> mpsc::Receiver (merged stream)
Bus Subscriber        ──> spawned task ──> mpsc::Sender ──┘
```

为什么不直接用 `tokio::select!`：两个 `StreamExt::next()` 在同一个 `select!` 里可以工作，但每次只处理一个分支，另一个分支的缓冲区可能堆积。统一 channel 有背压控制，且主循环逻辑更简单。

### DisplayState

所有可变 UI 状态集中在一个结构体中，render 函数只读它：

| 字段 | 用途 |
|------|------|
| `output_lines` | 已完成的输出行（用户输入回显、assistant 文本、tool 状态） |
| `streaming_text` | 当前正在流式生成的 assistant 文本（TextDelta 累积，TextDone 时清空并移入 output_lines） |
| `model_name` | 状态栏显示 |
| `turn_active` | 状态栏 idle/busy，控制 Ctrl+C 行为 |
| `active_tool` | 当前正在执行的工具名（状态指示） |
| `total_tokens` | 累计 token 用量 |
| `input` / `input_cursor` | 用户输入缓冲区及光标位置 |
| `scroll_offset` | 输出区滚动偏移 |

### 终端生命周期：TuiGuard

RAII 包装器，Drop impl 确保终端状态恢复：

```
init()  → enable_raw_mode + EnterAlternateScreen + Terminal::new
Drop    → show_cursor + LeaveAlternateScreen + disable_raw_mode
```

### 渲染

垂直三段式布局：

```
┌─────────────────────────────┐
│                             │
│   OutputArea (scrollable)   │  Constraint::Min(3)
│                             │
├─────────────────────────────┤
│ gpt-4o-mini | idle | tok: 0│  Constraint::Length(1)  — Status bar
├─────────────────────────────┤
│ > user input here           │  Constraint::Length(1)  — Input area
└─────────────────────────────┘
```

- OutputArea: `Paragraph` + `Wrap`，包含 `output_lines` + `streaming_text` + `active_tool` 指示
- Status bar: 模型名 (cyan) | idle/busy (bold) | token 计数
- Input area: `> ` 前缀 + 用户输入，光标定位在 `2 + input_cursor` 位置

### 事件处理

**键盘事件** → `handle_key_event()` → 返回 `Option<TuiAction>`：

| 按键 | 条件 | 动作 |
|------|------|------|
| Enter | input 非空 | Submit |
| Ctrl+C | turn_active | Interrupt |
| Ctrl+C | !turn_active | Quit |
| Ctrl+D | — | Quit |
| Char | — | 插入字符 |
| Backspace | — | 删除字符 |
| Left/Right | — | 移动光标 |
| Up/Down | — | 滚动输出 |

**Agent 事件** → `handle_agent_event()` → 更新 DisplayState：

| Event | 更新 |
|-------|------|
| `TurnStarted` | turn_active = true |
| `TextDelta` | streaming_text += delta |
| `TextDone` | streaming_text → output_lines |
| `ToolCallBegin` | active_tool = name, output_lines += "[calling ...]" |
| `ToolCallEnd` | active_tool = None, output_lines += "[... done/failed]" |
| `TurnComplete` | turn_active = false, 累加 token |
| `TurnInterrupted` | turn_active = false, streaming_text 落地 |
| `Error` | turn_active = false, output_lines += "[error] ..." |
| `ApprovalRequired` | output_lines += "[approval] ..." |

### 主循环伪代码

```
init TuiGuard (raw mode + alternate screen)
create DisplayState
spawn crossterm → mpsc forwarder
spawn bus subscriber → mpsc forwarder

loop {
    event = merged_stream.next().await

    match event {
        Key(key) → handle_key_event() → maybe Submit/Interrupt/Quit
        AgentEvent(e) → handle_agent_event() → update state
        AgentGone → render final state, break
        Resize → no-op (ratatui auto-handles on next draw)
    }

    render(state, terminal)
}

drop TuiGuard (terminal restored)
```

## 设计决策

### 为什么事件合并用 mpsc 而不是 tokio::select!

两种方案都可以。mpsc 的优势：
- 主循环只有一个 `next().await`，逻辑线性
- channel 有缓冲区，crossterm 和 bus forwarder 不会互相阻塞
- 未来如果要加第三个事件源（如 signal handler），只需再 spawn 一个 forwarder

### 为什么 streaming_text 和 output_lines 分开

`streaming_text` 是"正在生成"的文本，每次 render 都以绿色高亮显示。`TextDone` 时整体移入 `output_lines`，颜色恢复正常。这避免了逐行累积时的闪烁，也保持了语义清晰。

### 为什么 TuiGuard 用 RAII Drop

终端状态恢复必须保证执行（即使 panic）。`Drop` impl 是 Rust 中最可靠的清理机制。`main.rs` 的错误处理只管打印错误退出，不需要手动恢复终端。

## 未来改进方向

- **多行输入**: 当前是单行 input，未来支持 Shift+Enter 换行
- **Markdown 渲染**: output_lines 当前是纯文本，可以引入 markdown 渲染
- **工具输出展示**: 当前只显示 `[calling ...]` / `[... done]`，未来展示工具返回内容
- **自动滚动**: 新内容到达时自动滚到底部（当前需要手动 Up/Down）
- **历史回溯**: 上下箭头在 input 空时切换历史输入（当前用于滚动输出）
