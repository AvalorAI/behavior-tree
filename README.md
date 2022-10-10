# behavior-tree

This [behavior tree](<https://en.wikipedia.org/wiki/Behavior_tree_(artificial_intelligence,_robotics_and_control)>) is fully event-based, largely following the definitions of [Unreal Engine](https://docs.unrealengine.com/5.0/en-US/behavior-tree-in-unreal-engine---overview/#behaviortreesareevent-driven). The events are implemented using the [actor model crate](https://gitlab.com/avalor_ai/actor-model).

Some unique decorators have been added. The BlockingAction prevents stopping the specific action, and allows it to finish. The BlockingCheck is a condition that does not fail when no value actor value is set, but instead awaits its first value. Thereafter, it always succeeds.

Conditions and Actions can be implemented using the Evaluator and Executor trait respectively.

**Feature requests**:

- Loop until success / failure
- Condition without a child that returns success upon succesful evaluation
