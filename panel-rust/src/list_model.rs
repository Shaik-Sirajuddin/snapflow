//! Shared in-place reconciliation for Slint `VecModel` instances.
//!
//! Stable keys are used to compute structural edits. Unchanged rows are then
//! updated with `set_row_data`, so their Slint delegate identity survives
//! list-shape changes elsewhere.

use crate::dirty::{diff_by_id, RowOp};
use slint::{Model, VecModel};

/// Reconciles `model` from `old_keys` to `new_keys` without replacing the
/// model object. Returns the structural operations for focused tests.
pub fn reconcile<T, K>(
    model: &VecModel<T>,
    old_keys: &mut Vec<K>,
    new_keys: &[K],
    new_rows: &[T],
) -> Vec<RowOp<T>>
where
    T: Clone + PartialEq + 'static,
    K: Clone + Eq,
{
    assert_eq!(
        new_keys.len(),
        new_rows.len(),
        "row keys and row data must have the same length"
    );
    // This crate builds with `panic = "abort"` (real-time render path can't
    // unwind), so a hard assert here used to take down the whole host
    // process on any desync between a persistent VecModel and its paired key
    // cache -- e.g. a caller that mutated the model directly, or two
    // reconcile() calls racing on the same model from different dirty
    // events. Self-heal instead: drop the stale key cache and rebuild the
    // model from scratch this one time, so a bug elsewhere degrades to a
    // visible one-frame flicker/rebuild rather than an app-wide abort.
    if model.row_count() != old_keys.len() {
        eprintln!(
            "panel-rust: list_model::reconcile key-cache desync (model rows: {}, key cache: {}) -- rebuilding from scratch",
            model.row_count(),
            old_keys.len()
        );
        old_keys.clear();
        for _ in 0..model.row_count() {
            model.remove(0);
        }
    }

    let ops = diff_by_id(old_keys, new_keys, new_rows);
    for op in &ops {
        match op {
            RowOp::Insert { at, row } => model.insert(*at, row.clone()),
            RowOp::Remove { at } => {
                model.remove(*at);
            }
            RowOp::Move { from, to } => {
                let row = model.remove(*from);
                model.insert(*to, row);
            }
        }
    }

    // A matching key is intentionally absent from the structural diff. Its
    // contents can still have changed, so update that row in place.
    for (index, row) in new_rows.iter().enumerate() {
        if model.row_data(index).as_ref() != Some(row) {
            model.set_row_data(index, row.clone());
        }
    }
    *old_keys = new_keys.to_vec();
    ops
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconciliation_preserves_model_identity_and_updates_rows() {
        let model = VecModel::from(vec!["a".to_owned(), "b".to_owned()]);
        let mut old_keys = vec!["a", "b"];
        let new_keys = vec!["b", "c"];
        let new_rows = vec!["B".to_owned(), "c".to_owned()];

        let ops = reconcile(&model, &mut old_keys, &new_keys, &new_rows);

        assert_eq!(ops.len(), 3);
        assert_eq!(old_keys, new_keys);
        assert_eq!(model.row_count(), 2);
        assert_eq!(model.row_data(0).as_deref(), Some("B"));
        assert_eq!(model.row_data(1).as_deref(), Some("c"));
    }

    #[test]
    fn reconciliation_handles_cold_start_and_clear() {
        let model = VecModel::default();
        let mut old_keys: Vec<String> = Vec::new();
        let new_keys = vec!["a".to_owned(), "b".to_owned()];
        let new_rows = vec![1, 2];

        reconcile(&model, &mut old_keys, &new_keys, &new_rows);
        assert_eq!(model.row_count(), 2);

        let empty_keys: Vec<String> = Vec::new();
        let empty_rows: Vec<i32> = Vec::new();
        reconcile(&model, &mut old_keys, &empty_keys, &empty_rows);
        assert_eq!(model.row_count(), 0);
        assert!(old_keys.is_empty());
    }
}
