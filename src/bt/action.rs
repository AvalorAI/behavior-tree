use anyhow::Result;
use async_trait::async_trait;
use std::{mem, vec};
use tokio::sync::broadcast::{channel, Receiver, Sender};

use super::node::{ChildMessage, Node, NodeError, NodeHandle, ParentMessage, Status};

#[async_trait]
pub trait ActionLogic {
    fn get_name(&self) -> String;

    async fn execute(&self) -> Result<bool>;
}

// Prevent typo errors in booleans by using explicit types
pub struct Action {}

impl Action {
    pub fn new<T>(inner: T) -> NodeHandle
    where
        T: ActionLogic + Send + Sync + 'static,
    {
        ActionProcess::new(inner, false)
    }
}

pub struct BlockingAction {}

impl BlockingAction {
    pub fn new<T>(inner: T) -> NodeHandle
    where
        T: ActionLogic + Send + Sync + 'static,
    {
        ActionProcess::new(inner, true)
    }
}

struct ActionProcess<T>
where
    T: ActionLogic + Send + Sync + 'static,
{
    tx: Sender<ParentMessage>,
    rx: Option<Receiver<ChildMessage>>,
    blocking: bool,
    status: Status,
    inner: T,
}

impl<T> ActionProcess<T>
where
    T: ActionLogic + Send + Sync + 'static,
{
    pub fn new(inner: T, blocking: bool) -> NodeHandle {
        let (node_tx, _) = channel(100);
        let (tx, node_rx) = channel(100);

        let name = inner.get_name();
        let node = Self::_new(node_tx.clone(), node_rx, inner, blocking);
        tokio::spawn(Self::serve(node));

        NodeHandle::new(tx, node_tx, "Action", name, vec![])
    }

    fn _new(
        tx: Sender<ParentMessage>,
        rx: Receiver<ChildMessage>,
        inner: T,
        blocking: bool,
    ) -> Self {
        Self {
            tx,
            rx: Some(rx),
            blocking,
            status: Status::Idle,
            inner,
        }
    }

    async fn update_status(&mut self, status: Status) -> Result<(), NodeError> {
        self.status = status.clone();
        self.notify_parent(ParentMessage::Status(status)).await?;
        Ok(())
    }

    async fn notify_parent(&mut self, msg: ParentMessage) -> Result<(), NodeError> {
        log::debug!(
            "Action {:?} - notify parent: {:?}",
            self.inner.get_name(),
            msg
        );
        self.tx
            .send(msg)
            .map_err(|e| NodeError::TokioBroadcastSendError(e.to_string()))?;
        Ok(())
    }

    async fn process_msg_from_parent(&mut self, msg: ChildMessage) -> Result<(), NodeError> {
        match msg {
            ChildMessage::Start => self.update_status(Status::Running).await?,
            ChildMessage::Stop => {
                if !self.blocking {
                    self.update_status(Status::Failure).await?
                }
            }
            ChildMessage::Kill => return Err(NodeError::KillError),
        }
        Ok(())
    }

    async fn listen_for_parent_msg(&self, rx: &mut Receiver<ChildMessage>) -> Option<ChildMessage> {
        while let Ok(msg) = rx.recv().await {
            if !self.blocking || !self.status.is_running() {
                return Some(msg); // If it needs to be stopped immediately, pass each message directly
            }
        }
        None
    }

    async fn _serve(mut self) -> Result<(), NodeError> {
        let mut rx = mem::replace(&mut self.rx, None).unwrap(); // To take ownership
        loop {
            if self.status.is_running() {
                tokio::select! {
                    Some(msg) = self.listen_for_parent_msg(&mut rx) => self.process_msg_from_parent(msg).await?,
                    res = self.inner.execute() => match res.map_err(|e| NodeError::ExecutionError(e.to_string()))? {
                        true => self.update_status(Status::Succes).await?,
                        false => self.update_status(Status::Failure).await?
                    },
                    else => log::warn!("Only invalid messages received"),
                }
            } else if let Ok(msg) = rx.recv().await {
                self.process_msg_from_parent(msg).await?
            }
        }
    }
}

#[async_trait]
impl<T: ActionLogic + Send + Sync + 'static> Node for ActionProcess<T> {
    async fn serve(mut self) {
        let poison_tx = self.tx.clone();
        let name = self.inner.get_name();
        let res = Self::_serve(self).await;

        log::debug!("Action {name:?} - exited with error: {res:?}");

        match res {
            Err(err) => match err {
                NodeError::KillError => {
                    // Notify the handles
                    if let Err(e) = poison_tx.send(ParentMessage::Killed) {
                        log::warn!("Action {name:?} - killing acknowledgement failed! {e:?}")
                    }
                }
                err => {
                    // Else poison the parent
                    log::debug!("Action {name:?} - poisoning parent");
                    if let Err(e) = poison_tx.send(ParentMessage::Poison(NodeError::PoisonError(
                        err.to_string(),
                    ))) {
                        log::warn!("Action {name:?} - poisoning the parent failed! {e:?}")
                    }
                }
            },
            Ok(_) => {} // Should never occur
        }
    }
}

#[cfg(test)]
pub(crate) mod mocking {
    /*
    The Mock action is intended to completely mock all logic of a normal action, but does not execute anything complex.
    */
    use anyhow::Result;
    use async_trait::async_trait;
    use tokio::time::{sleep, Duration};

    use super::{Action, ActionLogic, BlockingAction};
    use crate::bt::node::NodeHandle;

    pub struct MockAction {
        name: String,
    }

    impl MockAction {
        pub fn new(id: i32) -> NodeHandle {
            Action::new(Self::_new(id))
        }

        fn _new(id: i32) -> Self {
            Self {
                name: id.to_string(),
            }
        }
    }

    #[async_trait]
    impl ActionLogic for MockAction {
        fn get_name(&self) -> String {
            self.name.clone()
        }

        async fn execute(&self) -> Result<bool> {
            sleep(Duration::from_millis(500)).await;
            Ok(true)
        }
    }

    // Same for mock blocking

    pub struct MockBlockingAction {
        name: String,
    }

    impl MockBlockingAction {
        pub fn new(id: i32) -> NodeHandle {
            BlockingAction::new(Self::_new(id))
        }

        fn _new(id: i32) -> Self {
            Self {
                name: id.to_string(),
            }
        }
    }

    #[async_trait]
    impl ActionLogic for MockBlockingAction {
        fn get_name(&self) -> String {
            self.name.clone()
        }

        async fn execute(&self) -> Result<bool> {
            sleep(Duration::from_millis(500)).await;
            Ok(true)
        }
    }
}
