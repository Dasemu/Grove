//! Pure classification of a worktree's sync state (section 7.4 of the plan).

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// Uncommitted changes (tracked, staged or untracked, minus ignored).
    pub dirty: bool,
    /// Local branch has commits the remote branch lacks.
    pub ahead: bool,
    /// Remote branch has commits the local branch lacks.
    pub behind: bool,
    /// Last operation failed (auth, network, hook failure...).
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Synced,
    Dirty,
    Ahead,
    Behind,
    /// Remote moved but the worktree has local edits: never touched.
    BehindDirty,
    Diverged,
    Error(String),
}

pub fn classify(f: &Facts) -> State {
    if let Some(e) = &f.error {
        return State::Error(e.clone());
    }
    match (f.ahead, f.behind, f.dirty) {
        (true, true, _) => State::Diverged,
        (false, true, true) => State::BehindDirty,
        (false, true, false) => State::Behind,
        (true, false, _) => State::Ahead,
        (false, false, true) => State::Dirty,
        (false, false, false) => State::Synced,
    }
}

/// What the receive step may do to the files of a worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveAction {
    FastForward,
    Skip,
}

/// Only a clean, strictly-behind worktree is ever fast-forwarded.
pub fn receive_action(state: &State) -> ReceiveAction {
    match state {
        State::Behind => ReceiveAction::FastForward,
        _ => ReceiveAction::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(dirty: bool, ahead: bool, behind: bool) -> Facts {
        Facts {
            dirty,
            ahead,
            behind,
            error: None,
        }
    }

    #[test]
    fn classifies_every_combination() {
        assert_eq!(classify(&f(false, false, false)), State::Synced);
        assert_eq!(classify(&f(true, false, false)), State::Dirty);
        assert_eq!(classify(&f(false, true, false)), State::Ahead);
        assert_eq!(classify(&f(true, true, false)), State::Ahead);
        assert_eq!(classify(&f(false, false, true)), State::Behind);
        assert_eq!(classify(&f(true, false, true)), State::BehindDirty);
        assert_eq!(classify(&f(false, true, true)), State::Diverged);
        assert_eq!(classify(&f(true, true, true)), State::Diverged);
    }

    #[test]
    fn error_wins() {
        let mut x = f(true, true, true);
        x.error = Some("auth".into());
        assert_eq!(classify(&x), State::Error("auth".into()));
    }

    #[test]
    fn only_clean_behind_fast_forwards() {
        for s in [
            State::Synced,
            State::Dirty,
            State::Ahead,
            State::BehindDirty,
            State::Diverged,
            State::Error("x".into()),
        ] {
            assert_eq!(receive_action(&s), ReceiveAction::Skip);
        }
        assert_eq!(receive_action(&State::Behind), ReceiveAction::FastForward);
    }
}
