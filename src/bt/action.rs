use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{
    broadcast::{channel, Receiver, Sender},
    mpsc, oneshot,
};
use tokio::time::{sleep, Duration};

use super::handle::{ChildMessage, Node, NodeError, NodeHandle, ParentMessage, Status};
use super::CHANNEL_SIZE;

#[async_trait]
pub trait Executor {
    fn get_name(&self) -> String;

    async fn execute(&self) -> Result<bool>;
}

// Prevent typo errors in booleans by using explicit types
pub struct Action {}

impl Action {
    pub fn new<T>(inner: T) -> NodeHandle
    where
        T: Executor + Send + Sync + 'static,
    {
        ActionProcess::new(inner, false)
    }
}

pub struct BlockingAction {}

impl BlockingAction {
    pub fn new<T>(inner: T) -> NodeHandle
    where
        T: Executor + Send + Sync + 'static,
    {
        ActionProcess::new(inner, true)
    }
}

struct ActionProcess<T>
where
    T: Executor + Send + Sync + 'static,
{
    tx: Sender<ParentMessage>,
    rx: Receiver<ChildMessage>,
    rx_expand: mpsc::Receiver<oneshot::Sender<Vec<NodeHandle>>>,
    blocking: bool,
    status: Status,
    inner: T,
}

impl<T> ActionProcess<T>
where
    T: Executor + Send + Sync + 'static,
{
    pub fn new(inner: T, blocking: bool) -> NodeHandle {
        let (node_tx, _) = channel(CHANNEL_SIZE);
        let (tx, node_rx) = channel(CHANNEL_SIZE);
        let (tx_prior, rx_expand) = mpsc::channel(CHANNEL_SIZE);

        let name = inner.get_name();
        let node = Self::_new(node_tx.clone(), node_rx, rx_expand, inner, blocking);
        tokio::spawn(Self::serve(node));

        NodeHandle::new(tx_prior, tx, node_tx, "Action", name, vec![], vec![])
    }

    fn _new(
        tx: Sender<ParentMessage>,
        rx: Receiver<ChildMessage>,
        rx_expand: mpsc::Receiver<oneshot::Sender<Vec<NodeHandle>>>,
        inner: T,
        blocking: bool,
    ) -> Self {
        Self {
            tx,
            rx,
            rx_expand,
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
        if !self.blocking || !self.status.is_running() {
            match msg {
                ChildMessage::Start => self.update_status(Status::Running).await?,
                ChildMessage::Stop => {
                    if !self.blocking {
                        self.update_status(Status::Failure).await?
                    }
                }
                ChildMessage::Kill => return Err(NodeError::KillError),
            }
        }
        Ok(())
    }

    async fn expand_tree(
        &mut self,
        sender: oneshot::Sender<Vec<NodeHandle>>,
    ) -> Result<(), NodeError> {
        log::debug!("Action {:?} expanding tree", self.inner.get_name());
        let child_handles = vec![];
        sender
            .send(child_handles)
            .map_err(|_| NodeError::TokioBroadcastSendError("The oneshot failed".to_string()))?;
        Ok(())
    }

    async fn execute(inner: &T, is_running: bool) -> Result<bool, NodeError> {
        if is_running {
            Ok(inner
                .execute()
                .await
                .map_err(|e| NodeError::ExecutionError(e.to_string()))?)
        } else {
            loop {
                sleep(Duration::from_secs(10)).await // Sleep until execution started by parent
            }
        }
    }

    async fn _serve(mut self) -> Result<(), NodeError> {
        loop {
            tokio::select! {
                Ok(msg) =  self.rx.recv() => self.process_msg_from_parent(msg).await?,
                res = ActionProcess::execute(&self.inner, self.status.is_running()) => match res.map_err(|e| NodeError::ExecutionError(e.to_string()))? {
                    true => self.update_status(Status::Succes).await?,
                    false => self.update_status(Status::Failure).await?
                },
                Some(msg) = self.rx_expand.recv() => self.expand_tree(msg).await?,
                else => log::warn!("Only invalid messages received"),
            }
        }
    }
}

#[async_trait]
impl<T: Executor + Send + Sync + 'static> Node for ActionProcess<T> {
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

/*
Waiting is already pre-implemented for convenience
*/

pub struct Wait {
    name: String,
    secs: u64,
}

impl Wait {
    pub fn new(secs: u64) -> NodeHandle {
        Action::new(Self::_new(secs))
    }

    fn _new(secs: u64) -> Self {
        Self {
            name: "Waiting".to_string(),
            secs,
        }
    }
}

#[async_trait]
impl Executor for Wait {
    fn get_name(&self) -> String {
        self.name.clone()
    }

    async fn execute(&self) -> Result<bool> {
        sleep(Duration::from_secs(self.secs)).await;

        Ok(true)
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

    use super::{Action, BlockingAction, Executor};
    use crate::bt::handle::NodeHandle;

    pub struct MockAction {
        name: String,
        succeed: bool,
    }

    impl MockAction {
        pub fn new(id: i32) -> NodeHandle {
            Action::new(Self::_new(id, true))
        }

        pub fn new_failing(id: i32) -> NodeHandle {
            Action::new(Self::_new(id, false))
        }

        fn _new(id: i32, succeed: bool) -> Self {
            Self {
                name: id.to_string(),
                succeed,
            }
        }
    }

    #[async_trait]
    impl Executor for MockAction {
        fn get_name(&self) -> String {
            self.name.clone()
        }

        async fn execute(&self) -> Result<bool> {
            sleep(Duration::from_millis(500)).await;
            Ok(self.succeed)
        }
    }

    // Same for Mock blocking, which cannot be stopped during execution

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
    impl Executor for MockBlockingAction {
        fn get_name(&self) -> String {
            self.name.clone()
        }

        async fn execute(&self) -> Result<bool> {
            sleep(Duration::from_millis(500)).await;
            Ok(true)
        }
    }
}
