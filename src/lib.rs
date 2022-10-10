mod bt;

pub use crate::bt::BehaviorTree;
pub use crate::bt::{
    action::{Action, BlockingAction, Executor, Wait},
    blocking_check::BlockingCheck,
    condition::{Condition, Evaluator},
    fallback::Fallback,
    loop_dec::LoopDecorator,
    sequence::Sequence,
};

#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod logging {
    use env_logger::Env;

    pub fn load_logger() {
        let filter = "debug";
        let log_level = Env::default().default_filter_or(filter);
        env_logger::Builder::from_env(log_level)
            .format_timestamp(None)
            .init();
    }
}
