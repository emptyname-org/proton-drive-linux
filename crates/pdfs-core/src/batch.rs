//! Per-node outcomes of the SDK's batch node operations.
//!
//! `trash_nodes`, `restore_nodes`, `delete_nodes` and `move_nodes` report one
//! outcome per node instead of failing the whole call: a node the server
//! rejects leaves the rest of the batch applied. The daemon's callers fall into
//! two shapes, and both are here so neither has to re-derive the fold:
//!
//! - one node in, one answer out — [`into_unit`], which collapses the single
//!   outcome back to a `Result`;
//! - a user-supplied list — [`split`], which keeps the successes and hands back
//!   the failures to report.

use proton_sdk::error::ProtonError;
use proton_sdk::ids::NodeUid;

/// One node's outcome, as the SDK batch operations report it.
pub type Outcome = (NodeUid, Result<(), ProtonError>);

/// Collapse a batch result to the first per-node failure, for the callers that
/// pass exactly one node and want the old all-or-nothing answer.
///
/// An empty outcome list is `Ok`: the server accepted the request and reported
/// on nothing, which is not an error the caller can act on.
pub fn into_unit(outcomes: Vec<Outcome>) -> Result<(), ProtonError> {
    for (_, outcome) in outcomes {
        outcome?;
    }
    Ok(())
}

/// Split a batch result into the nodes that succeeded and the ones that failed.
pub fn split(outcomes: Vec<Outcome>) -> (Vec<NodeUid>, Vec<(NodeUid, ProtonError)>) {
    let mut done = Vec::with_capacity(outcomes.len());
    let mut failed = Vec::new();
    for (uid, outcome) in outcomes {
        match outcome {
            Ok(()) => done.push(uid),
            Err(e) => failed.push((uid, e)),
        }
    }
    (done, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proton_sdk::ids::{LinkId, VolumeId};

    fn uid(link: &str) -> NodeUid {
        NodeUid::new(VolumeId::new("vol"), LinkId::new(link))
    }

    fn failure() -> ProtonError {
        ProtonError::invalid_operation("nope")
    }

    #[test]
    fn into_unit_is_ok_when_every_node_succeeded() {
        assert!(into_unit(vec![(uid("a"), Ok(())), (uid("b"), Ok(()))]).is_ok());
        assert!(
            into_unit(Vec::new()).is_ok(),
            "nothing reported is not a failure"
        );
    }

    #[test]
    fn into_unit_surfaces_a_failed_node() {
        let outcomes = vec![(uid("a"), Ok(())), (uid("b"), Err(failure()))];
        assert!(into_unit(outcomes).is_err());
    }

    #[test]
    fn split_keeps_successes_and_failures_apart() {
        let outcomes = vec![
            (uid("a"), Ok(())),
            (uid("b"), Err(failure())),
            (uid("c"), Ok(())),
        ];
        let (done, failed) = split(outcomes);
        assert_eq!(done, vec![uid("a"), uid("c")]);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, uid("b"));
    }
}
