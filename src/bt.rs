use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use simple_xml_builder::XMLElement;
use std::fs::File;
use tokio::time::{sleep, Duration};

use self::handle::NodeError;
use handle::{ChildMessage, NodeHandle, ParentMessage, Status};

const CHANNEL_SIZE: usize = 20;

pub mod action;
pub mod blocking_check;
pub mod condition;
pub mod fallback;
pub mod handle;
pub mod loop_dec;
pub mod sequence;

pub struct BehaviorTree {
    pub root_node: NodeHandle,
    pub handles: Option<Vec<NodeHandle>>,
}

impl BehaviorTree {
    pub fn new(root_node: NodeHandle) -> Self {
        Self {
            root_node,
            handles: None,
        }
    }

    // Run continuously
    pub async fn run(&mut self) -> Result<()> {
        self.expand_handles().await?;

        if let Err(e) = self._run().await {
            log::warn!("BT crashed - {:?} ", e);
            let handles = self.handles.as_mut().unwrap(); // Safe unwrap as its setter is guarding this function
            for handle in handles {
                log::debug!("Killing {} {:?}", handle.element, handle.name);
                if let Err(e) = handle.kill().await {
                    log::error!("Killing {:?} failed: {e:?}", handle.name);
                    return Err(anyhow!(
                        "Cannot safely rebuild behaviour tree with active nodes: {e:?}"
                    ));
                };
            }
            log::debug!("Killed all nodes succesfully");
        }
        Ok(()) // This Ok is never fired, as _run can only exit through error propagation
    }

    async fn _run(&mut self) -> Result<()> {
        log::debug!("Starting BT from {:?}", self.root_node.name);
        loop {
            let status = self.run_once().await?;
            log::debug!("Exited BT with status: {:?} - restarting again", status);
            sleep(Duration::from_millis(1000)).await; // wait until mavlink values received
        }
    }

    async fn expand_handles(&mut self) -> Result<()> {
        let mut handles = self.root_node.append_childs(vec![]).await?;
        handles.push(self.root_node.clone());
        self.handles = Some(handles);
        Ok(())
    }

