//! Bridges the client's tokio world to any other executor (GPUI's).
//!
//! reqwest needs a tokio reactor; GPUI runs its own executor. The backend owns
//! a small tokio runtime on two background threads. Callers get back a
//! `JoinHandle`, a plain `Future` that any executor can await, so the UI
//! thread never blocks and never needs tokio itself.

use std::future::Future;
use std::sync::Arc;

use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::api::Client;
use crate::error::ClientError;
use crate::stream::{EventStream, StreamMessage};

#[derive(Clone)]
pub struct Backend {
    runtime: Arc<Runtime>,
    client: Client,
}

impl Backend {
    pub fn new(client: Client) -> Result<Self, ClientError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("ironclaw-net")
            .enable_all()
            .build()
            .map_err(|e| ClientError::Config(format!("cannot start network runtime: {e}")))?;
        Ok(Self {
            runtime: Arc::new(runtime),
            client,
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Run `f(client)` on the network runtime.
    pub fn spawn<F, Fut, T>(&self, f: F) -> JoinHandle<T>
    where
        F: FnOnce(Client) -> Fut,
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.runtime.spawn(f(self.client.clone()))
    }

    pub fn subscribe(&self, thread_id: &str) -> (EventStream, mpsc::Receiver<StreamMessage>) {
        let _guard = self.runtime.enter();
        self.client.subscribe(thread_id)
    }
}
