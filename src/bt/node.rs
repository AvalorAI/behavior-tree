use anyhow::{Result};
use async_trait::async_trait;
use simple_xml_builder::XMLElement;
use tokio::sync::broadcast::{Receiver, Sender};
use thiserror::Error;
use actor_model::ActorError;

#[derive(Debug)]
pub struct NodeHandle {
    pub element: String,
    pub name: String,
    pub children_names: Vec<String>,
    tx: Sender<ChildMessage>, // This handle is held by a parent, so it can send child messages
    rx: Receiver<ParentMessage>, // The parent can receive messages from its child, so can listen to the handle for messages
    tx_child: Sender<ParentMessage>, // It holds a clone of the sender of the child, so it can call subscribe for new receivers when cloning the struct
}

impl Clone for NodeHandle {
    fn clone(&self) -> NodeHandle {
        Self {
            tx: self.tx.clone(),
            rx: self.tx_child.subscribe(),
            tx_child: self.tx_child.clone(),
            element: self.element.clone(),
            name: self.name.clone(),
            children_names: self.children_names.clone(),
        }
    }
}

impl NodeHandle {
    pub fn new<S, T>(
        tx: Sender<ChildMessage>,
        tx_child: Sender<ParentMessage>,
        element: S,
        name: T,
        children_names: Vec<String>,
    ) -> NodeHandle
    where
        S: Into<String>,
        T: Into<String>,
    {
        Self {
            tx,
            rx: tx_child.subscribe(),
            tx_child,
            element: element.into(),
            name: name.into(),
            children_names,
        }
    }

    pub async fn kill(&mut self) -> Result<()> {
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
                "name": self.name.clone(),
                "type": self.element.clone(),
                "children": self.children_names.clone()})
        } else {
            serde_json::json!({ 
                "name": self.name.clone(),
                "type": self.element.clone()})
        }
    }
}

pub enum FutResponse {
    Parent(ChildMessage, Receiver<ChildMessage>),
    Child(usize, ParentMessage, Receiver<ParentMessage>),
}

#[async_trait]
pub trait Node: Sync + Send {
    async fn serve(mut self);

    // Consuming and returning the receiver allows stacking it in a future vector
    async fn run_listen_parent(mut rx: Receiver<ChildMessage>) -> Result<FutResponse, NodeError> {
        let res = Self::_listen_parent(&mut rx).await;
        let msg = res.unwrap();
        Ok(FutResponse::Parent(msg, rx))
    }

    async fn _listen_parent(rx: &mut Receiver<ChildMessage>) -> Result<ChildMessage, NodeError> {
        // TODO add some timeout?
        Ok(rx.recv().await?)
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
    Kill,  // (oneshot::Sender<Result<()>>)
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
    #[error("Tokio sender error")]
    TokioSendError,
    #[error("Actor Error")]
    ActorError(#[from] ActorError),
}
