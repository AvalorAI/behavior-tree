use anyhow::Result;
use futures::future::select_all;
use futures::{Future, FutureExt};
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio::sync::mpsc::Sender;

use super::handle::{FutResponse, NodeError, NodeHandle, ParentMessage, Status};

// Simplify complex type
type FutVec = Vec<Pin<Box<dyn Future<Output = Result<FutResponse, NodeError>> + Send>>>;

pub struct Listener {
    handles: Vec<NodeHandle>,
    tx: Sender<Update>,
}

impl Listener {
    pub fn new(handles: Vec<NodeHandle>, tx: Sender<Update>) -> Self {
        Self { handles, tx }
    }

    pub async fn run_listeners(&mut self) -> Result<()> {
        let mut futures = self.extract_futures(); // To take ownership

        loop {
            let (response, _, rem_futures) = select_all(futures).await; // Listen out all actions
            futures = rem_futures;
            match response? {
                FutResponse::Child(handle_index, msg, rx) => {
                    // Dont break the running BT because an external crate dropped the rx!
                    if let Err(e) = self.process_msg(msg, handle_index).await {
                        log::warn!("Failed to send the node status updates: {e:?}")
                    }
                    futures.push(NodeHandle::run_listen(rx, handle_index).boxed());
                }
                _ => {}
            }
        }
    }

    fn extract_futures(&mut self) -> FutVec {
        let mut futures = vec![];
        for (handle_index, handle) in self.handles.clone().iter_mut().enumerate() {
            let rx = handle.get_rx();
            futures.push(NodeHandle::run_listen(rx, handle_index).boxed());
        }
        futures
    }

    async fn process_msg(&self, msg: ParentMessage, handle_index: usize) -> Result<()> {
        match msg {
            ParentMessage::RequestStart => {}
            ParentMessage::Status(status) => self.update_status(handle_index, status.into()).await?,
            ParentMessage::Poison(_) => self.update_status(handle_index, OuterStatus::Poisonend).await?,
            ParentMessage::Killed => self.update_status(handle_index, OuterStatus::Killed).await?,
        }
        Ok(())
    }

    async fn update_status(&self, handle_index: usize, status: OuterStatus) -> Result<()> {
        Ok(self
            .tx
            .send(Update::new(self.handles[handle_index].id.clone(), status))
            .await?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Update {
    pub id: String,
    #[serde(rename = "status")]
    pub status: OuterStatus,
}

impl Update {
    fn new(id: String, status: OuterStatus) -> Self {
        Self { id, status }
    }
}

#[derive(PartialEq, Debug, Clone, Serialize, Deserialize)]
pub enum OuterStatus {
    Killed,
    Poisonend,
    Succes,
    Failure,
    Running,
    Idle,
}

impl From<Status> for OuterStatus {
    fn from(status: Status) -> OuterStatus {
        match status {
            Status::Succes => OuterStatus::Succes,
            Status::Failure => OuterStatus::Failure,
            Status::Running => OuterStatus::Running,
            Status::Idle => OuterStatus::Idle,
        }
    }
}
