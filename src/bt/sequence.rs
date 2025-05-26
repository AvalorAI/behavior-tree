use anyhow::Result;
use futures::future::select_all;
use futures::{Future, FutureExt};

use std::pin::Pin;
use tokio::sync::broadcast::{channel, Receiver, Sender};

use super::handle::NodeError;
use crate::bt::handle::{ChildMessage, FutResponse, Node, NodeHandle, ParentMessage, Status};
use crate::bt::CHANNEL_SIZE;

// Simplify complex type
type FutVec = Vec<Pin<Box<dyn Future<Output = Result<FutResponse, NodeError>> + Send>>>;

// Prevent typo errors in booleans by using explicit types
pub struct Sequence {}

impl Sequence {
    pub fn new(children: Vec<NodeHandle>) -> NodeHandle {
        let name = String::from("default_sequence");
        SequenceProcess::new(children, name, false)
    }

    pub fn new_with_name<S: Into<String> + Clone>(
        name: S,
        children: Vec<NodeHandle>,
    ) -> NodeHandle {
        SequenceProcess::new(children, name.into(), false)
    }
}

pub struct BlockingSequence {}

impl BlockingSequence {
    pub fn new(children: Vec<NodeHandle>) -> NodeHandle {
        let name = String::from("default_sequence");
        SequenceProcess::new(children, name, true)
    }

    pub fn new_with_name<S: Into<String> + Clone>(
        name: S,
        children: Vec<NodeHandle>,
    ) -> NodeHandle {
        SequenceProcess::new(children, name.into(), true)
    }
}

pub struct SequenceProcess {
    name: String,
    children: Vec<NodeHandle>,
    tx: Sender<ParentMessage>,
    rx: Option<Receiver<ChildMessage>>,
    running_child: Option<usize>,
    status: Status,
    blocking: bool,
}

impl SequenceProcess {
    fn new(mut children: Vec<NodeHandle>, name: String, blocking: bool) -> NodeHandle {
        let (parent_tx, parent_rx) = channel(CHANNEL_SIZE);
        let (child_tx, child_rx) = channel(CHANNEL_SIZE);

        let child_names = children.iter().map(|x| x.name.clone()).collect();
        let child_ids = children.iter().map(|x| x.id.clone()).collect();
        let mut handles = vec![];
        for child in children.iter_mut() {
            handles.append(&mut child.take_handles());
        }
        let node = Self::_new(
            name.clone(),
            children,
            child_tx.clone(),
            Some(parent_rx),
            blocking,
        );
        tokio::spawn(Self::serve(node));

        NodeHandle::new(
            parent_tx,
            child_rx,
            "Sequence",
            name,
            child_names,
            child_ids,
            handles,
        )
    }

