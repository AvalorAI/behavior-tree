use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use simple_xml_builder::XMLElement;
use std::fs::File;
use std::mem;
use tokio::{
    sync::mpsc::{channel, Receiver, Sender},
    time::{sleep, Duration},
};

use handle::{ChildMessage, NodeError, NodeHandle, ParentMessage, Status};
use listener::{Listener, Update};

#[cfg(feature = "websocket")]
use crate::ws::socket_connector::SocketConnector;

const CHANNEL_SIZE: usize = 20;
const BT_LOOP_TIME: u64 = 1000; // [ms]

pub mod action;
pub mod blocking_check;
pub mod condition;
pub mod fallback;
pub mod handle;
pub mod listener;
pub mod loop_dec;
pub mod sequence;

pub struct BehaviorTree {
    pub name: String,
    root_node: NodeHandle,
    handles: Vec<NodeHandle>,
    tx: Sender<Update>,
    rx: Option<Receiver<Update>>,
}

impl BehaviorTree {
    pub fn new<S: Into<String>>(root_node: NodeHandle, name: S) -> Self {
        BehaviorTree::_new(root_node, name.into())
    }

    #[cfg(test)]
    fn new_test(root_node: NodeHandle) -> Self {
        BehaviorTree::_new(root_node, "test".to_string())
    }

    fn _new(mut root_node: NodeHandle, name: String) -> Self {
        let handles = root_node.take_handles();
        // Non-unique UUIDs should not be possible, but check anyway
        BehaviorTree::verify_unique_ids(&handles).expect("UUIDs are non-unique!");
        let (tx, rx) = channel(CHANNEL_SIZE);
        Self {
            name,
            root_node,
            handles,
            tx,
            rx: Some(rx),
        }
    }

    // Mutually exclusive public access to take rx.
    // Taking the rx is only allowed when handled externally, as opposed to the interal ws feature
    #[cfg(not(feature = "websocket"))]
    pub fn take_rx(&mut self) -> Result<Receiver<Update>> {
        self._take_rx()
    }

    #[cfg(feature = "websocket")]
    fn take_rx(&mut self) -> Result<Receiver<Update>> {
        self._take_rx()
    }

    fn _take_rx(&mut self) -> Result<Receiver<Update>> {
        mem::replace(&mut self.rx, None).ok_or(anyhow!("Receiver already taken"))
    }

    // The connect function automatically send the BT once as soon as connection established
    // When connected, the writer forwards any updates directly
    #[cfg(feature = "websocket")]
    pub fn connect<S: Into<String>>(&mut self, socket_url: S) -> Result<()> {
        let rx = self.take_rx()?;
        let bt_export = self.export_json(self.name.clone())?;
        SocketConnector::spawn(socket_url.into(), rx, bt_export)?;
        Ok(())
    }

    // Run continuously
    pub async fn run(&mut self) -> Result<String> {
        log::debug!("Starting BT from {:?}", self.root_node.name);
        let mut listener = Listener::new(self.handles.clone(), self.tx.clone());

        let res = tokio::select! {
            res = listener.run_listeners() => {res}
            res = self._run() => {res}
        };

        if let Err(e) = res {
            log::warn!("BT crashed - {:?} ", e);
            self.kill().await?;
            return Ok(e.to_string());
        }
        Ok("".to_string()) // This Ok should never fire, as _run can only exit through error propagation
    }

    async fn _run(&mut self) -> Result<()> {
        loop {
            let status = self.run_once().await?;
            log::debug!("Exited BT with status: {:?} - restarting again", status);
            sleep(Duration::from_millis(BT_LOOP_TIME)).await;
        }
    }

    pub async fn kill(&mut self) -> Result<()> {
        for handle in &mut self.handles {
            log::debug!("Killing {} {:?}", handle.element, handle.name);
            if let Err(e) = handle.kill().await {
                log::debug!("Killing {:?} failed: {e:?}", handle.name);
                return Err(anyhow!("Cannot safely rebuild behaviour tree with active nodes: {e:?}"));
            };
        }
        log::debug!("Killed all nodes succesfully");
        Ok(())
    }

    fn verify_unique_ids(handles: &Vec<NodeHandle>) -> Result<()> {
        let mut ids = vec![];
        for handle in handles {
            ids.push(handle.id.clone());
        }
        let original_len = ids.len();
        ids.dedup();
        if ids.len() < original_len {
            Err(anyhow!("The behavior tree contained non-unique IDs"))
        } else {
            Ok(())
        }
    }

