//! Cancellation and result ownership for asynchronous terminal connection requests.

use std::collections::HashMap;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Attempt {
    request: Uuid,
    target: Option<Uuid>,
    cancel: CancellationToken,
}

impl Drop for Attempt {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[derive(Default)]
pub(super) struct ConnectionAttempts(HashMap<Uuid, Attempt>);

impl ConnectionAttempts {
    pub(super) fn begin(
        &mut self,
        connection: Uuid,
        target: Option<Uuid>,
    ) -> Option<(Uuid, CancellationToken)> {
        let std::collections::hash_map::Entry::Vacant(entry) = self.0.entry(connection) else {
            return None;
        };
        let request = Uuid::new_v4();
        let cancel = CancellationToken::new();
        entry.insert(Attempt {
            request,
            target,
            cancel: cancel.clone(),
        });
        Some((request, cancel))
    }

    pub(super) fn complete(&mut self, connection: Uuid, request: Uuid) -> bool {
        if self
            .0
            .get(&connection)
            .is_some_and(|attempt| attempt.request == request)
        {
            self.0.remove(&connection);
            true
        } else {
            false
        }
    }

    pub(super) fn cancel_pane(&mut self, pane: Uuid) {
        self.0.retain(|_, attempt| attempt.target != Some(pane));
    }

    pub(super) fn clear(&mut self) {
        self.0.clear();
    }

    pub(super) fn contains(&self, id: &Uuid) -> bool {
        self.0.contains_key(id)
    }
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_split_owner_cancels_only_its_request_and_rejects_late_results() {
        let mut attempts = ConnectionAttempts::default();
        let connection = Uuid::new_v4();
        let other = Uuid::new_v4();
        let pane = Uuid::new_v4();
        let (old, canceled) = attempts.begin(connection, Some(pane)).expect("first open");
        let (independent, retained) = attempts.begin(other, None).expect("independent tab");
        assert!(
            attempts.begin(connection, Some(pane)).is_none(),
            "duplicate clicks must share the pending open"
        );
        attempts.cancel_pane(pane);
        assert!(canceled.is_cancelled());
        assert!(!retained.is_cancelled());
        let (replacement, _) = attempts
            .begin(connection, None)
            .expect("reopen after cancel");
        assert!(
            !attempts.complete(connection, old),
            "late success must not consume the replacement"
        );
        assert!(attempts.contains(&connection));
        assert!(attempts.complete(connection, replacement));
        assert!(attempts.complete(other, independent));
        assert!(attempts.is_empty());
    }

    #[test]
    fn shutdown_and_drop_cancel_pending_opens() {
        let mut attempts = ConnectionAttempts::default();
        let (_, first) = attempts.begin(Uuid::new_v4(), None).expect("pending open");
        attempts.clear();
        assert!(first.is_cancelled());
        let (_, second) = attempts.begin(Uuid::new_v4(), None).expect("pending open");
        drop(attempts);
        assert!(second.is_cancelled());
    }
}
