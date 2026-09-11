use splice_platform::files::ViewState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadDecision {
    Deny,
    Commit,
    Serve,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelOutcome {
    ZeroPayload,
    StopTransfer,
    AlreadyRetired,
}

#[derive(Debug)]
pub struct Gate {
    state: ViewState,
    dragging: bool,
}

impl Gate {
    pub fn new() -> Self {
        Self {
            state: ViewState::Offered,
            dragging: false,
        }
    }

    pub fn committed() -> Self {
        Self {
            state: ViewState::Committed,
            dragging: false,
        }
    }

    pub fn state(&self) -> ViewState {
        self.state
    }

    pub fn drag_started(&mut self) {
        if self.state == ViewState::Offered {
            self.dragging = true;
        }
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    pub fn drop_performed(&mut self) -> bool {
        match self.state {
            ViewState::Offered => {
                self.state = ViewState::DroppedAwaitingRead;
                self.dragging = false;
                true
            }
            _ => false,
        }
    }

    pub fn on_read(&mut self) -> ReadDecision {
        match self.state {
            ViewState::Offered => ReadDecision::Deny,
            ViewState::DroppedAwaitingRead => {
                self.state = ViewState::Committed;
                ReadDecision::Commit
            }
            ViewState::Committed => ReadDecision::Serve,
            ViewState::Retired => ReadDecision::Deny,
        }
    }

    pub fn cancel(&mut self) -> CancelOutcome {
        self.dragging = false;
        match self.state {
            ViewState::Offered | ViewState::DroppedAwaitingRead => {
                self.state = ViewState::Retired;
                CancelOutcome::ZeroPayload
            }
            ViewState::Committed => {
                self.state = ViewState::Retired;
                CancelOutcome::StopTransfer
            }
            ViewState::Retired => CancelOutcome::AlreadyRetired,
        }
    }

    pub fn retire(&mut self) {
        self.state = ViewState::Retired;
        self.dragging = false;
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_committed_gate_serves_without_recommit() {
        let mut gate = Gate::committed();
        assert_eq!(gate.on_read(), ReadDecision::Serve);
        assert!(!gate.drop_performed());
        assert_eq!(gate.cancel(), CancelOutcome::StopTransfer);
    }

    #[test]
    fn pre_drop_read_denied() {
        let mut gate = Gate::new();
        assert_eq!(gate.on_read(), ReadDecision::Deny);
        assert_eq!(gate.on_read(), ReadDecision::Deny);
        assert_eq!(gate.state(), ViewState::Offered);
    }

    #[test]
    fn failed_pre_drop_reads_do_not_authorize() {
        let mut gate = Gate::new();
        assert_eq!(gate.on_read(), ReadDecision::Deny);
        assert!(gate.drop_performed());
        assert_eq!(gate.on_read(), ReadDecision::Commit);
        assert_eq!(gate.state(), ViewState::Committed);
    }

    #[test]
    fn first_post_drop_read_commits_exactly_once() {
        let mut gate = Gate::new();
        assert!(gate.drop_performed());
        assert_eq!(gate.on_read(), ReadDecision::Commit);
        assert_eq!(gate.on_read(), ReadDecision::Serve);
        assert_eq!(gate.on_read(), ReadDecision::Serve);
    }

    #[test]
    fn cancel_before_read_is_zero_payload() {
        let mut gate = Gate::new();
        assert!(gate.drop_performed());
        assert_eq!(gate.cancel(), CancelOutcome::ZeroPayload);
        assert_eq!(gate.on_read(), ReadDecision::Deny);
    }

    #[test]
    fn cancel_before_any_drop_is_zero_payload() {
        let mut gate = Gate::new();
        assert_eq!(gate.cancel(), CancelOutcome::ZeroPayload);
    }

    #[test]
    fn cancel_after_commit_stops_transfer() {
        let mut gate = Gate::new();
        assert!(gate.drop_performed());
        assert_eq!(gate.on_read(), ReadDecision::Commit);
        assert_eq!(gate.cancel(), CancelOutcome::StopTransfer);
        assert_eq!(gate.on_read(), ReadDecision::Deny);
    }

    #[test]
    fn cancel_is_idempotent_after_retire() {
        let mut gate = Gate::new();
        gate.cancel();
        assert_eq!(gate.cancel(), CancelOutcome::AlreadyRetired);
    }

    #[test]
    fn drop_performed_after_commit_or_retire_is_ignored() {
        let mut gate = Gate::new();
        assert!(gate.drop_performed());
        gate.on_read();
        assert!(!gate.drop_performed());
        gate.cancel();
        assert!(!gate.drop_performed());
    }

    #[test]
    fn drag_started_marks_only_offered_views_and_clears_on_leaving_offered() {
        let mut gate = Gate::new();
        assert!(!gate.is_dragging());
        gate.drag_started();
        assert!(gate.is_dragging());
        assert!(gate.drop_performed());
        assert!(!gate.is_dragging());

        let mut committed = Gate::committed();
        committed.drag_started();
        assert!(!committed.is_dragging());

        let mut cancelled = Gate::new();
        cancelled.drag_started();
        cancelled.cancel();
        assert!(!cancelled.is_dragging());

        let mut retired = Gate::new();
        retired.drag_started();
        retired.retire();
        assert!(!retired.is_dragging());
    }

    #[test]
    fn drop_performed_is_idempotent_while_awaiting_read() {
        let mut gate = Gate::new();
        assert!(gate.drop_performed());
        assert!(!gate.drop_performed());
        assert_eq!(gate.state(), ViewState::DroppedAwaitingRead);
    }
}
