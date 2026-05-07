use crate::event::RealtimeEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseResult {
    Passed,
    Blocked,
    Failed,
    Incomplete,
}

#[derive(Debug, Default)]
pub struct PhaseGate {
    saw_live_delta: bool,
    saw_completion: bool,
    saw_failure: bool,
}

impl PhaseGate {
    pub fn observe(&mut self, event: &RealtimeEvent) {
        match event {
            RealtimeEvent::LiveDelta(delta) | RealtimeEvent::LiveHypothesis(delta)
                if !delta.trim().is_empty() && !self.saw_completion =>
            {
                self.saw_live_delta = true;
            }
            RealtimeEvent::Completed(_) => {
                self.saw_completion = true;
            }
            RealtimeEvent::Failed(_) | RealtimeEvent::Error(_) => {
                self.saw_failure = true;
            }
            _ => {}
        }
    }

    pub fn result(&self) -> PhaseResult {
        if self.saw_failure {
            PhaseResult::Failed
        } else if self.saw_completion && self.saw_live_delta {
            PhaseResult::Passed
        } else if self.saw_completion {
            PhaseResult::Blocked
        } else {
            PhaseResult::Incomplete
        }
    }
}
