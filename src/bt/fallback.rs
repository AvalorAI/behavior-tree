use anyhow::Result;
use async_trait::async_trait;
use futures::future::select_all;
use futures::{Future, FutureExt};
use std::mem;
use std::pin::Pin;
use tokio::sync::broadcast::{channel, Receiver, Sender};

use crate::bt::handle::{ChildMessage, FutResponse, Node, NodeHandle, ParentMessage, Status};
use crate::bt::CHANNEL_SIZE;

use super::handle::NodeError;

// Simplify complex type
type FutVec = Vec<Pin<Box<dyn Future<Output = Result<FutResponse, NodeError>> + Send>>>;

pub struct Fallback {
    name: String,
    children: Vec<NodeHandle>,
    tx: Sender<ParentMessage>,
    rx: Option<Receiver<ChildMessage>>,
    running_child: Option<usize>,
    prio_child_on_hold: Option<usize>,
    status: Status,
}

impl Fallback {
    pub fn new(children: Vec<NodeHandle>) -> NodeHandle {
        let name = String::from("default_fallback");
        Self::_init(children, name)
    }

    pub fn new_with_name<S: Into<String> + Clone>(name: S, children: Vec<NodeHandle>) -> NodeHandle {
        Self::_init(children, name.into())
    }

    pub fn _init(mut children: Vec<NodeHandle>, name: String) -> NodeHandle {
        let (node_tx, _) = channel(CHANNEL_SIZE);
        let (tx, node_rx) = channel(CHANNEL_SIZE);

        let child_names = children.iter().map(|x| x.name.clone()).collect();
        let child_ids = children.iter().map(|x| x.id.clone()).collect();
        let mut handles = vec![];
        for child in children.iter_mut() {
            handles.append(&mut child.take_handles());
        }
        let node = Self::_new(name.clone(), children, node_tx.clone(), Some(node_rx));
        tokio::spawn(Self::serve(node));

        NodeHandle::new(tx, node_tx, "Fallback", name, child_names, child_ids, handles)
    }

    fn _new(
        name: String,
        children: Vec<NodeHandle>,
        tx: Sender<ParentMessage>,
        rx: Option<Receiver<ChildMessage>>,
    ) -> Self {
        Self {
            name,
            children,
            tx,
            rx,
            running_child: None,
            prio_child_on_hold: None,
            status: Status::Idle,
        }
    }

    fn extract_futures(&mut self) -> FutVec {
        let mut futures = vec![];
        for (child_index, child) in self.children.iter_mut().enumerate() {
            let rx = child.get_rx();
            futures.push(NodeHandle::run_listen(rx, child_index).boxed());
        }
        futures
    }

    fn update_status(&mut self, status: Status) -> Result<(), NodeError> {
        self.status = status.clone();
        self.notify_parent(ParentMessage::Status(status))?;
        Ok(())
    }

    fn notify_parent(&mut self, msg: ParentMessage) -> Result<(), NodeError> {
        log::debug!("Fallback {:?} - notify parent: {:?}", self.name, msg);
        self.tx
            .send(msg)
            .map_err(|e| NodeError::TokioBroadcastSendError(e.to_string()))?;
        Ok(())
    }

    fn notify_child(&mut self, child_index: usize, msg: ChildMessage) -> Result<(), NodeError> {
        log::debug!("Fallback {:?} - notify child {:?}: {:?}", self.name, self.children[child_index].name, msg);
        self.children[child_index].send(msg)?;
        Ok(())
    }

    fn start_child(&mut self, child_index: usize) -> Result<(), NodeError> {
        self.running_child = Some(child_index);
        self.notify_child(child_index, ChildMessage::Start)?;
        Ok(())
    }

    fn process_msg_from_parent(&mut self, msg: ChildMessage) -> Result<(), NodeError> {
        match msg {
            ChildMessage::Start => {
                if !self.status.is_running() {
                    self.update_status(Status::Running)?;
                    self.start_child(0)?;
                }
            }
            ChildMessage::Stop => {
                if self.status.is_running() {
                    if let Some(child_index) = self.running_child {
                        self.status = Status::Idle;
                        self.notify_child(child_index, ChildMessage::Stop)?;
                    }
                }
            }
            ChildMessage::Kill => return Err(NodeError::KillError),
        }
        Ok(())
    }

