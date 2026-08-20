//! Tracks whether the agent's own plan still has work left, so a turn that
//! ends early can be continued without a human.
//!
//! The gap this fills: `maybe_start_auto_nudge_turn` only fires on a *stall*
//! (a repeat-guard block or a no-progress timeout). A turn that ends with an
//! ordinary `task_complete` while the plan is half-finished is not a stall by
//! any of those definitions, so nothing nudges it and the session sits idle
//! until a human types "continue". Observed 2026-08-19: a session announced
//! "Now I'll start writing the app code" and then idled for eleven hours.
//!
//! The signal is the agent's own `update_plan` state, not a timer -- the same
//! preference for evidence over stopwatches that the repeat guard follows.

use codex_protocol::plan_tool::StepStatus;
use codex_protocol::plan_tool::UpdatePlanArgs;

/// Plan state plus the streak of continues that produced nothing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PlanProgress {
    total: usize,
    completed: usize,
    /// Consecutive auto-continues since a plan step last advanced. Reset by
    /// real progress, which is what lets an agent that is genuinely working
    /// run indefinitely while one that is spinning still stops.
    idle_continues: u32,
    /// Whether a plan has ever been recorded. Without this an empty plan and
    /// "no plan tool used at all" look identical, and we would continue
    /// sessions that never opted into planning.
    seen: bool,
}

impl PlanProgress {
    /// Record the latest `update_plan` call.
    pub fn record(&mut self, args: &UpdatePlanArgs) {
        let completed = args
            .plan
            .iter()
            .filter(|step| matches!(step.status, StepStatus::Completed))
            .count();
        // Any forward movement clears the streak. Compare against the previous
        // count rather than "did the plan change at all" -- an agent that keeps
        // rewording the same unfinished steps is not making progress, and that
        // is precisely the spin this guard has to stop.
        if !self.seen || completed > self.completed {
            self.idle_continues = 0;
        }
        self.total = args.plan.len();
        self.completed = completed;
        self.seen = true;
    }

    /// Whether the recorded plan still has unfinished steps.
    pub fn has_unfinished_work(&self) -> bool {
        self.seen && self.total > 0 && self.completed < self.total
    }

    /// Whether another auto-continue is allowed, consuming one from the
    /// streak. `max` of 0 disables continuation entirely.
    pub fn take_continue(&mut self, max: u32) -> bool {
        if max == 0 || !self.has_unfinished_work() || self.idle_continues >= max {
            return false;
        }
        self.idle_continues += 1;
        true
    }

    /// Cleared when a new user message arrives: the human is present and
    /// steering, so any previous spin should not count against them.
    pub fn reset_streak(&mut self) {
        self.idle_continues = 0;
    }

    pub fn remaining(&self) -> usize {
        self.total.saturating_sub(self.completed)
    }
}

/// Injected when a turn ends with plan steps outstanding. Deliberately does
/// not restate the plan -- the model can already see it -- and deliberately
/// tells it to act rather than re-plan, because the observed failure was an
/// agent narrating its next step instead of taking it.
pub fn plan_continue_message(remaining: usize) -> String {
    format!(
        "Your turn ended with {remaining} plan step(s) still unfinished, and no \
         one is waiting to reply -- this session is running unattended. Continue \
         the work now: pick up the next incomplete step and actually perform it \
         with tool calls. Do not restate the plan, do not summarize what you are \
         about to do, and do not end your turn again until either the step is \
         genuinely done or you are blocked on something only a human can resolve \
         (in which case say plainly what you need)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::plan_tool::PlanItemArg;

    fn plan(statuses: &[StepStatus]) -> UpdatePlanArgs {
        UpdatePlanArgs {
            explanation: None,
            plan: statuses
                .iter()
                .map(|s| PlanItemArg {
                    step: "step".to_string(),
                    status: s.clone(),
                })
                .collect(),
        }
    }

    #[test]
    fn no_plan_recorded_means_no_continuation() {
        let mut p = PlanProgress::default();
        assert!(!p.has_unfinished_work());
        assert!(!p.take_continue(3), "a session that never planned must not be continued");
    }

    #[test]
    fn fully_completed_plan_does_not_continue() {
        let mut p = PlanProgress::default();
        p.record(&plan(&[StepStatus::Completed, StepStatus::Completed]));
        assert!(!p.has_unfinished_work());
        assert!(!p.take_continue(3));
    }

    #[test]
    fn unfinished_plan_continues_up_to_the_cap() {
        let mut p = PlanProgress::default();
        p.record(&plan(&[StepStatus::Completed, StepStatus::Pending]));
        assert!(p.take_continue(2));
        assert!(p.take_continue(2));
        assert!(!p.take_continue(2), "must stop once the idle streak hits the cap");
    }

    #[test]
    fn real_progress_resets_the_streak_so_a_working_agent_runs_indefinitely() {
        let mut p = PlanProgress::default();
        p.record(&plan(&[StepStatus::Pending, StepStatus::Pending, StepStatus::Pending]));
        assert!(p.take_continue(1));
        assert!(!p.take_continue(1), "streak exhausted");
        // The agent completes a step -- it is working, not spinning.
        p.record(&plan(&[StepStatus::Completed, StepStatus::Pending, StepStatus::Pending]));
        assert!(p.take_continue(1), "progress must buy another continue");
    }

    #[test]
    fn rewording_unfinished_steps_is_not_progress() {
        let mut p = PlanProgress::default();
        p.record(&plan(&[StepStatus::Pending, StepStatus::InProgress]));
        assert!(p.take_continue(1));
        // Same completed count, different wording/status churn.
        p.record(&plan(&[StepStatus::InProgress, StepStatus::Pending]));
        assert!(!p.take_continue(1), "status churn without completion must not reset the streak");
    }

    #[test]
    fn in_progress_still_counts_as_unfinished() {
        let mut p = PlanProgress::default();
        p.record(&plan(&[StepStatus::InProgress]));
        assert!(p.has_unfinished_work());
    }

    #[test]
    fn zero_max_disables_continuation() {
        let mut p = PlanProgress::default();
        p.record(&plan(&[StepStatus::Pending]));
        assert!(!p.take_continue(0));
    }

    #[test]
    fn a_new_user_message_clears_the_streak() {
        let mut p = PlanProgress::default();
        p.record(&plan(&[StepStatus::Pending]));
        assert!(p.take_continue(1));
        assert!(!p.take_continue(1));
        p.reset_streak();
        assert!(p.take_continue(1), "human steering must not leave the session stuck");
    }

    #[test]
    fn remaining_counts_unfinished_steps() {
        let mut p = PlanProgress::default();
        p.record(&plan(&[StepStatus::Completed, StepStatus::Pending, StepStatus::InProgress]));
        assert_eq!(p.remaining(), 2);
    }
}