    async fn run_once(&mut self) -> Result<Status, NodeError> {
        self.root_node.send(ChildMessage::Start)?;
        loop {
            match self.root_node.listen().await? {
                ParentMessage::Status(status) => match status {
                    Status::Success => return Ok(status),
                    Status::Failure => return Ok(status),
                    _ => {}
                },
                ParentMessage::RequestStart => {
                    log::debug!("Invalid status start request received from the root node")
                }
                ParentMessage::Poison(err) => return Err(err),
                ParentMessage::Killed => return Err(NodeError::KillError), // This should not occur
            }
        }
    }

    pub fn save_xml_export<S: Into<String> + Clone>(&mut self, name: S) -> Result<()> {
        let file = File::create(format!("{}.xml", name.clone().into()))?;
        let root = self.export_xml(name)?;
        root.write(file)?;
        Ok(())
    }

    pub fn export_xml<S: Into<String> + Clone>(&mut self, name: S) -> Result<XMLElement> {
        // Groot format. See https://github.com/BehaviorTree/Groot
        let mut root = XMLElement::new("root");
        root.add_attribute("main_tree_to_execute", "MainTree");
        let mut tree = XMLElement::new(name.into());
        tree.add_attribute("ID", "MainTree");

        // Start with root node
        let root_element = self.root_node.get_xml();
        let children_names = self.root_node.children_names.clone();
        let root_element = self.add_children(&self.handles, root_element, children_names);

        tree.add_child(root_element); // Insert custom BT logic
        root.add_child(tree); // Insert in boilerplate

        Ok(root)
    }

    fn add_children(
        &self,
        handles: &Vec<NodeHandle>,
        mut element: XMLElement,
        children_names: Vec<String>,
    ) -> XMLElement {
        for child in &children_names {
            let handle = handles
                .iter()
                .find_map(|x| if x.name == *child { Some(x.clone()) } else { None })
                .expect("A child was not present in the handles!");

            let el_base = handle.get_xml();
            let children_names = handle.children_names.clone();
            let el_expanded = self.add_children(handles, el_base, children_names);
            element.add_child(el_expanded)
        }
        element
    }

    pub fn save_json_export<S: Into<String> + Clone>(&mut self, name: S) -> Result<()> {
        let file = File::create(format!("{}.json", name.clone().into()))?;
        let bt = self.export_json(name)?;
        serde_json::to_writer(&file, &bt)?;
        Ok(())
    }

    pub fn export_json<S: Into<String> + Clone>(&mut self, name: S) -> Result<Value> {
        let node_description: Vec<serde_json::value::Value> = self.handles.iter().map(|x| x.get_json()).collect();

        let bt = json!({
            "name": name.into(),
            "rootNode": self.root_node.id.clone(),
            "nodes": json!(node_description),
        });

        Ok(bt)
    }
}

#[cfg(test)]
#[allow(unused_imports)]
mod tests {
    use actify::Handle;
    use std::collections::HashMap;

    use super::*;
    use super::{
        action::{Failure, Success},
        blocking_check::BlockingCheck,
        condition::{Condition, OneTimeCondition},
        fallback::Fallback,
        loop_dec::LoopDecorator,
        sequence::Sequence,
    };
    use crate::bt::action::mocking::{MockAction, MockBlockingAction, MockRunBlockingOnce};
    use crate::bt::condition::mocking::MockAsyncCondition;
    use crate::logging::load_logger;
    use crate::{BlockingFallback, BlockingSequence};
    use listener::OuterStatus;

    async fn dummy_bt() -> BehaviorTree {
        let handle = Handle::new_from(-1);
        let action1 = MockAction::new(1);
        let cond1 = Condition::new("cond1", handle.clone(), |i: i32| i > 0, action1);
        let seq = Sequence::new(vec![cond1]);
        let action2 = MockAction::new_failing(2);
        let fb = Fallback::new(vec![seq, action2]);
        BehaviorTree::new_test(fb)
    }

    #[tokio::test]
    #[ignore]
    async fn test_save_xml_export() {
        let mut bt = dummy_bt().await;
        let res = bt.save_xml_export("Test_BT");
        assert!(res.is_ok())
    }

