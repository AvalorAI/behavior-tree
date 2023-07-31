use actify::{ActorError, Handle};
use anyhow::Result;
use async_trait::async_trait;
use std::fmt::Debug;
use std::marker::PhantomData;
use tokio::sync::broadcast::{channel, Receiver, Sender};
use tokio::time::{sleep, Duration};

use super::handle::{ChildMessage, Node, NodeError, NodeHandle, ParentMessage, Status};
use super::CHANNEL_SIZE;

// Any custom (async) evaluator can be made with this trait
#[async_trait]
pub trait Evaluator<V> {
    fn get_name(&self) -> String;

    async fn evaluate(&mut self, val: V) -> Result<bool>;
}

// If you pass in just a sync closure to condition::new(), this hidden wrapper is used beneath
#[derive(Clone)]
pub struct ClosureEvaluator<V, F>
where
    F: Fn(V) -> bool + Clone,
{
    name: String,
    function: F,
    phantom: PhantomData<V>,
}

impl<V, F> ClosureEvaluator<V, F>
where
    V: Clone + Debug + Send + Sync + Clone + 'static,
    F: Fn(V) -> bool + Sync + Send + Clone + 'static,
{
    pub fn new(name: String, function: F) -> ClosureEvaluator<V, F> {
        Self {
            name,
            function,
            phantom: PhantomData,
        }
    }
}

#[async_trait]
impl<V, F> Evaluator<V> for ClosureEvaluator<V, F>
where
    V: Clone + Debug + Send + Sync + Clone + 'static,
    F: Fn(V) -> bool + Sync + Send + Clone + 'static,
{
    fn get_name(&self) -> String {
        self.name.clone()
    }

    async fn evaluate(&mut self, val: V) -> Result<bool> {
        Ok((self.function)(val))
    }
}

pub struct OneTimeCondition {}

impl OneTimeCondition {
    pub fn new_from<V, T>(evaluator: T, handle: Handle<V>) -> NodeHandle
    where
        T: Evaluator<V> + Clone + Send + Sync + 'static,
        V: Clone + Debug + Send + Sync + Clone + 'static,
    {
        ConditionProcess::new(handle, evaluator, None)
    }

    pub fn new<V, S, F>(name: S, handle: Handle<V>, function: F) -> NodeHandle
    where
        S: Into<String> + Clone,
        F: Fn(V) -> bool + Sync + Send + Clone + 'static,
        V: Clone + Debug + Send + Sync + Clone + 'static,
    {
        let evaluator = ClosureEvaluator::new(name.into(), function);
        ConditionProcess::new(handle, evaluator, None)
    }
}

pub struct Condition {}

impl Condition {
    pub fn new_from<V, T>(evaluator: T, handle: Handle<V>, child: NodeHandle) -> NodeHandle
    where
        T: Evaluator<V> + Clone + Send + Sync + 'static,
        V: Clone + Debug + Send + Sync + Clone + 'static,
    {
        ConditionProcess::new(handle, evaluator, Some(child))
    }

    pub fn new<V, S, F>(name: S, handle: Handle<V>, function: F, child: NodeHandle) -> NodeHandle
    where
        S: Into<String> + Clone,
        F: Fn(V) -> bool + Sync + Send + Clone + 'static,
        V: Clone + Debug + Send + Sync + Clone + 'static,
    {
        let evaluator = ClosureEvaluator::new(name.into(), function);
        ConditionProcess::new(handle, evaluator, Some(child))
    }
}

struct ConditionProcess<V, T>
where
    T: Evaluator<V> + Clone + Send + Sync + 'static,
{
    handle: Handle<V>,
    child: Option<NodeHandle>,
    tx: Sender<ParentMessage>,
    rx: Receiver<ChildMessage>,
    status: Status,
    evaluator: T,
    prev_evaluation: bool,
}

