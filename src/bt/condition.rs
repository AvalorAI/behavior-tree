use actor_model::{ActorError, Handle};
use anyhow::Result;
use async_trait::async_trait;
use std::fmt::Debug;
use std::marker::PhantomData;
use tokio::sync::{
    broadcast::{channel, Receiver, Sender},
    mpsc, oneshot,
};

use super::handle::{ChildMessage, Node, NodeError, NodeHandle, ParentMessage, Status};
use super::CHANNEL_SIZE;

// Any custom (async) evaluator can be made with this trait
#[async_trait]
pub trait Evaluator<V> {
    fn get_name(&self) -> String;

    async fn evaluate(&self, val: V) -> Result<bool>;
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

    async fn evaluate(&self, val: V) -> Result<bool> {
        Ok((self.function)(val))
    }
}

pub struct Condition {}

impl Condition {
    pub fn new_from<V, T>(evaluator: T, handle: Handle<V>, child: NodeHandle) -> NodeHandle
    where
        T: Evaluator<V> + Clone + Send + Sync + 'static,
        V: Clone + Debug + Send + Sync + Clone + 'static,
    {
        ConditionProcess::new(handle, evaluator, child)
    }

    pub fn new<V, S, F>(name: S, handle: Handle<V>, function: F, child: NodeHandle) -> NodeHandle
    where
        S: Into<String> + Clone,
        F: Fn(V) -> bool + Sync + Send + Clone + 'static,
        V: Clone + Debug + Send + Sync + Clone + 'static,
    {
        let evaluator = ClosureEvaluator::new(name.into(), function);
        ConditionProcess::new(handle, evaluator, child)
    }
}

struct ConditionProcess<V, T>
where
    T: Evaluator<V> + Clone + Send + Sync + 'static,
{
    handle: Handle<V>,
    child: NodeHandle,
    tx: Sender<ParentMessage>,
    rx: Receiver<ChildMessage>,
    rx_expand: mpsc::Receiver<oneshot::Sender<Vec<NodeHandle>>>,
    status: Status,
    evaluator: T,
}

impl<V, T> ConditionProcess<V, T>
where
    T: Evaluator<V> + Clone + Send + Sync + 'static,
    V: Clone + Debug + Send + Sync + Clone + 'static,
{
    pub fn new(handle: Handle<V>, evaluator: T, child: NodeHandle) -> NodeHandle {
        let (node_tx, _) = channel(CHANNEL_SIZE);
        let (tx, node_rx) = channel(CHANNEL_SIZE);
        let (tx_prior, rx_expand) = mpsc::channel(CHANNEL_SIZE);

        let child_name = child.name.clone();
        let node = Self::_new(
            evaluator.clone(),
            handle,
            child,
            node_tx.clone(),
            node_rx,
            rx_expand,
        );
        tokio::spawn(Self::serve(node));

        NodeHandle::new(
            tx_prior,
            tx,
            node_tx,
            "Condition",
            evaluator.get_name(),
            vec![child_name],
        )
    }

    fn _new(
        evaluator: T,
        handle: Handle<V>,
        child: NodeHandle,
        tx: Sender<ParentMessage>,
        rx: Receiver<ChildMessage>,
        rx_expand: mpsc::Receiver<oneshot::Sender<Vec<NodeHandle>>>,
    ) -> Self {
        Self {
            handle,
            evaluator,
            child,
            tx,
            rx,
            rx_expand,
            status: Status::Idle,
        }
    }

    async fn process_msg_from_child(&mut self, msg: ParentMessage) -> Result<(), NodeError> {
        match msg {
            ParentMessage::RequestStart => {
                if self.evaluate_now().await? {
                    match self.status {
                        Status::Failure => self.notify_parent(ParentMessage::RequestStart)?,
                        Status::Idle => {} // When Idle or succesful, child nodes should never become active
                        Status::Succes => {}
                        Status::Running => log::warn!(
                            "Condition is running while the child is making a start request"
                        ),
                    }
                }
            }
            ParentMessage::Status(status) => match status {
                Status::Succes => self.update_status(Status::Succes)?,
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
        self.tx
            .send(msg)
            .map_err(|e| NodeError::TokioBroadcastSendError(e.to_string()))?;
        Ok(())
    }

    fn notify_child(&mut self, msg: ChildMessage) -> Result<(), NodeError> {
        log::debug!(
            "Condition {:?} - notify child {:?}: {:?}",
            self.evaluator.get_name(),
            self.child.name,
            msg
        );
        self.child.send(msg)?;
        Ok(())
    }

    async fn process_incoming_val(&mut self, val: V) -> Result<(), NodeError> {
        // If its running but the function evaluates to false, then fail
        // If it failed but function evaluates to true, then request start
        if self.status.is_running() && !self.run_evaluator(val.clone()).await? {
            let status = self.stop_workflow().await?;
            self.update_status(status)?;
        } else if self.status.is_failure() && self.run_evaluator(val).await? {
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
                    self.notify_child(ChildMessage::Start)?; // Start child
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

    async fn run_evaluator(&self, val: V) -> Result<bool, NodeError> {
        match self.evaluator.evaluate(val).await {
            Ok(res) => Ok(res),
            Err(e) => Err(NodeError::ExecutionError(e.to_string())),
        }
    }

    async fn evaluate_now(&self) -> Result<bool, NodeError> {
        match self.handle.get().await {
            // A value is evaluated by the function
            Ok(val) => self.run_evaluator(val).await,
            Err(ActorError::NoValueSet(_)) => Ok(false), // No value always evaluates to false
            Err(e) => Err(e.into()),
        }
    }

    async fn expand_tree(
        &mut self,
        sender: oneshot::Sender<Vec<NodeHandle>>,
    ) -> Result<(), NodeError> {
        log::debug!("Condition {:?} expanding tree", self.evaluator.get_name());
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
        let mut val_rx = self.handle.subscribe().await?;
        loop {
            tokio::select! {
                Ok(msg) = self.rx.recv() => self.process_msg_from_parent(msg).await?,
                Ok(msg) = self.child.listen() => self.process_msg_from_child(msg).await?,
                Ok(val) = val_rx.recv() => self.process_incoming_val(val).await?,
                Some(msg) = self.rx_expand.recv() => self.expand_tree(msg).await?,
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

        async fn evaluate(&self, _val: i32) -> Result<bool> {
            self.some_async_function().await;
            Ok(true)
        }
    }
}
