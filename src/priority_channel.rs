use std::{hash::Hash, sync::Arc};

use ahash::RandomState;
use async_atomic::Atomic;
use futures::Stream;
use priority_queue::PriorityQueue;
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum SendError<T> {
    #[error("the channel is closed and no more messages may be sent")]
    Closed(T),
}

#[derive(Debug, thiserror::Error)]
pub enum TryRecvError {
    #[error("the channel is empty")]
    Empty,
    #[error("the channel is closed and empty")]
    ClosedAndEmpty,
}

#[derive(Debug, thiserror::Error)]
pub enum RecvError {
    #[error("the channel is closed and empty")]
    ClosedAndEmpty,
}

struct PriorityChannelInner<T: Hash + Eq, P: Ord> {
    queue: Mutex<PriorityQueue<T, P, RandomState>>,
    sender_count: Atomic<usize>,
    receiver_count: Atomic<usize>,
    closed: Atomic<bool>,
    len: Atomic<usize>,
}

impl<T: Hash + Eq, P: Ord> PriorityChannelInner<T, P> {
    fn close(&self) -> bool {
        !self.closed.swap(true)
    }

    fn is_closed(&self) -> bool {
        self.closed.load()
    }

    fn is_empty(&self) -> bool {
        self.len.load() == 0
    }
}

pub struct PriorityReceiver<T: Hash + Eq, P: Ord> {
    inner: Arc<PriorityChannelInner<T, P>>,
}

pub struct PrioritySender<T: Hash + Eq, P: Ord> {
    inner: Arc<PriorityChannelInner<T, P>>,
}

pub fn priority_channel<T: Hash + Eq, P: Ord>() -> (PrioritySender<T, P>, PriorityReceiver<T, P>) {
    let queue = Mutex::new(PriorityQueue::<T, P, RandomState>::with_default_hasher());
    let sender_count = Atomic::new(1);
    let receiver_count = Atomic::new(1);
    let closed = Atomic::new(false);
    let len = Atomic::new(0);

    let inner = Arc::new(PriorityChannelInner {
        queue,
        sender_count,
        receiver_count,
        closed,
        len,
    });

    let sender = PrioritySender {
        inner: inner.clone(),
    };
    let receiver = PriorityReceiver { inner };
    (sender, receiver)
}

impl<T: Hash + Eq, P: Ord> Clone for PrioritySender<T, P> {
    fn clone(&self) -> Self {
        let count = self.inner.sender_count.fetch_add(1);
        if count == usize::MAX {
            panic!("sender_count overflowed usize::MAX");
        }
        PrioritySender {
            inner: self.inner.clone(),
        }
    }
}

// XXX: This would be cool, but the async-atomic only supports a single listener
// impl<T: Hash + Eq, P: Ord> Clone for PriorityReceiver<T, P> {
//     fn clone(&self) -> Self {
//         let count = self.inner.receiver_count.fetch_add(1);
//         if count == usize::MAX {
//             panic!("sender_count overflowed usize::MAX");
//         }
//         PriorityReceiver {
//             inner: self.inner.clone(),
//         }
//     }
// }

impl<T: Hash + Eq, P: Ord> Drop for PrioritySender<T, P> {
    fn drop(&mut self) {
        if self.inner.sender_count.fetch_sub(1) == 1 {
            self.inner.close();
        }
    }
}

impl<T: Hash + Eq, P: Ord> Drop for PriorityReceiver<T, P> {
    fn drop(&mut self) {
        if self.inner.receiver_count.fetch_sub(1) == 1 {
            self.inner.close();
        }
    }
}

impl<T: Hash + Eq, P: Ord> PrioritySender<T, P> {
    pub async fn send(&self, msg: T, prio: P) -> Result<(), SendError<T>> {
        if self.inner.is_closed() {
            return Err(SendError::Closed(msg));
        }
        let mut queue = self.inner.queue.lock().await;
        queue.push(msg, prio);
        let old_len = self.inner.len.fetch_add(1);
        if old_len == usize::MAX {
            panic!("queue overflowed usize::MAX");
        }
        Ok(())
    }
}

impl<T: Hash + Eq, P: Ord> PriorityReceiver<T, P> {
    pub async fn try_recv(&self) -> Result<(T, P), TryRecvError> {
        match (self.inner.is_closed(), self.inner.is_empty()) {
            (true, true) => Err(TryRecvError::ClosedAndEmpty),
            (false, true) => Err(TryRecvError::Empty),
            (_, false) => {
                let mut queue = self.inner.queue.lock().await;
                let (msg, prio) = queue.pop().expect("queue is guaranteed to be non-empty");
                self.inner.len.fetch_sub(1);
                Ok((msg, prio))
            }
        }
    }

    pub async fn recv(&self) -> Result<(T, P), RecvError> {
        loop {
            match self.try_recv().await {
                Ok(val) => {
                    return Ok(val);
                }
                Err(TryRecvError::Empty) => {
                    let mut len = self.inner.len.subscribe_ref();
                    let mut closed = self.inner.closed.subscribe_ref();
                    tokio::select! {
                        _ = len.wait(|len| len > 0) => {
                            continue;
                        },
                        _ = closed.wait(|closed| closed) => {
                            continue
                        },
                    }
                }
                Err(TryRecvError::ClosedAndEmpty) => {
                    return Err(RecvError::ClosedAndEmpty);
                }
            }
        }
    }

    pub fn into_stream(self) -> impl Stream<Item = (T, P)> + Unpin {
        Box::pin(futures::stream::unfold(self, |state| async move {
            match state.recv().await {
                Ok(val) => Some((val, state)),
                Err(RecvError::ClosedAndEmpty) => None,
            }
        }))
    }
}
