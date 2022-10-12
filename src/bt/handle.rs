use anyhow::{Result};
use async_trait::async_trait;
use simple_xml_builder::XMLElement;
use tokio::sync::{broadcast::{Receiver, Sender}, oneshot, mpsc};
use thiserror::Error;
use actor_model::ActorError;
use rand::{thread_rng, Rng};
use rand::distributions::Alphanumeric;

#[derive(Debug)]
pub struct NodeHandle {
    pub element: String,
    pub name: String,
    pub id: String,
    pub children_names: Vec<String>,
    pub children_ids: Vec<String>,
    tx_prior: mpsc::Sender<oneshot::Sender<Vec<NodeHandle>>>, // This handle is used to expand the tree to a vector 
    tx: Sender<ChildMessage>, // This handle is held by a parent, so it can send child messages
    rx: Receiver<ParentMessage>, // The parent can receive messages from its child, so can listen to the handle for messages
    tx_child: Sender<ParentMessage>, // It holds a clone of the sender of the child, so it can call subscribe for new receivers when cloning the struct
}

impl Clone for NodeHandle {
    fn clone(&self) -> NodeHandle {
        Self {
            rx: self.tx_child.subscribe(), // An rx cannot be cloned, but it can be created by subscribing to the transmitter
            tx_prior: self.tx_prior.clone(),
            tx: self.tx.clone(),
            tx_child: self.tx_child.clone(),
            element: self.element.clone(),
            name: self.name.clone(),
            id: self.id.clone(),
            children_names: self.children_names.clone(),
            children_ids: self.children_ids.clone(),
        }
    }
}

impl NodeHandle {
    pub fn new<S, T>(
        tx_prior: mpsc::Sender<oneshot::Sender<Vec<NodeHandle>>>,
        tx: Sender<ChildMessage>,
        tx_child: Sender<ParentMessage>,
        element: S,
        name: T,
        children_names: Vec<String>,
        children_ids: Vec<String>
    ) -> NodeHandle
    where
        S: Into<String>,
        T: Into<String>,
    {
        Self {
            tx_prior,
            tx,
            rx: tx_child.subscribe(),
            tx_child,
            element: element.into(),
            name: name.into(),
            id: generate_id(),
            children_names,
            children_ids,
        }
    }

    pub(crate) async fn kill(&mut self) -> Result<()> {
        if self.tx.receiver_count() > 0 { // It already exited if no receiver is alive
            self.send(ChildMessage::Kill)?; 
            loop { 
                match self.listen().await? {
                    ParentMessage::Killed => return Ok(()),
                    _ => {}
                };
            }
        } else {
            log::debug!("{} {:?} already exited", self.element, self.name);
        }
        Ok(())  
    }

    pub fn send(&self, msg: ChildMessage) -> Result<(), NodeError> {
        self.tx.send(msg).map_err(|e| NodeError::TokioBroadcastSendError(e.to_string()))?;
        Ok(())
    }

    pub fn get_rx(&mut self) -> Receiver<ParentMessage> {
        self.tx_child.subscribe()
    }

    // Consuming and returning the receiver allows stacking it in a future vector
    pub async fn run_listen(mut rx: Receiver<ParentMessage>, child_index: usize) -> Result<FutResponse, NodeError> {
        let msg = NodeHandle::_listen(&mut rx).await?;
        Ok(FutResponse::Child(child_index, msg, rx)) // The rx is returned to ensure the channel is fully read
    }

    pub async fn listen(&mut self) -> Result<ParentMessage, NodeError> {
       NodeHandle::_listen(&mut self.rx).await
    }

    async fn _listen(rx: &mut Receiver<ParentMessage>) -> Result<ParentMessage, NodeError> {
        Ok(rx.recv().await?)
    }

    pub async fn append_childs(&self, mut handles: Vec<NodeHandle>) -> Result<Vec<NodeHandle>>{
        let (respond_to, get_result) = oneshot::channel();
        self.tx_prior.send(respond_to).await?; 
        let mut child_handles = get_result.await?;
        handles.append(&mut child_handles);
        Ok(handles)
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
                "children": self.children_names.clone()})
        } else {
            serde_json::json!({ 
                "id": self.id.clone(),
                "name": self.name.clone(),
                "type": self.element.clone()})
        }
    }
}

pub enum FutResponse {
    Parent(ChildMessage, Receiver<ChildMessage>),
    Child(usize, ParentMessage, Receiver<ParentMessage>),
    Expansion(oneshot::Sender<Vec<NodeHandle>>, mpsc::Receiver<oneshot::Sender<Vec<NodeHandle>>>)
}
pub fn generate_id() -> String {
    thread_rng()
    .sample_iter(&Alphanumeric)
    .take(15)
    .map(char::from)
    .collect()
}

#[async_trait]
pub trait Node: Sync + Send {
    async fn serve(mut self);

    // Consuming and returning the receiver allows stacking it in a future vector
    async fn run_listen_parent(mut rx: Receiver<ChildMessage>) -> Result<FutResponse, NodeError> {
        let msg = rx.recv().await?;
        Ok(FutResponse::Parent(msg, rx))
    }

    async fn run_listen_expansion(mut rx: mpsc::Receiver<oneshot::Sender<Vec<NodeHandle>>>) -> Result<FutResponse, NodeError> {
        let msg = rx.recv().await.ok_or(NodeError::TokioRecvError)?;
        Ok(FutResponse::Expansion(msg, rx))
    }

}

#[derive(PartialEq, Debug, Clone)]
pub enum Status {
    Succes,
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
        matches!(self, Status::Succes)
    }
}

#[derive(PartialEq, Debug, Clone)]
pub enum ChildMessage {
    Start,
    Stop,
    Kill, 
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
    #[error("Tokio mpsc receiver error")]
    TokioRecvError,
    #[error("Tokio sender error")]
    TokioSendError,
    #[error("Actor Error")]
    ActorError(#[from] ActorError),
}