    fn _new(
        name: String,
        children: Vec<NodeHandle>,
        tx: Sender<ParentMessage>,
        rx: Option<Receiver<ChildMessage>>,
        blocking: bool,
    ) -> Self {
        Self {
            name,
            children,
            tx,
            rx,
            running_child: None,
            status: Status::Idle,
            blocking,
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
        log::debug!("Sequence {:?} - notify parent: {:?}", self.name, msg);
        self.tx.send(msg)?;
        Ok(())
    }

    fn notify_child(&mut self, child_index: usize, msg: ChildMessage) -> Result<(), NodeError> {
        log::debug!(
            "Sequence {:?} - notify child {:?}: {:?}",
            self.name,
            self.children[child_index].name,
            msg
        );
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
                if self.status.is_running() && !self.blocking {
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

    fn process_msg_from_child(
        &mut self,
        msg: ParentMessage,
        child_index: usize,
    ) -> Result<(), NodeError> {
        match msg {
            ParentMessage::RequestStart => {
                match self.status {
                    Status::Failure => self.notify_parent(ParentMessage::RequestStart)?,
                    Status::Idle => {} // When Idle or succesful, child nodes should never become active
                    Status::Success => {}
                    Status::Running => {} // A request start is never valid in a running sequence
                }
            }
            ParentMessage::Status(status) => {
                match status {
                    Status::Success => {
                        if self.status.is_running() {
                            if let Some(current_child_index) = self.running_child {
                                if child_index != current_child_index {
                                    // TODO make this into a result
                                    log::warn!("Received a succes message from a child that is not equal to the running child!")
                                }
                                if current_child_index < (self.children.len() - 1) {
                                    self.start_child(child_index + 1)?; // Start next in sequence
                                } else {
                                    self.update_status(Status::Success)?; // The sequence has completed
                                }
                            }
                        } else if self.status.is_idle() {
                            // This occurs when the sequence has been stopped, and is waiting for confirmation
                            if let Some(current_child_index) = self.running_child {
                                if current_child_index == (self.children.len() - 1) {
                                    self.update_status(Status::Success)?; // Confirm success to parent if the sequence is finished
                                } else {
                                    self.update_status(Status::Failure)?; // Else confirm failure to parent
                                }
                            } else {
                                self.update_status(Status::Failure)?; // Else confirm failure to parent
                            }
                        }
                    }
                    Status::Failure => self.update_status(Status::Failure)?,
                    Status::Idle => log::warn!("Unexpected idle status received from child node"),
                    Status::Running => {}
                }
            }
            ParentMessage::Poison(err) => return Err(err),
            ParentMessage::Killed => return Ok(()), // Killing is handled by the BT, not the hierarchy
        }
        Ok(())
    }

    async fn _serve(mut self) -> Result<(), NodeError> {
        let mut futures = self.extract_futures(); // To take ownership
        let rx = self.rx.take().expect("BT sequence rx must be set");
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

impl Node for SequenceProcess {
    async fn serve(self) {
        let poison_tx = self.tx.clone();
        let name = self.name.clone();
        let res = Self::_serve(self).await;

        log::debug!("Sequence {name:?} - exited with error: {res:?}");

        match res {
            Err(err) => match err {
                NodeError::KillError => {
                    // Notify the handles
                    if let Err(e) = poison_tx.send(ParentMessage::Killed) {
                        log::warn!("Sequence {name:?} - killing acknowledgement failed! {e:?}")
                    }
                }
                NodeError::PoisonError(e) => poison_parent(poison_tx, name, e), // Propagate error
                err => poison_parent(poison_tx, name, err.to_string()), // If any error in itself, poison parent
            },
            Ok(_) => {} // Should never occur
        }
    }
}

fn poison_parent(poison_tx: Sender<ParentMessage>, name: String, err: String) {
    log::debug!("Sequence {name:?} - poisoning parent");
    if let Err(e) = poison_tx.send(ParentMessage::Poison(NodeError::PoisonError(err))) {
        log::warn!("Sequence {name:?} - poisoning the parent failed! {e:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bt::action::mocking::MockAction;

    #[tokio::test]
    async fn test_seq_listen_childs() {
        // Setup
        let nr_of_actions = 5;
        let mut children = vec![];
        for i in 0..nr_of_actions {
            children.push(MockAction::new(i));
        }

        let (node_tx, _) = channel(CHANNEL_SIZE); // Only needed for construction, as a handle is not useful here
        let mut seq =
            SequenceProcess::_new(String::from("some name"), children, node_tx, None, false);

        // When
        let mut futures = seq.extract_futures();
        let mut result = vec![];
        for i in 0..nr_of_actions {
            seq.children[i as usize].send(ChildMessage::Start).unwrap(); // Start in order
            loop {
                let (response, _, rem_futures) = select_all(futures).await; // Listen out all actions
                futures = rem_futures;
                if let FutResponse::Child(child_index, msg, rx) = response.unwrap() {
                    futures.push(NodeHandle::run_listen(rx, child_index).boxed());
                    if let ParentMessage::Status(Status::Success) = msg {
                        result.push(child_index);
                        break; // Loop over messages received from active child, but only activate next child as soon as a succes is received
                    }
                } else {
                    panic!("Wrong response received!");
                }
            }
        }

        // Then all childs should succeed in order, as they are called in order
        let expected: Vec<usize> = (0..nr_of_actions as usize).collect();
        assert_eq!(result, expected);
    }
}
