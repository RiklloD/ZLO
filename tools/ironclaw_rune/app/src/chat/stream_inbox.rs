//! Where a thread's stream task leaves decoded messages for the UI thread.
//!
//! The task decodes on the network runtime and pushes here; only the push
//! that finds the inbox idle queues an update on the UI thread, and that
//! update takes everything pushed by the time it runs. However fast frames
//! arrive, one stream has at most one update waiting, and the UI thread
//! wakes once for it, not once per burst.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use zlo_client::StreamMessage;

#[derive(Default)]
pub(crate) struct StreamInbox {
    messages: Mutex<Vec<StreamMessage>>,
    /// An update that will take the messages is queued and has not run.
    posted: AtomicBool,
}

impl StreamInbox {
    /// Leave `batch`; true when the caller must queue the update that takes
    /// it (none is waiting).
    pub(crate) fn push(&self, batch: impl IntoIterator<Item = StreamMessage>) -> bool {
        self.messages
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(batch);
        !self.posted.swap(true, Ordering::AcqRel)
    }

    /// On the UI thread, in the queued update: everything left so far. A
    /// push after this queues a new update.
    pub(crate) fn take(&self) -> Vec<StreamMessage> {
        // Clear the flag first: a push racing this take either lands in
        // the messages taken now or queues the next update, never neither.
        self.posted.store(false, Ordering::Release);
        std::mem::take(&mut *self.messages.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

#[cfg(test)]
mod tests {
    use zlo_client::{StreamMessage, StreamStatus};

    use super::StreamInbox;

    fn status() -> StreamMessage {
        StreamMessage::Status(StreamStatus::Live)
    }

    #[test]
    fn only_the_push_that_finds_it_idle_queues_an_update() {
        let inbox = StreamInbox::default();
        assert!(inbox.push([status()]));
        assert!(!inbox.push([status(), status()]));
        assert!(!inbox.push([status()]));
        assert_eq!(inbox.take().len(), 4);
        assert!(inbox.take().is_empty());
        assert!(inbox.push([status()]), "taken: the next push queues again");
    }

    /// A writer racing the reader never strands a message: every push is
    /// either taken by a running update or queues one.
    #[test]
    fn no_message_is_stranded_by_a_racing_take() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let inbox = Arc::new(StreamInbox::default());
        let posts = Arc::new(AtomicUsize::new(0));
        let writer = {
            let (inbox, posts) = (inbox.clone(), posts.clone());
            std::thread::spawn(move || {
                for _ in 0..10_000 {
                    if inbox.push([status()]) {
                        posts.fetch_add(1, Ordering::SeqCst);
                    }
                }
            })
        };
        let mut taken = 0;
        let mut takes = 0;
        while !writer.is_finished() || takes < posts.load(Ordering::SeqCst) {
            if takes < posts.load(Ordering::SeqCst) {
                takes += 1;
                taken += inbox.take().len();
            }
        }
        writer.join().expect("writer");
        taken += inbox.take().len();
        assert_eq!(taken, 10_000);
    }
}
