use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{
    broadcast::{channel, Receiver, Sender},
    mpsc, oneshot,
};
use tokio::time::{sleep, Duration};

use super::handle::{ChildMessage, Node, NodeError, NodeHandle, ParentMessage, Status};
use super::CHANNEL_SIZE;

pub struct LoopDecorator {
    name: String,
    child: NodeHandle,
    tx: Sender<ParentMessage>,
    rx: Receiver<ChildMessage>,
    rx_expand: mpsc::Receiver<oneshot::Sender<Vec<NodeHandle>>>,
    status: Status,
    pause_millis: u64,
}

impl LoopDecorator {
    pub fn new<S: Into<String> + Clone>(
        name: S,
        child: NodeHandle,
        pause_millis: u64,
    ) -> NodeHandle {
        let (node_tx, _) = channel(CHANNEL_SIZE);
        let (tx, node_rx) = channel(CHANNEL_SIZE);
        let (tx_prior, rx_expand) = mpsc::channel(CHANNEL_SIZE);

        let child_name = child.name.clone();
        let node = Self::_new(
            name.clone().into(),
            child,
            node_tx.clone(),
            node_rx,
            rx_expand,
            pause_millis,
        );
        tokio::spawn(Self::serve(node));

        NodeHandle::new(tx_prior, tx, node_tx, "Decorator", name, vec![child_name])
    }

    fn _new(
        name: String,
        child: NodeHandle,
        tx: Sender<ParentMessage>,
        rx: Receiver<ChildMessage>,
        rx_expand: mpsc::Receiver<oneshot::Sender<Vec<NodeHandle>>>,
        pause_millis: u64,
    ) -> Self {
        Self {
            name,
            child,
            tx,
            rx,
            rx_expand,
            status: Status::Idle,
            pause_millis,
        }
    }

    async fn process_msg_from_child(&mut self, msg: ParentMessage) -> Result<(), NodeError> {
        match msg {
            ParentMessage::RequestStart => {
                match self.status {
                    Status::Failure => self.notify_parent(ParentMessage::RequestStart)?,
                    Status::Idle => {} // When Idle or succesful, child nodes should never become active
                    Status::Succes => {}
                    Status::Running => {}
                }
            }
            ParentMessage::Status(status) => match status {
                // Start child again upon succes or failure // TODO make this configurable to a loop until succes/ failure
                Status::Succes => {
                    sleep(Duration::from_millis(self.pause_millis)).await;
                    self.notify_child(ChildMessage::Start)?
                }
                Status::Failure => {
                    sleep(Duration::from_millis(self.pause_millis)).await;
                    self.notify_child(ChildMessage::Start)?
                }
                Status::Running => {}
                Status::Idle => log::warn!("Unexpected idle status received from child node"),
            },
            ParentMessage::Poison(err) => return Err(err),
            ParentMessage::Killed => return Ok(()), // Killing is handled by the BT, not the hierarchy
        }
        Ok(())
    }

    fn update_status(&mut self, status: Status) -> Result<(), NodeError> {
        self.status = status.clone();
        self.notify_parent(ParentMessage::Status(status))?;
        Ok(())
    }

    fn notify_parent(&mut self, msg: ParentMessage) -> Result<(), NodeError> {
        log::debug!("Loop {:?} - notify parent: {:?}", self.name, msg);
        self.tx
            .send(msg)
            .map_err(|e| NodeError::TokioBroadcastSendError(e.to_string()))?;
        Ok(())
    }

    fn notify_child(&mut self, msg: ChildMessage) -> Result<(), NodeError> {
        log::debug!(
            "Loop {:?} - notify child {:?}: {:?}",
            self.name,
            self.child.name,
            msg
        );
        self.child.send(msg)?;
        Ok(())
    }

    async fn process_msg_from_parent(&mut self, msg: ChildMessage) -> Result<(), NodeError> {
        match msg {
            ChildMessage::Start => {
                if !self.status.is_running() {
                    self.update_status(Status::Running)?;
                    self.notify_child(ChildMessage::Start)?; // Start child
                }
            }
            ChildMessage::Stop => {
                let status = self.stop_workflow().await?;
                self.update_status(status)?;
            }
            ChildMessage::Kill => return Err(NodeError::KillError),
        }
        Ok(())
    }

    async fn stop_workflow(&mut self) -> Result<Status, NodeError> {
        // Only if its running it needs to block and await its child, otherwise fail immediately
        if self.status.is_running() {
            self.notify_child(ChildMessage::Stop)?; // Stop child

            // Wait until stopping confirmed
            loop {
                match self.child.listen().await? {
                    ParentMessage::Status(Status::Failure) => return Ok(Status::Failure),
                    ParentMessage::Status(Status::Succes) => return Ok(Status::Succes),
                    _ => log::warn!("Invalid message received from child when stopping"),
                }
            }
        }
        Ok(Status::Failure) // Default failure to parent to prevent blocking
    }

    async fn expand_tree(
        &mut self,
        sender: oneshot::Sender<Vec<NodeHandle>>,
    ) -> Result<(), NodeError> {
        log::debug!("Loop {:?} expanding tree", self.name.clone());
        let mut child_handles = self
            .child
            .append_childs(vec![])
            .await
            .map_err(|e| NodeError::TokioBroadcastSendError(e.to_string()))?;
        child_handles.append(&mut vec![self.child.clone()]);
        sender
            .send(child_handles)
            .map_err(|_| NodeError::TokioBroadcastSendError("The oneshot failed".to_string()))?;
        Ok(())
    }

    async fn _serve(mut self) -> Result<(), NodeError> {
        loop {
            tokio::select! {
                Ok(msg) = self.rx.recv() => self.process_msg_from_parent(msg).await?,
                Ok(msg) = self.child.listen() => self.process_msg_from_child(msg).await?,
                Some(msg) = self.rx_expand.recv() => self.expand_tree(msg).await?,
                else => log::warn!("Only invalid messages received"),
            };
        }
    }
}

#[async_trait]
impl Node for LoopDecorator {
    async fn serve(mut self) {
        let poison_tx = self.tx.clone();
        let name = self.name.clone();
        let res = Self::_serve(self).await;

        log::debug!("Loop {name:?} exited with error: {res:?}");

        match res {
            Err(err) => match err {
                NodeError::KillError => {
                    // Notify the handles
                    if let Err(e) = poison_tx.send(ParentMessage::Killed) {
                        log::warn!("Loop {name:?} - killing acknowledgement failed! {e:?}")
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
    log::debug!("Loop {name:?} - poisoning parent");
    if let Err(e) = poison_tx.send(ParentMessage::Poison(NodeError::PoisonError(err))) {
        log::warn!("Loop {name:?} - poisoning the parent failed! {e:?}")
    }
}
