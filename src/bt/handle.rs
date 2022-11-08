use anyhow::{Result};
use async_trait::async_trait;
use simple_xml_builder::XMLElement;
use tokio::sync::{broadcast::{Receiver, Sender, error::SendError}};
use thiserror::Error;
use actor_model::ActorError;
use std::mem;
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
    tx_child: Sender<ParentMessage>, // It holds a clone of the sender of the child, so it can call subscribe for new receivers when cloning the struct
}

impl Clone for NodeHandle {
    fn clone(&self) -> NodeHandle {
        Self {
            rx: self.tx_child.subscribe(), // An rx cannot be cloned, but it can be created by subscribing to the transmitter
            tx: self.tx.clone(),
            tx_child: self.tx_child.clone(),
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
        tx_child: Sender<ParentMessage>,
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
            rx: tx_child.subscribe(),
            tx_child,
            element: element.into(),
            name: name.into(),
            id: Uuid::now_v1(&[1, 2, 3, 4, 5, 6]).to_string(),
            handles,
            children_names,
            children_ids,
        }
    }

    pub(crate) async fn kill(&mut self) -> Result<()> {
        if self.tx.receiver_count() > 0 { // It already exited if no receiver is alive
            self.send(ChildMessage::Kill)?; 
            loop { 
                match self.listen().await {
                    Ok(ParentMessage::Killed) => return Ok(()),
                    Err(e) => log::debug!("Channel error received {e:?}"),
                    _ => {} // Some other message that can be discarded
                };
            }
        } else {
            log::debug!("{} {:?} already exited", self.element, self.name);
        }
        Ok(())  
    }

    pub fn take_handles(&mut self) -> Vec<NodeHandle>{
        let mut handles = mem::replace(&mut self.handles, vec![]);
        handles.push(self.clone());
        handles
    }

    pub fn send(&self, msg: ChildMessage) -> Result<(), NodeError> {
        self.tx.send(msg)?;
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

    #[cfg(test)]
    pub fn get_child_tx(&mut self) -> Sender<ParentMessage> {
        self.tx_child.clone()
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
    #[error("Actor Error")]
    ActorError(#[from] ActorError),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bt::{action::mocking::MockAction, CHANNEL_SIZE}};
    use tokio::time::{sleep, Duration};
  
    #[tokio::test]
    async fn test_killing_nodes() {
        let mut action = MockAction::new(1);
        let action_tx = action.get_child_tx();

        // Channel sizes double in size (starting from 8 --> 16 --> 32), so ensure that it always lags behind at least one message
        for _ in 0..2*CHANNEL_SIZE{
            // Ensure that even with overflown channels the killing is succesful
            action_tx.send(ParentMessage::RequestStart).unwrap();
        }
        sleep(Duration::from_millis(1000)).await;
        let res = action.kill().await;
        assert!(res.is_ok());

    }
}