//! 事件总线模块。
//!
//! 提供 Agent 与外部（CLI、telemetry 等）之间的异步事件通信。
//! Agent 作为唯一发布者，向 Bus 写入 Event；
//! 外部模块通过 subscribe 订阅，获得独立的事件流副本。

use tokio::sync::broadcast;

use crate::model::TokenUsage;

// ==================== 事件定义 ====================

/// Agent 产生的所有事件。
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    // Turn 生命周期
    TurnStarted,
    TurnComplete { usage: Option<TokenUsage> },

    // 模型输出（流式）
    TextDelta(String),
    TextDone(String),

    // 工具调用
    ToolCallBegin { id: String, name: String },
    ToolCallEnd {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },

    // 审批请求
    ApprovalRequired {
        id: String,
        tool_name: String,
        description: String,
    },

    // 错误
    Error(String),
}

// ==================== Bus ====================

/// 事件总线，支持一对多的发布/订阅。
///
/// 内部使用 `broadcast` channel，每个订阅者拥有独立的消费游标。
/// 慢消费者会收到 `Lagged` 通知并丢弃中间事件。
pub struct Bus {
    tx: broadcast::Sender<Event>,
}

/// 订阅者句柄，通过 `recv()` 异步消费事件。
pub struct Subscriber {
    rx: broadcast::Receiver<Event>,
}

impl Bus {
    /// 创建一个新的 Bus。
    ///
    /// `capacity` 为每个订阅者内部的缓冲区大小。
    /// 当事件堆积超过 capacity 时，最早的未消费事件会被丢弃，
    /// 订阅者的下一次 `recv()` 会返回 `RecvError::Lagged`。
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// 发布一个事件，所有活跃订阅者都会收到。
    ///
    /// 如果没有任何订阅者，事件被静默丢弃。
    pub fn publish(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    /// 创建一个新的订阅者。
    ///
    /// 订阅者只能收到订阅之后发布的事件。
    pub fn subscribe(&self) -> Subscriber {
        Subscriber {
            rx: self.tx.subscribe(),
        }
    }

    /// 返回当前活跃订阅者数量。
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl Subscriber {
    /// 异步接收下一个事件。
    ///
    /// 如果发送端已关闭（Bus 被 drop），返回 `None`。
    /// 如果消费速度落后于生产速度导致事件被丢弃，返回 `Lagged(n)`，
    /// 其中 `n` 为丢弃的事件数量，下一次调用会返回最新的可用事件。
    pub async fn recv(&mut self) -> Option<ReceiveResult> {
        match self.rx.recv().await {
            Ok(event) => Some(ReceiveResult::Event(event)),
            Err(broadcast::error::RecvError::Lagged(n)) => Some(ReceiveResult::Lagged(n)),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }
}

/// `Subscriber::recv()` 的返回值。
#[derive(Debug, Clone, PartialEq)]
pub enum ReceiveResult {
    /// 成功接收到一个事件。
    Event(Event),
    /// 消费速度落后，丢弃了 `n` 个事件。
    Lagged(u64),
}

// ==================== 测试 ====================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_and_subscribe() {
        let bus = Bus::new(16);
        let mut sub = bus.subscribe();

        bus.publish(Event::TurnStarted);
        bus.publish(Event::TextDelta("hello".to_string()));

        let result = sub.recv().await;
        assert!(matches!(result, Some(ReceiveResult::Event(Event::TurnStarted))));

        let result = sub.recv().await;
        assert!(matches!(
            result,
            Some(ReceiveResult::Event(Event::TextDelta(ref s))) if s == "hello"
        ));
    }

    #[tokio::test]
    async fn subscribe_receives_only_post_subscribe_events() {
        let bus = Bus::new(16);

        bus.publish(Event::TurnStarted);

        let mut sub = bus.subscribe();
        bus.publish(Event::TextDelta("after".to_string()));

        // 只收到订阅后的事件
        let result = sub.recv().await;
        assert!(matches!(
            result,
            Some(ReceiveResult::Event(Event::TextDelta(ref s))) if s == "after"
        ));
    }

    #[tokio::test]
    async fn multiple_subscribers() {
        let bus = Bus::new(16);
        let mut sub1 = bus.subscribe();
        let mut sub2 = bus.subscribe();

        bus.publish(Event::TurnStarted);

        let r1 = sub1.recv().await;
        let r2 = sub2.recv().await;

        assert!(matches!(r1, Some(ReceiveResult::Event(Event::TurnStarted))));
        assert!(matches!(r2, Some(ReceiveResult::Event(Event::TurnStarted))));
        assert_eq!(bus.subscriber_count(), 2);
    }

    #[tokio::test]
    async fn no_subscribers_event_dropped() {
        let bus = Bus::new(16);

        // 没有 subscriber，publish 不 panic
        bus.publish(Event::TurnStarted);

        let mut sub = bus.subscribe();
        bus.publish(Event::TextDelta("visible".to_string()));

        let result = sub.recv().await;
        assert!(matches!(
            result,
            Some(ReceiveResult::Event(Event::TextDelta(ref s))) if s == "visible"
        ));
    }

    #[tokio::test]
    async fn lagged_when_buffer_full() {
        let bus = Bus::new(2);
        let mut sub = bus.subscribe();

        // 填满 capacity=2 后继续发布，触发 lag
        bus.publish(Event::TurnStarted);
        bus.publish(Event::TextDelta("a".to_string()));
        bus.publish(Event::TextDelta("b".to_string()));
        bus.publish(Event::TextDelta("c".to_string()));

        // broadcast 保留最新的 capacity 个消息，lag 通知先到达
        let result = sub.recv().await;
        assert!(matches!(result, Some(ReceiveResult::Lagged(2))));

        // 缓冲区中保留的是最新的 2 个事件
        let result = sub.recv().await;
        assert!(matches!(
            result,
            Some(ReceiveResult::Event(Event::TextDelta(ref s))) if s == "b"
        ));

        let result = sub.recv().await;
        assert!(matches!(
            result,
            Some(ReceiveResult::Event(Event::TextDelta(ref s))) if s == "c"
        ));
    }

    #[tokio::test]
    async fn closed_when_bus_dropped() {
        let mut sub;
        {
            let bus = Bus::new(16);
            sub = bus.subscribe();
        }

        let result = sub.recv().await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn subscriber_count_updates() {
        let bus = Bus::new(16);
        assert_eq!(bus.subscriber_count(), 0);

        let sub1 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);

        let _sub2 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 2);

        drop(sub1);
        assert_eq!(bus.subscriber_count(), 1);
    }
}
