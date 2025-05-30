use actify::CacheRecvNewestError;
use anyhow::Result;
use simple_xml_builder::XMLElement;

use thiserror::Error;
use tokio::sync::broadcast::{error::SendError, Receiver, Sender};
use uuid::Uuid;

#[derive(Debug)]
pub struct NodeHandle {
    pub element: String,
    pub name: String,
    pub id: String,
    pub children_names: Vec<String>,
    pub children_ids: Vec<String>,
    handles: Vec<NodeHandle>,
    tx: Sender<ChildMessage>, // This handle is held by a parent, so it can send child messages
    rx: Receiver<ParentMessage>, // The parent can receive messages from its child, so can listen to the handle for messages
}

impl Clone for NodeHandle {
    fn clone(&self) -> NodeHandle {
        Self {
            rx: self.rx.resubscribe(), // An rx cannot be cloned, but it can be created by subscribing to the transmitter
            tx: self.tx.clone(),
            element: self.element.clone(),
            name: self.name.clone(),
            id: self.id.clone(),
            handles: self.handles.clone(),
            children_names: self.children_names.clone(),
            children_ids: self.children_ids.clone(),
        }
    }
}

impl NodeHandle {
    pub fn new<S, T>(
        tx: Sender<ChildMessage>,
        rx: Receiver<ParentMessage>,
        element: S,
        name: T,
        children_names: Vec<String>,
        children_ids: Vec<String>,
        handles: Vec<NodeHandle>,
    ) -> NodeHandle
    where
        S: Into<String>,
        T: Into<String>,
    {
        Self {
            tx,
            rx,
            element: element.into(),
            name: name.into(),
            id: Uuid::now_v1(&[1, 2, 3, 4, 5, 6]).to_string(),
            handles,
            children_names,
            children_ids,
        }
    }

    pub(crate) async fn kill(&mut self) {
        if self.tx.receiver_count() > 0 {
            // It already exited if no receiver is alive
            if self.send(ChildMessage::Kill).is_err() {
                log::debug!(
                    "Send failed for ChildMessage::Kill - {} {:?} already exited",
                    self.element,
                    self.name
                );
                return;
            };
            loop {
                match self.listen().await {
                    Ok(ParentMessage::Killed) => {
                        log::debug!(
                            "Received ParentMessage::Killed from {} {}",
                            self.element,
                            self.name
                        );
                        return;
                    }
                    Ok(_) => {} // Some other message that can be discarded
                    Err(NodeError::TokioBroadcastRecvError(_)) => {
                        log::debug!(
                            "Error while listening to child - {} {} already exited",
                            self.element,
                            self.name
                        );
                        return;
                    }
                    Err(e) => log::debug!("Node error received {e:?}"),
                };
            }
        } else {
            log::debug!("{} {:?} already exited", self.element, self.name);
        }
    }

    pub fn take_handles(&mut self) -> Vec<NodeHandle> {
        let mut handles = std::mem::take(&mut self.handles);
        handles.push(self.clone());
        handles
    }

    pub fn send(&self, msg: ChildMessage) -> Result<(), NodeError> {
        self.tx.send(msg)?;
        Ok(())
    }

    pub fn get_rx(&mut self) -> Receiver<ParentMessage> {
        self.rx.resubscribe()
    }

    // Consuming and returning the receiver allows stacking it in a future vector
    pub async fn run_listen(
        mut rx: Receiver<ParentMessage>,
        child_index: usize,
    ) -> Result<FutResponse, NodeError> {
        let msg = NodeHandle::_listen(&mut rx).await?;
        Ok(FutResponse::Child(child_index, msg, rx)) // The rx is returned to ensure the channel is fully read
    }

    pub async fn listen(&mut self) -> Result<ParentMessage, NodeError> {
        NodeHandle::_listen(&mut self.rx).await
    }

    async fn _listen(rx: &mut Receiver<ParentMessage>) -> Result<ParentMessage, NodeError> {
        Ok(rx.recv().await?)
    }

    pub fn get_xml(&self) -> XMLElement {
        let element = if self.element == "Condition" {
            String::from("Decorator") // Groot sees any condition as decorator
        } else {
            self.element.clone()
        };
        let mut element = XMLElement::new(element);
        element.add_attribute("name", &self.name);
        element
    }

    pub fn get_json(&self) -> serde_json::value::Value {
        if !self.children_names.is_empty() {
            serde_json::json!({
                "id": self.id.clone(),
                "name": self.name.clone(),
                "type": self.element.clone(),
                "children": self.children_ids.clone()})
        } else {
            serde_json::json!({
                "id": self.id.clone(),
                "name": self.name.clone(),
                "type": self.element.clone()})
        }
    }
}

#[derive(Debug)]
pub enum FutResponse {
    Parent(ChildMessage, Receiver<ChildMessage>),
    Child(usize, ParentMessage, Receiver<ParentMessage>),
}

pub trait Node: Sync + Send {
    async fn serve(self);

    // Consuming and returning the receiver allows stacking it in a future vector
    async fn run_listen_parent(mut rx: Receiver<ChildMessage>) -> Result<FutResponse, NodeError> {
        let msg = rx.recv().await?;
        Ok(FutResponse::Parent(msg, rx))
    }
}

#[derive(PartialEq, Debug, Clone)]
pub enum Status {
    Success,
    Failure,
    Running,
    Idle,
}

impl Status {
    pub fn is_running(&self) -> bool {
        matches!(self, Status::Running)
    }

    pub fn is_failure(&self) -> bool {
        matches!(self, Status::Failure)
    }

    pub fn is_idle(&self) -> bool {
        matches!(self, Status::Idle)
    }

    pub fn is_succes(&self) -> bool {
        matches!(self, Status::Success)
    }
}

#[derive(PartialEq, Debug, Clone)]
pub enum ChildMessage {
    Start,
    Stop,
    Kill,
}

impl ChildMessage {
    pub fn is_kill(&self) -> bool {
        *self == ChildMessage::Kill
    }
}

#[derive(PartialEq, Debug, Clone)]
pub enum ParentMessage {
    RequestStart,
    Status(Status),
    Poison(NodeError),
    Killed,
}

#[derive(Error, Debug, PartialEq, Clone)]
pub enum NodeError {
    #[error("The node is killed")]
    KillError,
    #[error("Poison error: {0}")]
    PoisonError(String),
    #[error("Execution error: {0}")]
    ExecutionError(String),
    #[error("Tokio broadcast send error: {0}")]
    TokioBroadcastSendError(String),
    #[error("Tokio broadcast receiver error")]
    TokioBroadcastRecvError(#[from] tokio::sync::broadcast::error::RecvError),
    #[error("Cache Error")]
    CacheError(#[from] CacheRecvNewestError),
}

impl<T> From<SendError<T>> for NodeError {
    fn from(err: SendError<T>) -> NodeError {
        NodeError::TokioBroadcastSendError(err.to_string())
    }
}

impl From<anyhow::Error> for NodeError {
    fn from(err: anyhow::Error) -> NodeError {
        NodeError::ExecutionError(err.to_string())
    }
}