    fn process_msg_from_child(&mut self, msg: ParentMessage, child_index: usize) -> Result<(), NodeError> {
        match msg {
            ParentMessage::RequestStart => {
                match self.status {
                    Status::Failure => self.notify_parent(ParentMessage::RequestStart)?,
                    Status::Idle => {} // When Idle or succesful, child nodes should never become active
                    Status::Success => {}
                    Status::Running => {
                        if let Some(current_child_index) = self.running_child {
                            if child_index <= current_child_index {
                                self.notify_child(current_child_index, ChildMessage::Stop)?; // Stop current child
                                self.prio_child_on_hold = Some(child_index); // Set new child on hold until current child failes
                            }
                        }
                    }
                }
            }
            ParentMessage::Status(status) => match status {
                Status::Success => self.update_status(Status::Success)?,
                Status::Failure => {
                    if self.status.is_running() {
                        if let Some(child_index) = self.prio_child_on_hold {
                            self.start_child(child_index)?; // Start the previously failed but prio child
                            self.prio_child_on_hold = None; // Clear the waiting child
                        } else if let Some(child_index) = self.running_child {
                            if child_index < (self.children.len() - 1) {
                                self.start_child(child_index + 1)?; // Start next in sequence
                            } else {
                                self.update_status(Status::Failure)?; // The sequence has completed
                            }
                        }
                    } else if self.status.is_idle() {
                        // This occurs when the fallback has been stopped, and is waiting for confirmation
                        self.update_status(Status::Failure)?; // Confirm failure to parent
                    }
                }
                Status::Idle => log::warn!("Unexpected idle status received from child node"),
                Status::Running => {}
            },
            ParentMessage::Poison(err) => return Err(err),
            ParentMessage::Killed => return Ok(()), // Killing is handled by the BT, not the hierarchy
        }
        Ok(())
    }

    async fn _serve(mut self) -> Result<(), NodeError> {
        let mut futures = self.extract_futures(); // To take ownership
        let rx = mem::replace(&mut self.rx, None).expect("BT sequence rx must be set");
        futures.push(Self::run_listen_parent(rx).boxed());

        loop {
            let (response, _, rem_futures) = select_all(futures).await; // Listen out all actions
            futures = rem_futures;
            match response? {
                FutResponse::Parent(msg, rx) => {
                    self.process_msg_from_parent(msg)?;
                    futures.push(Self::run_listen_parent(rx).boxed());
                }
                FutResponse::Child(child_index, msg, rx) => {
                    self.process_msg_from_child(msg, child_index)?;
                    futures.push(NodeHandle::run_listen(rx, child_index).boxed());
                }
            }
        }
    }
}

#[async_trait]
impl Node for Fallback {
    async fn serve(mut self) {
        let poison_tx = self.tx.clone();
        let name = self.name.clone();
        let res = Self::_serve(self).await;

        log::debug!("Fallback {name:?} - exited with error: {res:?}");

        match res {
            Err(err) => match err {
                NodeError::KillError => {
                    // Notify the handles
                    if let Err(e) = poison_tx.send(ParentMessage::Killed) {
                        log::warn!("Fallback {name:?} - killing acknowledgement failed! {e:?}")
                    }
                }
                NodeError::PoisonError(e) => poison_parent(poison_tx, name, e), // Propagate error
                err => poison_parent(poison_tx, name, err.to_string()),         // If any error in itself, poison parent
            },
            Ok(_) => {} // Should never occur
        }
    }
}

fn poison_parent(poison_tx: Sender<ParentMessage>, name: String, err: String) {
    log::debug!("Fallback {name:?} - poisoning parent");
    if let Err(e) = poison_tx.send(ParentMessage::Poison(NodeError::PoisonError(err))) {
        log::warn!("Fallback {name:?} - poisoning the parent failed! {e:?}")
    }
}