    async fn run_once(&mut self) -> Result<Status, NodeError> {
        // Run_once is called directly from testing functions, so doubly check the existance of the handles
        if self.handles.is_none() {
            // TODO if this fails, it should not be allowed to exit, because killing cannot be done safely
            self.expand_handles()
                .await
                .expect("Expanding the handles failed");
        }

        self.root_node.send(ChildMessage::Start)?;
        loop {
            match self.root_node.listen().await? {
                ParentMessage::Status(status) => match status {
                    Status::Succes => return Ok(status),
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

    pub async fn save_xml_export<S: Into<String> + Clone>(&mut self, name: S) -> Result<()> {
        let file = File::create(format!("{}.xml", name.clone().into()))?;
        let root = self.export_xml(name).await?;
        root.write(file)?;
        Ok(())
    }

    pub async fn export_xml<S: Into<String> + Clone>(&mut self, name: S) -> Result<XMLElement> {
        // Groot format. See https://github.com/BehaviorTree/Groot
        if self.handles.is_none() {
            self.expand_handles().await.expect("Expansion failed");
        }

        let handles = self.handles.as_ref().ok_or(anyhow!("No handles set"))?;

        let mut root = XMLElement::new("root");
        root.add_attribute("main_tree_to_execute", "MainTree");
        let mut tree = XMLElement::new(name.into());
        tree.add_attribute("ID", "MainTree");

        // Start with root node
        let root_element = self.root_node.get_xml();
        let children_names = self.root_node.children_names.clone();
        let root_element = self.add_children(handles, root_element, children_names);

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
                .find_map(|x| {
                    if x.name == *child {
                        Some(x.clone())
                    } else {
                        None
                    }
                })
                .expect("A child was not present in the handles!");

            let el_base = handle.get_xml();
            let children_names = handle.children_names.clone();
            let el_expanded = self.add_children(handles, el_base, children_names);
            element.add_child(el_expanded)
        }
        element
    }

    pub async fn save_json_export<S: Into<String> + Clone>(&mut self, name: S) -> Result<()> {
        let file = File::create(format!("{}.json", name.clone().into()))?;
        let bt = self.export_json(name).await?;
        serde_json::to_writer(&file, &bt)?;
        Ok(())
    }

    pub async fn export_json<S: Into<String> + Clone>(&mut self, name: S) -> Result<Value> {
        if self.handles.is_none() {
            self.expand_handles().await.expect("Expansion failed");
        }

        let handles = self.handles.as_ref().ok_or(anyhow!("No handles set"))?;

        let node_description: Vec<serde_json::value::Value> =
            handles.iter().map(|x| x.get_json()).collect();

        let bt = json!({
            "name": name.into(),
            "rootNode": self.root_node.name.clone(),
            "nodes": json!(node_description),
        });

        Ok(bt)
    }
}

#[cfg(test)]
#[allow(unused_imports)]
mod tests {
    use actor_model::Handle;
    use std::collections::HashMap;

    use super::*;
    use super::{
        blocking_check::BlockingCheck,
        condition::{Condition, OneTimeCondition},
        fallback::Fallback,
        loop_dec::LoopDecorator,
        sequence::Sequence,
    };
    use crate::bt::action::mocking::{MockAction, MockBlockingAction};
    use crate::bt::condition::mocking::MockAsyncCondition;
    use crate::logging::load_logger;

    fn dummy_bt() -> BehaviorTree {
        let handle = Handle::new_from(-1);
        let action1 = MockAction::new(1);
        let cond1 = Condition::new("cond1", handle.clone(), |i: i32| i > 0, action1);
        let seq = Sequence::new(vec![cond1]);
        let action2 = MockAction::new_failing(2);
        let fb = Fallback::new(vec![seq, action2]);
        BehaviorTree::new(fb)
    }

    #[tokio::test]
    #[ignore]
    async fn test_save_xml_export() {
        let mut bt = dummy_bt();
        let res = bt.save_xml_export("Test_BT").await;
        assert!(res.is_ok())
    }

    #[tokio::test]
    #[ignore]
    async fn test_save_json_export() {
        let mut bt = dummy_bt();
        let res = bt.save_json_export("Test_BT").await;
        assert!(res.is_ok())
    }

    #[tokio::test]
    async fn test_export_xml() {
        let mut bt = dummy_bt();
        let res = bt.export_xml("Test_BT").await;
        assert!(res.is_ok())
    }

    #[tokio::test]
    async fn test_export_json() {
        let mut bt = dummy_bt();
        let res = bt.export_json("Test_BT").await;
        assert!(res.is_ok())
    }

    #[tokio::test]
    async fn test_killing_nodes() {
        let mut action = MockAction::new(1);
        _ = action.append_childs(vec![]).await.unwrap();

        let mut blocking_action = MockBlockingAction::new(1);
        _ = blocking_action.append_childs(vec![]).await.unwrap();
        assert!(blocking_action.kill().await.is_ok());

        let handle = Handle::new_from(1);
        let mut cond = Condition::new("1", handle.clone(), |x| x > 0, action.clone());
        _ = cond.append_childs(vec![]).await.unwrap();
        assert!(cond.kill().await.is_ok());

        let mut check = BlockingCheck::new("1", handle.clone(), action.clone());
        _ = check.append_childs(vec![]).await.unwrap();
        assert!(check.kill().await.is_ok());

        let mut bt_loop = LoopDecorator::new("1", action.clone(), 100);
        _ = bt_loop.append_childs(vec![]).await.unwrap();
        assert!(bt_loop.kill().await.is_ok());

        let mut fb = Fallback::new(vec![action.clone()]);
        _ = fb.append_childs(vec![]).await.unwrap();
        assert!(fb.kill().await.is_ok());

        let mut seq = Sequence::new(vec![action.clone()]);
        _ = seq.append_childs(vec![]).await.unwrap();
        assert!(seq.kill().await.is_ok());

        assert!(action.kill().await.is_ok()); // Actions cannot be killed before expansion of the tree
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
        let mut bt = BehaviorTree::new(fb);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Succes);
    }

    //  Cond1
    //    |
    // Action1
    //
    // Cond1 passes, cond2 fails during Action1, BT still succesful
    #[tokio::test]
    async fn test_force_action_completion() {
        // Setup
        let handle = Handle::new_from(1);

        // When
        let action1 = MockBlockingAction::new(1);
        let cond1 = Condition::new("1", handle.clone(), |x| x > 0, action1);
        let mut bt = BehaviorTree::new(cond1);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(-1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Succes);
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
        let mut bt = BehaviorTree::new(cond1);

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
        let mut bt = BehaviorTree::new(bt_loop);

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
        let mut bt = BehaviorTree::new(cond1);

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
        let mut bt = BehaviorTree::new(cond1);

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
        assert_eq!(res.unwrap().unwrap(), Status::Succes);
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
        let mut bt = BehaviorTree::new(cond1);

        // Then
        assert_eq!(bt.run_once().await.unwrap(), Status::Succes);
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
        let mut bt = BehaviorTree::new(cond1);

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
        let mut bt = BehaviorTree::new(cond1);

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
        let mut bt = BehaviorTree::new(cond1);

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
        let mut bt = BehaviorTree::new(seq);

        // Then
        assert_eq!(bt.run_once().await.unwrap(), Status::Succes);
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
        let mut bt = BehaviorTree::new(fb);

        // Then
        assert_eq!(bt.run_once().await.unwrap(), Status::Succes);
    }

    //      FB
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
        let fb = Fallback::new(vec![cond1, action1]);
        let mut bt = BehaviorTree::new(fb);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(-1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Succes);
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
        let mut bt = BehaviorTree::new(fb);

        // Then
        assert_eq!(bt.run_once().await.unwrap(), Status::Succes);
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
        let mut bt = BehaviorTree::new(seq);

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
        let mut bt = BehaviorTree::new(seq);

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
        let mut bt = BehaviorTree::new(seq);

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
        let mut bt = BehaviorTree::new(fb);

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
        assert_eq!(res.unwrap(), Status::Succes);
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
        let mut bt = BehaviorTree::new(fb);

        let (res, res2) = tokio::join!(bt.run_once(), async {
            sleep(Duration::from_millis(200)).await;
            handle.set(1).await
        });
        res2.unwrap(); // Check for any unsuspected errors

        // Then
        assert_eq!(res.unwrap(), Status::Succes);
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
        let mut bt = BehaviorTree::new(seq);

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
        let mut bt = BehaviorTree::new(cond1);

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
        let mut bt = BehaviorTree::new(fb1);

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
        assert_eq!(res.unwrap(), Status::Succes);
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
        let mut bt = BehaviorTree::new(cond1);

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
        let mut bt = BehaviorTree::new(fb);

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
        assert_eq!(res.unwrap(), Status::Succes);
    }
}
