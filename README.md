# behavior-tree

This [behavior tree](<https://en.wikipedia.org/wiki/Behavior_tree_(artificial_intelligence,_robotics_and_control)>) is fully event-based, largely following the definitions of [Unreal Engine](https://docs.unrealengine.com/5.0/en-US/behavior-tree-in-unreal-engine---overview/#behaviortreesareevent-driven). The events are implemented using the [actor model crate](https://github.com/AvalorAI/actor-model).

Some unique decorators have been added. The BlockingAction prevents stopping the specific action, and allows it to finish.

Conditions and Actions can be implemented using the Evaluator and Executor trait respectively.

**Feature requests**:

- Loop until success / failure
- General node type composing overlapping code
- Remove Ok(msg) from select statement to guarantee that all messages are processed
- Condition without handle

# Versioning

We only use behavior-tree internally so we can we can use versioning losely.
Whenever you add some new code just update the minor version by 1.

You have to update the version in the Cargo.toml and as git tag. The tag is used by cargo to download the correct version. This way you can also already test a branch of the behavior-tree without merging to main first.

```bash
git tag -a vX.X -m "Release version vX.X"
git push origin vX.X
```
