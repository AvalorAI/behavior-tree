use std::future::Future;

use anyhow::Result;

use tokio::sync::broadcast::{channel, Receiver, Sender};
use tokio::time::{sleep, Duration};

use super::handle::{ChildMessage, Node, NodeError, NodeHandle, ParentMessage, Status};
use super::CHANNEL_SIZE;

pub trait Executor {
    fn get_name(&self) -> String;
    fn execute(&mut self) -> impl Future<Output = Result<bool>> + Send;
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
    rx: Option<Receiver<ChildMessage>>,
    blocking: bool,
    status: Status,
    inner: T,
}

impl<T> ActionProcess<T>
where
    T: Executor + Send + Sync + 'static,
{
    pub fn new(inner: T, blocking: bool) -> NodeHandle {
        let (parent_tx, parent_rx) = channel(CHANNEL_SIZE);
        let (child_tx, child_rx) = channel(CHANNEL_SIZE);

        let name = inner.get_name();
        let node = Self::_new(parent_tx.clone(), child_rx, inner, blocking);
        tokio::spawn(Self::serve(node));

        NodeHandle::new(child_tx, parent_rx, "Action", name, vec![], vec![], vec![])
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
        self.tx.send(msg)?;
        Ok(())
    }

    async fn process_msg_from_parent(&mut self, msg: ChildMessage) -> Result<(), NodeError> {
        if msg.is_kill() {
            return Err(NodeError::KillError);
        }

        if self.status.is_running() && self.blocking {
            return Ok(()); // This should never happen as it should not even process the message?
        }

        match msg {
            ChildMessage::Start => self.update_status(Status::Running).await?,
            ChildMessage::Stop => {
                if !self.blocking {
                    // This should never happen as it should not even process the message?
                    self.update_status(Status::Failure).await?
                }
            }
            _ => {}
        }

        Ok(())
    }

    async fn execute(inner: &mut T, is_running: bool) -> Result<bool, NodeError> {
        if is_running {
            Ok(inner.execute().await?)
        } else {
            loop {
                sleep(Duration::from_secs(10)).await // Sleep until execution started by parent
            }
        }
    }

    async fn listen_for_parent_msg(
        is_blocking: bool,
        is_running: bool,
        rx: &mut Receiver<ChildMessage>,
    ) -> Option<ChildMessage> {
        while let Ok(msg) = rx.recv().await {
            if is_running && is_blocking && !msg.is_kill() {
                continue;
            } else {
                return Some(msg); // If it needs to be stopped immediately, pass each message directly
            }
        }
        None
    }

    async fn _serve(mut self) -> Result<(), NodeError> {
        let mut rx = self.rx.take().unwrap(); // To take ownership
        loop {
            tokio::select! {
                Some(msg) =  ActionProcess::<T>::listen_for_parent_msg(self.blocking, self.status.is_running(), &mut rx) => self.process_msg_from_parent(msg).await?,
                res = ActionProcess::execute(&mut self.inner, self.status.is_running()) => match res.map_err(|e| NodeError::ExecutionError(e.to_string()))? {
                    true => self.update_status(Status::Success).await?,
                    false => self.update_status(Status::Failure).await?
                },
                else => log::warn!("Only invalid messages received"),
            }
        }
    }
}

