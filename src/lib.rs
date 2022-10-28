mod bt;

#[cfg(feature = "websocket")]
mod ws;

pub use crate::bt::BehaviorTree;
pub use crate::bt::{
    action::{Action, BlockingAction, Executor, Failure, Success, Wait},
    blocking_check::BlockingCheck,
    condition::{Condition, Evaluator, OneTimeCondition},
    fallback::{BlockingFallback, Fallback},
    handle::NodeHandle,
    listener::{OuterStatus as Status, Update},
    loop_dec::LoopDecorator,
    sequence::{BlockingSequence, Sequence},
};

#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod logging {
    use env_logger::Env;

    pub fn load_logger() {
        let filter = "debug";
        let log_level = Env::default().default_filter_or(filter);
        env_logger::Builder::from_env(log_level).format_timestamp(None).init();
    }
}