    #[tokio::test]
    #[ignore]
    async fn test_save_json_export() {
        let mut bt = dummy_bt().await;
        let res = bt.save_json_export("Test_BT");
        assert!(res.is_ok())
    }

    #[tokio::test]
    async fn test_export_xml() {
        let mut bt = dummy_bt().await;
        let res = bt.export_xml("Test_BT");
        assert!(res.is_ok())
    }

    #[tokio::test]
    async fn test_export_json() {
        let mut bt = dummy_bt().await;
        let res = bt.export_json("Test_BT");
        assert!(res.is_ok())
    }

    #[cfg(feature = "websocket")]
    #[tokio::test]
    #[ignore]
    async fn test_websocket_connection() {
        let mut bt = dummy_bt().await;
        bt.connect(format!("ws://{}:{}", "localhost", 4012)).unwrap();
        sleep(Duration::from_secs(1)).await; // Allow for treabeard to receive messages
    }

    #[tokio::test]
    async fn test_killing_bt() {
        // Setup
        let handle = Handle::new_from(-1);

        // When
        let action = MockAction::new(1);
        let cond = Condition::new("1", handle.clone(), |i: i32| i > 0, action);
        let mut bt = BehaviorTree::new_test(cond);

        let timer = sleep(Duration::from_millis(200));
        tokio::pin!(timer);
        tokio::select! {
            _ = &mut timer => {None}
            res = bt.run() => {Some(res)}
        };

        sleep(Duration::from_millis(200)).await;
        bt.kill().await.unwrap();

        println!("Setting condition");
        handle.set(1).await.unwrap();
        sleep(Duration::from_millis(200)).await;

        // TODO some assert that the tree is not reacting to the value of the condition being changed?
    }

    //  Cond1
    //    |
    // Action1
    // Pass cond1, throw error in Action, cond1 handles error while in stop
    #[tokio::test]
    async fn test_poison_while_stopping() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = MockBlockingAction::new_error(1);
        let cond1 = Condition::new("1", handle.clone(), |i: i32| i > 0, action1);