impl<T: Executor + Send + Sync + 'static> Node for ActionProcess<T> {
    async fn serve(self) {
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
Some convenience actions are pre-implemented
*/

pub struct Wait {
    name: String,
    duration: Duration,
}

impl Wait {
    pub fn new(duration: Duration) -> NodeHandle {
        Action::new(Self {
            name: "Waiting".to_string(),
            duration,
        })
    }
}

impl Executor for Wait {
    fn get_name(&self) -> String {
        self.name.clone()
    }

    async fn execute(&mut self) -> Result<bool> {
        sleep(self.duration).await;
        Ok(true)
    }
}

pub struct Success {
    name: String,
}

impl Success {
    pub fn new() -> NodeHandle {
        Action::new(Self {
            name: "SUCCESS".to_string(),
        })
    }
}

impl Executor for Success {
    fn get_name(&self) -> String {
        self.name.clone()
    }

    async fn execute(&mut self) -> Result<bool> {
        Ok(true)
    }
}

pub struct Failure {
    name: String,
}

impl Failure {
    pub fn new() -> NodeHandle {
        Action::new(Self {
            name: "FAILURE".to_string(),
        })
    }
}

impl Executor for Failure {
    fn get_name(&self) -> String {
        self.name.clone()
    }

    async fn execute(&mut self) -> Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
pub(crate) mod mocking {

    use anyhow::{anyhow, Result};
    use tokio::time::{sleep, Duration};

    use super::{Action, BlockingAction, Executor};
    use crate::bt::handle::NodeHandle;

    // The Mock action is intended to completely mock all logic of a normal action, but does not execute anything complex.
    pub struct MockAction {
        name: String,
        calls: i32,
        succeed: bool,
        throw_error: bool,
        fail_on_twice: bool,
        keep_looping: bool,
    }

    #[allow(dead_code)]
    impl MockAction {
        pub fn new(id: i32) -> NodeHandle {
            Action::new(Self::_new(id, true, false, false, false))
        }

        pub fn new_loop(id: i32) -> NodeHandle {
            Action::new(Self::_new(id, true, false, false, true))
        }

        pub fn new_failing(id: i32) -> NodeHandle {
            Action::new(Self::_new(id, false, false, false, false))
        }

        pub fn fail_on_twice(id: i32) -> NodeHandle {
            Action::new(Self::_new(id, true, false, true, false))
        }

        pub fn new_error(id: i32) -> NodeHandle {
            Action::new(Self::_new(id, true, true, false, false))
        }

        fn _new(
            id: i32,
            succeed: bool,
            throw_error: bool,
            fail_on_twice: bool,
            keep_looping: bool,
        ) -> Self {
            Self {
                calls: 0,
                name: id.to_string(),
                succeed,
                throw_error,
                fail_on_twice,
                keep_looping,
            }
        }
    }

    impl Executor for MockAction {
        fn get_name(&self) -> String {
            self.name.clone()
        }

        async fn execute(&mut self) -> Result<bool> {
            self.calls += 1;

            loop {
                sleep(Duration::from_millis(500)).await;

                if !self.keep_looping {
                    break;
                }
            }

            if self.throw_error {
                Err(anyhow!("Some testing error!"))
            } else if self.fail_on_twice {
                Ok(self.calls < 2)
            } else {
                Ok(self.succeed)
            }
        }
    }

    // Same for Mock blocking, which cannot be stopped during execution
    pub struct MockBlockingAction {
        name: String,
        succeed: bool,
        throw_error: bool,
        keep_looping: bool,
    }

    #[allow(dead_code)]
    impl MockBlockingAction {
        pub fn new(id: i32) -> NodeHandle {
            BlockingAction::new(Self::_new(id, true, false, false))
        }

        pub fn new_loop(id: i32) -> NodeHandle {
            BlockingAction::new(Self::_new(id, true, false, true))
        }

        pub fn new_failing(id: i32) -> NodeHandle {
            BlockingAction::new(Self::_new(id, false, false, false))
        }

        pub fn new_error(id: i32) -> NodeHandle {
            BlockingAction::new(Self::_new(id, true, true, false))
        }

        fn _new(id: i32, succeed: bool, throw_error: bool, keep_looping: bool) -> Self {
            Self {
                name: id.to_string(),
                succeed,
                throw_error,
                keep_looping,
            }
        }
    }

    impl Executor for MockBlockingAction {
        fn get_name(&self) -> String {
            self.name.clone()
        }

        async fn execute(&mut self) -> Result<bool> {
            loop {
                sleep(Duration::from_millis(500)).await;

                if !self.keep_looping {
                    break;
                }
            }
            if self.throw_error {
                Err(anyhow!("Some testing error!"))
            } else {
                Ok(self.succeed)
            }
        }
    }

    // Run one fails if called multiple times
    pub struct MockRunBlockingOnce {
        name: String,
        called: bool,
    }

    impl MockRunBlockingOnce {
        pub fn new(id: i32) -> NodeHandle {
            BlockingAction::new(Self::_new(id))
        }

        fn _new(id: i32) -> Self {
            Self {
                name: id.to_string(),
                called: false,
            }
        }
    }

    impl Executor for MockRunBlockingOnce {
        fn get_name(&self) -> String {
            self.name.clone()
        }

        async fn execute(&mut self) -> Result<bool> {
            if self.called {
                Ok(false) // Return when already called
            } else {
                self.called = true;
                sleep(Duration::from_millis(500)).await;
                Ok(true)
            }
        }
    }
}
