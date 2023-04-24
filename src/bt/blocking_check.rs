use super::handle::{ChildMessage, Node, NodeError, NodeHandle, ParentMessage, Status};
use super::CHANNEL_SIZE;
use actify::Handle;
use anyhow::Result;
use async_trait::async_trait;
use std::fmt::Debug;
use tokio::sync::broadcast::{channel, Receiver, Sender};

pub struct BlockingCheck<V>
where
    V: Clone + Debug + Send + Sync + Clone + 'static,
{
    name: String,
    handle: Handle<V>,
    child: NodeHandle,
    tx: Sender<ParentMessage>,
    rx: Receiver<ChildMessage>,
    status: Status,
}

impl<V> BlockingCheck<V>
where
    V: Clone + Debug + Send + Sync + Clone + 'static,
{
    pub fn new<S: Into<String> + Clone>(name: S, handle: Handle<V>, mut child: NodeHandle) -> NodeHandle {
        let (node_tx, _) = channel(CHANNEL_SIZE);
        let (tx, node_rx) = channel(CHANNEL_SIZE);

        let child_name = child.name.clone();
        let child_id = child.id.clone();
        let handles = child.take_handles();
        let node = Self::_new(name.clone().into(), handle, child, node_tx.clone(), node_rx);
        tokio::spawn(Self::serve(node));

        NodeHandle::new(tx, node_tx, "Decorator", name, vec![child_name], vec![child_id], handles)
    }

    fn _new(
        name: String,
        handle: Handle<V>,
        child: NodeHandle,
        tx: Sender<ParentMessage>,
        rx: Receiver<ChildMessage>,
    ) -> Self {
        Self {
            name,
            handle,
            child,
            tx,
            rx,
            status: Status::Idle,
        }
    }

    async fn process_msg_from_child(&mut self, msg: ParentMessage) -> Result<(), NodeError> {
        match msg {
            ParentMessage::RequestStart => {
                // TODO verify request starts cannot be received when idle, as that would block listening to the parent
                self.evaluate().await?; // Blocks until message received
                match self.status {
                    Status::Failure => self.notify_parent(ParentMessage::RequestStart)?,
                    Status::Idle => {} // When Idle or succesful, child nodes should never become active
                    Status::Success => {}
                    Status::Running => {} // If a blocking check is running, a child condition can request a start // TODO is this true?
                }
            }
            ParentMessage::Status(status) => match status {
                Status::Success => self.update_status(Status::Success)?,
                Status::Failure => self.update_status(Status::Failure)?,
                Status::Idle => log::warn!("Unexpected idle status received from child node"),
                Status::Running => {} // Running status has no effect
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
        log::debug!("BlockingCheck {:?} - notify parent: {:?}", self.name, msg);
        self.tx.send(msg)?;
        Ok(())
    }

    fn notify_child(&mut self, msg: ChildMessage) -> Result<(), NodeError> {
        log::debug!("BlockingCheck {:?} - notify child {:?}: {:?}", self.name, self.child.name, msg);
        self.child.send(msg)?;
        Ok(())
    }

    async fn process_msg_from_parent(&mut self, msg: ChildMessage) -> Result<(), NodeError> {
        match msg {
            ChildMessage::Start => self.start_workflow().await?,
            ChildMessage::Stop => {
                let status = self.stop_workflow().await?;
                self.update_status(status)?;
            }
            ChildMessage::Kill => return Err(NodeError::KillError),
        }
        Ok(())
    }

    async fn start_workflow(&mut self) -> Result<(), NodeError> {
        if !self.status.is_running() {
            self.update_status(Status::Running)?; // Send running to parent
            self.evaluate().await?; // Exits if value is set
            self.notify_child(ChildMessage::Start)?; // Start child
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
                    ParentMessage::Status(Status::Success) => return Ok(Status::Success),
                    ParentMessage::Poison(err) => return Err(err),
                    _ => log::warn!("Invalid message received from child when stopping"),
                }
            }
        }
        Ok(Status::Failure) // Default failure to parent to prevent blocking
    }

    async fn evaluate(&self) -> Result<(), NodeError> {
        let mut cache = self.handle.create_initialized_cache().await?;
        match cache.listen().await {
            // A value is means it is set and can continue
            Ok(_) => Ok(()),
            // Any other error is propagated
            Err(e) => Err(NodeError::ExecutionError(e.to_string())),
        }
    }

    async fn _serve(mut self) -> Result<(), NodeError> {
        loop {
            tokio::select! {
                Ok(msg) = self.rx.recv() => self.process_msg_from_parent(msg).await?,
                Ok(msg) = self.child.listen() => self.process_msg_from_child(msg).await?,
                else => log::warn!("Only invalid messages received"),
            };
        }
    }
}

#[async_trait]
impl<V> Node for BlockingCheck<V>
where
    V: Clone + Debug + Send + Sync + Clone + 'static,
{
    async fn serve(mut self) {
        let poison_tx = self.tx.clone();
        let name = self.name.clone();
        let res = Self::_serve(self).await;

        log::debug!("BlockingCheck {name:?} exited with error: {res:?}");

        match res {
            Err(err) => match err {
                NodeError::KillError => {
                    // Notify the handles
                    if let Err(e) = poison_tx.send(ParentMessage::Killed) {
                        log::warn!("BlockingCheck {name:?} - killing acknowledgement failed! {e:?}")
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
    log::debug!("BlockingCheck {name:?} - poisoning parent");
    if let Err(e) = poison_tx.send(ParentMessage::Poison(NodeError::PoisonError(err))) {
        log::warn!("BlockingCheck {name:?} - poisoning the parent failed! {e:?}")
    }
}
