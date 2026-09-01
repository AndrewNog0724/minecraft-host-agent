//! 协作式取消令牌（R4：Ctrl-C 打断当前回合）。
//!
//! 设计文档 §9 提到的 CancellationToken 语义此处手写实现：
//! `AtomicBool` 表达"已取消"状态，`Notify` 用于唤醒等待方，
//! 这样等待端可以直接放进 `tokio::select!`，轮询端用 `is_cancelled()`。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

#[derive(Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// 发起取消：置位状态并唤醒所有等待方。幂等。
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// 非阻塞检查（工具执行循环内轮询用）。
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// 异步等待取消发生（配合 `tokio::select!` 使用）。
    pub async fn cancelled(&self) {
        // 先查状态再等待，避免"取消发生在注册等待前"的竞态；
        // notify_waiters 只唤醒已注册的等待者，所以必须先 listen。
        if self.is_cancelled() {
            return;
        }
        let notified = self.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn cancelled_wakes_up_waiter() {
        let token = CancelToken::new();
        let waiter = token.clone();
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = waiter.cancelled() => "cancelled",
                _ = tokio::time::sleep(Duration::from_secs(5)) => "timeout",
            }
        });
        token.cancel();
        assert_eq!(task.await.unwrap(), "cancelled");
    }

    #[tokio::test]
    async fn cancel_before_wait_returns_immediately() {
        let token = CancelToken::new();
        token.cancel();
        // 已取消时 cancelled() 应立即返回（不挂起）
        tokio::select! {
            _ = token.cancelled() => {}
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                panic!("cancelled() 未在已取消状态下立即返回")
            }
        }
    }
}