impl<V, T> ConditionProcess<V, T>
where
    T: Evaluator<V> + Clone + Send + Sync + 'static,
    V: Clone + Debug + Send + Sync + Clone + 'static,
{
    pub fn new(handle: Handle<V>, evaluator: T, child: Option<NodeHandle>) -> NodeHandle {
        let (node_tx, _) = channel(CHANNEL_SIZE);
        let (tx, node_rx) = channel(CHANNEL_SIZE);

        let child_name = child.clone().map(|x| x.name.clone());
        let child_id = child.clone().map(|x| x.id.clone());
        let handles = child.clone().map(|mut x| x.take_handles());
        let node = Self::_new(evaluator.clone(), handle, child, node_tx.clone(), node_rx);
        tokio::spawn(Self::serve(node));

        NodeHandle::new(
            tx,
            node_tx,
            "Condition",
            evaluator.get_name(),
            child_name.map_or(vec![], |x| vec![x]),
            child_id.map_or(vec![], |x| vec![x]),
            handles.map_or(vec![], |x| x),
        )
    }

    fn _new(
        evaluator: T,
        handle: Handle<V>,
        child: Option<NodeHandle>,
        tx: Sender<ParentMessage>,
        rx: Receiver<ChildMessage>,
    ) -> Self {
        Self {
            handle,
            evaluator,
            child,
            tx,
            rx,
            status: Status::Idle,
            prev_evaluation: false,
        }
    }

    async fn process_msg_from_child(&mut self, msg: ParentMessage) -> Result<(), NodeError> {
        match msg {
            ParentMessage::RequestStart => {
                if self.evaluate_now().await? {
                    match self.status {
                        Status::Failure => self.notify_parent(ParentMessage::RequestStart)?,
                        _ => {} // In other cases the request start should be ignored
                    }
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
        log::debug!(
            "Condition {:?} - notify parent: {:?}",
            self.evaluator.get_name(),
            msg
        );
        self.tx.send(msg)?;
        Ok(())
    }

    fn notify_child(&mut self, msg: ChildMessage) -> Result<(), NodeError> {
        if let Some(child) = &self.child {
            log::debug!(
                "Condition {:?} - notify child {:?}: {:?}",
                self.evaluator.get_name(),
                child.name,
                msg
            );
            child.send(msg)?;
        }
        Ok(())
    }

    async fn process_incoming_val(&mut self, val: Result<V, ActorError>) -> Result<(), NodeError> {
        // If its running but the function evaluates to false, then fail
        // If it failed but function evaluates to true, then request start

        // Skip errors
        let val = match val {
            Err(e) => {
                log::debug!("{e:?}");
                return Ok(());
            }
            Ok(v) => v,
        };

        if self.status.is_running() && !self.run_evaluator(val.clone()).await? {
            let status = self.stop_workflow().await?;
            self.update_status(status)?;
        } else if self.status.is_failure()
            && !self.prev_evaluation
            && self.run_evaluator(val).await?
        {
            // Only when a value has changed compared to the previous evaluation a request start is necessary
            self.notify_parent(ParentMessage::RequestStart)?
        }
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
            match self.evaluate_now().await? {
                true => {
                    self.update_status(Status::Running)?; // Send running to parent
                    if self.child.is_some() {
                        self.notify_child(ChildMessage::Start)?; // Start child
                    } else {
                        self.update_status(Status::Success)?; // Without a child a condition succeeds immediately
                    }
                }
                false => self.update_status(Status::Failure)?, // Send failure to parent
            }
        }
        Ok(())
    }

    async fn stop_workflow(&mut self) -> Result<Status, NodeError> {
        // Only if its running it needs to block and await its child, otherwise fail immediately
        if self.status.is_running() {
            self.notify_child(ChildMessage::Stop)?; // Stop child

            if let Some(child) = &mut self.child {
                // Wait until stopping confirmed
                loop {
                    match child.listen().await? {
                        // A returned status can be both succes and failure, even when stopping a child
                        ParentMessage::Status(Status::Failure) => return Ok(Status::Failure),
                        ParentMessage::Status(Status::Success) => return Ok(Status::Success),
                        ParentMessage::Poison(err) => return Err(err),
                        _ => log::warn!("Invalid message received from child when stopping"),
                    }
                }
            } else {
                return Ok(Status::Failure); // When no child is present the condition failes directly
            }
        }
        Ok(Status::Failure) // Default failure to parent to prevent blocking
    }

    async fn run_evaluator(&mut self, val: V) -> Result<bool, NodeError> {
        match self.evaluator.evaluate(val).await {
            Ok(res) => {
                self.prev_evaluation = res;
                Ok(res)
            }
            Err(e) => Err(NodeError::ExecutionError(e.to_string())),
        }
    }

    async fn evaluate_now(&mut self) -> Result<bool, NodeError> {
        match self.handle.get().await {
            // A value is evaluated by the function
            Ok(val) => self.run_evaluator(val).await,
            Err(e) => Err(e.into()),
        }
    }

    async fn listen_to_child(child: &mut Option<NodeHandle>) -> Result<ParentMessage, NodeError> {
        if let Some(child) = child {
            child.listen().await
        } else {
            loop {
                sleep(Duration::from_secs(10)).await; // wait until mavlink values received}
            }
        }
    }

    async fn _serve(mut self) -> Result<(), NodeError> {
        let mut cache = self.handle.create_initialized_cache().await?;
        loop {
            tokio::select! {
                Ok(msg) = self.rx.recv() => self.process_msg_from_parent(msg).await?,
                Ok(msg) = ConditionProcess::<V, T>::listen_to_child(&mut self.child) => self.process_msg_from_child(msg).await?,
                val = cache.listen_newest() => self.process_incoming_val(val).await?,
                else => log::warn!("Only invalid messages received"),
            };
        }
    }
}

#[async_trait]
impl<V, T> Node for ConditionProcess<V, T>
where
    T: Evaluator<V> + Clone + Send + Sync + 'static,
    V: Clone + Debug + Send + Sync + Clone + 'static,
{
    async fn serve(mut self) {
        let poison_tx = self.tx.clone();
        let name = self.evaluator.get_name();
        let res = Self::_serve(self).await;

        log::debug!("Condition {name:?} exited with error: {res:?}");

        match res {
            Err(err) => match err {
                NodeError::KillError => {
                    // Notify the handles
                    if let Err(e) = poison_tx.send(ParentMessage::Killed) {
                        log::warn!("Condition {name:?} - killing acknowledgement failed! {e:?}")
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
    log::debug!("Condition {name:?} - poisoning parent");
    if let Err(e) = poison_tx.send(ParentMessage::Poison(NodeError::PoisonError(err))) {
        log::warn!("Condition {name:?} - poisoning the parent failed! {e:?}")
    }
}

#[cfg(test)]
pub(crate) mod mocking {
    // A mock condition to showcase the usage of some arbitrary async work in the condition evaluation

    use anyhow::Result;
    use async_trait::async_trait;
    use tokio::time::{sleep, Duration};

    use super::Evaluator;

    #[derive(Clone, Debug)]
    pub struct MockAsyncCondition {
        name: String,
    }

    impl MockAsyncCondition {
        pub fn new() -> Self {
            Self {
                name: "Mock async condition".to_string(),
            }
        }

        async fn some_async_function(&self) {
            sleep(Duration::from_millis(500)).await;
        }
    }

    #[async_trait]
    impl Evaluator<i32> for MockAsyncCondition {
        fn get_name(&self) -> String {
            self.name.clone()
        }

        async fn evaluate(&mut self, _val: i32) -> Result<bool> {
            self.some_async_function().await;
            Ok(true)
        }
    }
}