        let mut bt = BehaviorTree::new_test(cond1);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(-1).await // Condition asks action to stop
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert!(matches!(res.unwrap_err(), NodeError::PoisonError(_)));
    }

    //      Fb
    //     /   \
    //   Seq  Action2
    //    |
    //  Cond1
    //    |
    // Action1
    //  Fail cond1, Start Action2, cond1 request start, seq success
    #[tokio::test]
    async fn test_sequence_request_start_while_failed() {
        // Setup
        let handle = Handle::new_from(-1);

        // When
        let action1 = MockAction::new(1);
        let cond1 = Condition::new("1", handle.clone(), |i: i32| i > 0, action1);
        let seq = Sequence::new(vec![cond1]);

        let action2 = MockAction::new_failing(2);
        let fb = Fallback::new(vec![seq, action2]);
        let mut bt = BehaviorTree::new_test(fb);
        assert_eq!(bt.handles.len(), 5);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Success);
    }

    //  Cond1
    //    |
    // Action1
    //
    // Cond1 passes, cond1 fails during Action1, BT still succesful
    #[tokio::test]
    async fn test_force_action_completion() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = MockBlockingAction::new(1);
        let cond1 = Condition::new("1", handle.clone(), |x| x > 0, action1);
        let mut bt = BehaviorTree::new_test(cond1);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(-1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Success);
    }

    //  Cond1
    //    |
    // Action1
    #[tokio::test]
    async fn test_auto_success() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = Success::new();
        let cond1 = Condition::new("1", handle.clone(), |x| x > 0, action1);
        let mut bt = BehaviorTree::new_test(cond1);

        // Then
        assert_eq!(bt.run_once().await.unwrap(), Status::Success);
    }

    //  Cond1
    //    |
    // Action1
    #[tokio::test]
    async fn test_auto_failure() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = Failure::new();
        let cond1 = Condition::new("1", handle.clone(), |x| x > 0, action1);
        let mut bt = BehaviorTree::new_test(cond1);

        // Then
        assert_eq!(bt.run_once().await.unwrap(), Status::Failure);
    }

    //  Cond1
    //    |
    // Action1
    //
    // Cond1 passes, cond1 fails during Action1, BT still succesful
    #[tokio::test]
    async fn test_prohibit_double_blocking_execution() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = MockRunBlockingOnce::new(1);
        let cond1 = Condition::new("1", handle.clone(), |x| x > 0, action1);
        let mut bt = BehaviorTree::new_test(cond1);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(-1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Success);
    }

    //  Cond1
    //    |
    // Action1
    #[tokio::test]
    async fn test_listen_rx() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = MockAction::new(1);
        let cond1 = Condition::new("1", handle.clone(), |x| x > 0, action1);
        let mut bt = BehaviorTree::new_test(cond1);
        bt.run_once().await.unwrap();

        // Note that because the whole tree is run, it should not be allowed to repeat
        tokio::select! {
            res = bt.run() => {res.unwrap();}
            _ = async {
                sleep(Duration::from_millis(1000)).await;
            } => {}
        };

        // Then
        let mut rx = bt.take_rx().unwrap();
        let goal_updates = vec![
            OuterStatus::Running,
            OuterStatus::Running,
            OuterStatus::Success,
            OuterStatus::Success,
        ];
        let mut all_updates = vec![];
        while let Ok(update) = rx.try_recv() {
            all_updates.push(update.status);
        }

        for (index, update) in all_updates.iter().enumerate() {
            assert_eq!(update, &goal_updates[index])
        }
    }

    //  Cond1
    //    |
    // Action1
    //
    // Blocking cond1 does not pass, tests terminates without reaching Action1
    #[tokio::test]
    async fn test_blocking_no_val_received() {
        // Setup
        let handle = Handle::<i32>::new();
        let timer = sleep(Duration::from_millis(1000));
        tokio::pin!(timer);

        // When
        let action1 = MockAction::new(1);
        let cond1 = BlockingCheck::new("1", handle.clone(), action1);
        let mut bt = BehaviorTree::new_test(cond1);

        let res = tokio::select! {
            _ = &mut timer => {None}
            res = bt.run_once() => {Some(res)}
        };

        // Then
        assert!(res.is_none());
    }

    //  Loop
    //    |
    // Action1
    //
    // Loop repeats, tests terminates without bt completion
    #[tokio::test]
    async fn test_loop_does_not_exit() {
        // Setup
        let timer = sleep(Duration::from_millis(1000));
        tokio::pin!(timer);

        // When
        let action1 = MockAction::new(1);
        let bt_loop = LoopDecorator::new("1", action1, 100);
        let mut bt = BehaviorTree::new_test(bt_loop);

        let res = tokio::select! {
            _ = &mut timer => {None}
            res = bt.run_once() => {Some(res)}
        };

        // Then
        assert!(res.is_none());
    }

    //  Cond1
    //    |
    //  Loop
    //    |
    // Action1
    //
    // Loop repeats, tests terminates without bt completion
    #[tokio::test]
    async fn test_loop_is_stopped() {
        // Setup
        let handle = Handle::<i32>::new_from(1);
        let timer = sleep(Duration::from_millis(1000));
        tokio::pin!(timer);

        // When
        let action1 = MockAction::new(1);
        let bt_loop = LoopDecorator::new("1", action1, 100);
        let cond1 = Condition::new("1", handle.clone(), |x| x > 0, bt_loop);
        let mut bt = BehaviorTree::new_test(cond1);
        assert_eq!(bt.handles.len(), 3);

        let res = tokio::select! {
            _ = &mut timer => {None}
            res = bt.run_once() => {Some(res)}
            _ = async {
                sleep(Duration::from_millis(200)).await;
                _ = handle.set(-1).await;
                sleep(Duration::from_millis(1000)).await;
            }  => {None}
        };

        // Then
        assert_eq!(res.unwrap().unwrap(), Status::Failure);
    }

    //  Cond1
    //    |
    // Action1
    //
    // Blocking cond1 does not pass, passes after time, bt terminates succesfully
    #[tokio::test]
    async fn test_blocking_some_val_received() {
        // Setup
        let handle = Handle::<i32>::new();
        let timer = sleep(Duration::from_millis(2000));
        tokio::pin!(timer);

        // When
        let action1 = MockAction::new(1);
        let cond1 = BlockingCheck::new("1", handle.clone(), action1);
        let mut bt = BehaviorTree::new_test(cond1);

        let res = tokio::select! {
            _ = &mut timer => {None}
            _ = async {
                sleep(Duration::from_millis(200)).await;
                handle.set(-1).await.unwrap();
                sleep(Duration::from_millis(1000)).await; // Allow timely execution of mock action
            } => {None}
            res = bt.run_once() => {Some(res)}
        };

        // Then
        assert_eq!(res.unwrap().unwrap(), Status::Success);
    }

    //  Cond1
    //    |
    // Action1
    //
    // Don't pas cond1
    #[tokio::test]
    async fn test_async_condition() {
        // Setup
        let handle: Handle<i32> = Handle::new_from(1);

        // When
        let action1 = MockAction::new(1);
        let cond1 = Condition::new_from(MockAsyncCondition::new(), handle, action1);
        let mut bt = BehaviorTree::new_test(cond1);

        // Then
        assert_eq!(bt.run_once().await.unwrap(), Status::Success);
    }

    //  Cond1
    //    |
    // Action1
    //
    // Don't pas cond1
    #[tokio::test]
    async fn test_if_val_exists() {
        // Setup
        let handle: Handle<i32> = Handle::new();

        // When
        let action1 = MockAction::new(1);
        let cond1 = Condition::new("1", handle, |_| true, action1);
        let mut bt = BehaviorTree::new_test(cond1);

        // Then
        assert_eq!(bt.run_once().await.unwrap(), Status::Failure);
    }

    //  Cond1
    //    |
    // Action1
    //
    // Don't pas cond1
    #[tokio::test]
    async fn test_if_vec_not_empty() {
        // Setup
        let handle: Handle<Vec<i32>> = Handle::new_from(vec![]);

        // When
        let action1 = MockAction::new(1);
        let cond1 = Condition::new("1", handle, |x| !x.is_empty(), action1);
        let mut bt = BehaviorTree::new_test(cond1);

        // Then
        assert_eq!(bt.run_once().await.unwrap(), Status::Failure);
    }

    //  Cond1
    //    |
    // Action1
    //
    // Don't pas cond1
    #[tokio::test]
    async fn test_if_map_not_empty() {
        // Setup
        let handle: Handle<HashMap<&str, i32>> = Handle::new_from(HashMap::new());

        // When
        let action1 = MockAction::new(1);
        let cond1 = Condition::new("1", handle, |x| !x.is_empty(), action1);
        let mut bt = BehaviorTree::new_test(cond1);

        // Then
        assert_eq!(bt.run_once().await.unwrap(), Status::Failure);
    }

    //      Seq
    //     /   \
    //  Cond1  Action2
    //    |
    // Action1
    //
    //  pass cond1, pass action1, pass action2, pass seq
    #[tokio::test]
    async fn test_simple_sequence() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = MockAction::new(1);
        let action2 = MockAction::new(2);
        let cond1 = Condition::new("1", handle, |i: i32| i > 0, action1);
        let seq = Sequence::new(vec![cond1, action2]);
        let mut bt = BehaviorTree::new_test(seq);
        assert_eq!(bt.handles.len(), 4);

        // Then
        assert_eq!(bt.run_once().await.unwrap(), Status::Success);
    }

    //      FB
    //     /   \
    //  Cond1  Action2
    //    |
    // Action1
    //
    // pass cond1, pass action1, pass fb
    #[tokio::test]
    async fn test_simple_fallback_plan_a() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = MockAction::new(1);
        let action2 = MockAction::new(2);
        let cond1 = Condition::new("1", handle, |i: i32| i > 0, action1);
        let fb = Fallback::new(vec![cond1, action2]);
        let mut bt = BehaviorTree::new_test(fb);
        assert_eq!(bt.handles.len(), 4);

        // Then
        assert_eq!(bt.run_once().await.unwrap(), Status::Success);
    }

    //     Cond1
    //       |
    //      Seq
    //     /   \
    //  Action1  Action2
    //
    // pass cond1, during action2 fail cond1, pass seq
    #[tokio::test]
    async fn test_finish_stopped_sequence_with_last_blocking_action() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = MockAction::new(1);
        let action2 = MockBlockingAction::new(2);
        let seq = Sequence::new(vec![action1, action2]);
        let cond1 = Condition::new("1", handle.clone(), |i: i32| i > 0, seq);
        let mut bt = BehaviorTree::new_test(cond1);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(600)).await;
            handle.set(-1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Success);
    }

    //     Cond1
    //       |
    //      Seq
    //     /   \
    //  Action1  Action2
    //
    // pass cond1, during action1 fail cond1, pass seq
    #[tokio::test]
    async fn test_blocking_sequence_finish_seq() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = MockAction::new(1);
        let action2 = MockAction::new(2);
        let seq = BlockingSequence::new(vec![action1, action2]);
        let cond1 = Condition::new("1", handle.clone(), |i: i32| i > 0, seq);
        let mut bt = BehaviorTree::new_test(cond1);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(-1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Success);
    }

    //     Cond1
    //       |
    //      Seq
    //     /   \
    //  Action1  Action2
    //
    // pass cond1, during action1 fail cond1, fail action2, fail seq
    #[tokio::test]
    async fn test_blocking_sequence_failure() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = MockAction::new_failing(1);
        let action2 = MockAction::new(2);
        let seq = BlockingSequence::new(vec![action1, action2]);
        let cond1 = Condition::new("1", handle.clone(), |i: i32| i > 0, seq);
        let mut bt = BehaviorTree::new_test(cond1);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(-1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Failure);
    }

    //     Cond1
    //       |
    //       Fb
    //     /   \
    //  Action1  Action2
    //
    // pass cond1, during action1 fail cond1, fail action1, pass fb
    #[tokio::test]
    async fn test_blocking_fallback_finish_fallback() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = MockAction::new_failing(1);
        let action2 = MockAction::new(2);
        let fb = BlockingFallback::new(vec![action1, action2]);
        let cond1 = Condition::new("1", handle.clone(), |i: i32| i > 0, fb);
        let mut bt = BehaviorTree::new_test(cond1);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(-1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Success);
    }

    //     Cond1
    //       |
    //      Fb
    //     /   \
    //  Action1  Action2
    //
    // pass cond1, during action1 fail cond1, succeed action1, pass fb
    #[tokio::test]
    async fn test_blocking_fallback_early_success() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = MockAction::new(1);
        let action2 = MockAction::new_failing(2);
        let fb = BlockingFallback::new(vec![action1, action2]);
        let cond1 = Condition::new("1", handle.clone(), |i: i32| i > 0, fb);
        let mut bt = BehaviorTree::new_test(cond1);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(-1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Success);
    }

    //      Seq
    //     /   \
    //  Cond1  Action1
    //
    // pass cond1, during action1 fail cond1, pass fb
    #[tokio::test]
    async fn test_one_time_condition() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = MockAction::new(1);
        let cond1 = OneTimeCondition::new("1", handle.clone(), |i: i32| i > 0);
        let seq = Sequence::new(vec![cond1, action1]);
        let mut bt = BehaviorTree::new_test(seq);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(-1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Success);
    }

    //      FB
    //     /   \
    //  Cond1  Action2
    //    |
    // Action1
    //
    // Fail cond1, pass action2, pass fb
    #[tokio::test]
    async fn test_simple_fallback_plan_b() {
        // Setup
        let handle = Handle::new_from(-1);

        // When
        let action1 = MockAction::new(1);
        let action2 = MockAction::new(2);
        let cond1 = Condition::new("1", handle, |i: i32| i > 0, action1);
        let fb = Fallback::new(vec![cond1, action2]);
        let mut bt = BehaviorTree::new_test(fb);

        // Then
        assert_eq!(bt.run_once().await.unwrap(), Status::Success);
    }

    //      Seq
    //     /   \
    //  Cond1  Action2
    //    |
    //  Cond2
    //    |
    // Action1
    //
    // Pass Cond1, fail Cond2, Fail seq
    #[tokio::test]
    async fn test_double_condition_sequence() {
        // Setup
        let handle1 = Handle::new_from(1);
        let handle2: Handle<i32> = Handle::new_from(-1);

        // When
        let action1 = MockAction::new(1);
        let action2 = MockAction::new(2);
        let cond2 = Condition::new("2", handle2, |i: i32| i > 0, action1);
        let cond1 = Condition::new("1", handle1, |i: i32| i > 0, cond2);
        let seq = Sequence::new(vec![cond1, action2]);
        let mut bt = BehaviorTree::new_test(seq);
        assert_eq!(bt.handles.len(), 5);

        // Then
        assert_eq!(bt.run_once().await.unwrap(), Status::Failure);
    }

    //      Seq
    //     /   \
    // Action1  Cond1
    //            |
    //         Action2
    //
    // Pass action 1, fail cond1 , fail seq
    #[tokio::test]
    async fn test_later_fail_of_sequence() {
        // Setup
        let handle = Handle::new_from(-1);

        // When
        let action1 = MockAction::new(1);
        let action2 = MockAction::new(2);
        let cond1 = Condition::new("1", handle.clone(), |i: i32| i > 0, action2);
        let seq = Sequence::new(vec![action1, cond1]);
        let mut bt = BehaviorTree::new_test(seq);
        assert_eq!(bt.handles.len(), 4);

        // Then
        assert_eq!(bt.run_once().await.unwrap(), Status::Failure);
    }

    //      Seq
    //     /   \
    //  Cond1  Action2
    //    |
    // Action1
    //
    // Pass cond1, fail cond1 during action 1, fail seq
    #[tokio::test]
    async fn test_simple_sequence_with_subscribe() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = MockAction::new(1);
        let action2 = MockAction::new(2);
        let cond1 = Condition::new("1", handle.clone(), |i: i32| i > 0, action1);
        let seq = Sequence::new(vec![cond1, action2]);
        let mut bt = BehaviorTree::new_test(seq);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(-1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Failure);
    }

    //      FB
    //     /   \
    //  Cond1  Cond2
    //    |      |
    // Action1 Action2
    //
    // Fail cond1, Pass cond1 during action1, Fail cond2 during action2, pass fb
    #[tokio::test]
    async fn test_vec_not_empty_with_subscribe() {
        // Setup
        let handle1: Handle<Vec<i32>> = Handle::new_from(vec![]);
        let handle2 = Handle::new_from(1);

        // When
        let action1 = MockAction::new(1);
        let action2 = MockAction::new(2);
        let cond1 = Condition::new("1", handle1.clone(), |x| !x.is_empty(), action1);
        let cond2 = Condition::new("1", handle2.clone(), |i: i32| i > 0, action2);
        let fb = Fallback::new(vec![cond1, cond2]);
        let mut bt = BehaviorTree::new_test(fb);
        assert_eq!(bt.handles.len(), 5);

        let (res, res2, res3) = tokio::join!(
            bt.run_once(),
            async {
                sleep(Duration::from_millis(200)).await;
                handle1.set(vec![i32::default()]).await
            },
            async {
                sleep(Duration::from_millis(400)).await;
                handle2.set(-1).await
            }
        );
        res2.unwrap(); // Check for any unsuspected errors
        res3.unwrap();

        // Then
        assert_eq!(res.unwrap(), Status::Success);
    }

    //      FB
    //     /   \
    //  Cond1  Action2
    //    |
    // Action1
    //
    // Fail cond1, pass cond1 during action 2, switch back to action 1, pass fb
    #[tokio::test]
    async fn test_fallback_switch_to_prio() {
        // Setup
        let handle = Handle::new_from(-1);

        // When
        let action1 = MockAction::new(1);
        let action2 = MockAction::new(2);
        let cond1 = Condition::new("1", handle.clone(), |i: i32| i > 0, action1);
        let fb = Fallback::new(vec![cond1, action2]);
        let mut bt = BehaviorTree::new_test(fb);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Success);
    }

    //      Seq
    //     /   \
    //  Cond1   Action2
    //    |
    //  Cond2
    //    |
    // Action1
    //
    // pass cond1, pass cond2, fail cond2 during action 1, fail cond1, fail seq
    #[tokio::test]
    async fn test_double_condition_sequence_with_subscribe() {
        // Setup
        let handle1 = Handle::new_from(1);
        let handle2 = Handle::new_from(1);

        // When
        let action1 = MockAction::new(1);
        let action2 = MockAction::new(2);
        let cond2 = Condition::new("2", handle2.clone(), |i: i32| i > 0, action1);
        let cond1 = Condition::new("1", handle1, |i: i32| i > 0, cond2);
        let seq = Sequence::new(vec![cond1, action2]);
        let mut bt = BehaviorTree::new_test(seq);
        assert_eq!(bt.handles.len(), 5);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle2.set(-1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Failure);
    }

    //     Cond1
    //       |
    //      Seq
    //     /   \
    // Action1 Action2
    //
    // pass cond1, fail cond1 during action 1, fail seq
    #[tokio::test]
    async fn test_conditional_sequence_with_subscribe() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = MockAction::new(1);
        let action2 = MockAction::new(2);
        let seq = Sequence::new(vec![action1, action2]);
        let cond1 = Condition::new("1", handle.clone(), |i: i32| i > 0, seq);
        let mut bt = BehaviorTree::new_test(cond1);
        assert_eq!(bt.handles.len(), 4);

        // let cond2 fail during execution
        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(-1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Failure);
    }

    //          FB
    //        /   \
    //     Cond1  Action3
    //       |
    //       FB
    //     /   \
    //  Cond2  Action2
    //    |
    // Action1
    //
    // Pass cond1, fail cond2, fail cond1 during action2, pass cond2 during action3 (no effect), pass fb
    #[tokio::test]
    async fn test_failed_fallback_with_delayed_child_request() {
        // Setup
        let handle1 = Handle::new_from(1);
        let handle2 = Handle::new_from(-1);

        // When
        let action1 = MockAction::new(1);
        let cond2 = Condition::new("2", handle2.clone(), |i: i32| i > 0, action1);
        let action2 = MockAction::new(2);
        let fb2 = Fallback::new(vec![cond2, action2]);
        let cond1 = Condition::new("1", handle1.clone(), |i: i32| i > 0, fb2);
        let action3 = MockAction::new(3);
        let fb1 = Fallback::new(vec![cond1, action3]);
        let mut bt = BehaviorTree::new_test(fb1);
        assert_eq!(bt.handles.len(), 7);

        let (res, res2, res3) = tokio::join!(
            bt.run_once(),
            async {
                sleep(Duration::from_millis(200)).await;
                handle1.set(-1).await
            },
            async {
                sleep(Duration::from_millis(400)).await;
                handle2.set(1).await
            }
        );
        res2.unwrap(); // Check for any unsuspected errors
        res3.unwrap();

        // Then
        assert_eq!(res.unwrap(), Status::Success);
    }

    //     Cond1
    //       |
    //       FB
    //     /   \
    //  Cond2  Action2
    //    |
    // Action1
    //
    // Pass cond1, pass cond2, fail cond1 during action1
    #[tokio::test]
    async fn test_prohibited_fallback() {
        // Setup
        let handle1 = Handle::new_from(1);
        let handle2 = Handle::new_from(1);

        // When
        let action1 = MockAction::new(1);
        let cond2 = Condition::new("2", handle2.clone(), |i: i32| i > 0, action1);
        let action2 = MockAction::new(2);
        let fb = Fallback::new(vec![cond2, action2]);
        let cond1 = Condition::new("1", handle1.clone(), |i: i32| i > 0, fb);
        let mut bt = BehaviorTree::new_test(cond1);
        assert_eq!(bt.handles.len(), 5);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle1.set(-1).await
        },);
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Failure);
    }

    //       FB
    //     /   \
    //  Cond1   Cond3
    //    |      |
    //  Cond2  Action2
    //    |
    // Action1
    //
    // pass cond1, fail cond2, pass cond3, pass cond2 during action 2, fail cond3 during action 1 (no effect), pass fb
    #[tokio::test]
    async fn test_double_request_start_before_failing_fallback() {
        // Setup
        let handle1 = Handle::new_from(1);
        let handle2 = Handle::new_from(-1);
        let handle3 = Handle::new_from(1);

        // When
        let action1 = MockAction::new(1);
        let cond2 = Condition::new("2", handle2.clone(), |i: i32| i > 0, action1);
        let cond1 = Condition::new("1", handle1, |i: i32| i > 0, cond2);
        let action2 = MockAction::new(2);
        let cond3 = Condition::new("3", handle3.clone(), |i: i32| i > 0, action2);
        let fb = Fallback::new(vec![cond1, cond3]);
        let mut bt = BehaviorTree::new_test(fb);
        assert_eq!(bt.handles.len(), 6);

        let (res, res2, res3) = tokio::join!(
            bt.run_once(),
            async {
                sleep(Duration::from_millis(200)).await;
                handle2.set(1).await
            },
            async {
                sleep(Duration::from_millis(400)).await;
                handle3.set(-1).await
            }
        );
        res2.unwrap(); // Check for any unsuspected errors
        res3.unwrap();

        // Then
        assert_eq!(res.unwrap(), Status::Success);
    }
}
