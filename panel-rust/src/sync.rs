//! `tea-slint-model` Phase 3: `sync(&Model, &ChatPanel, &[Dirty])` -- the
//! **sole** owner of pushing `Model` state into Slint `set_*` setters.
//! See `memory/rui/gen/plans/tea-slint-model/00-plan.md`.
//!
//! **Status: additive, not yet wired.** Nothing calls `sync()` yet --
//! `lib.rs`'s `refresh_*` functions remain the live path until Phase 4's
//! per-domain cutover. `ThreadListDiff`/`MessagesDiff`/`SkillsListDiff`
//! deliberately do not yet apply real `RowOp` mutations to a persistent
//! `VecModel` here (that id-keyed diff-application logic is Phase 5's
//! job, see 00-plan.md's "Known gap: list resets still break row
//! identity / animations") -- for now those arms only prove the
//! exhaustive-match/dirty-gating shape compiles and is unit-testable
//! against a headless `ChatPanel` instance.
//!
//! The match below is exhaustive with **no wildcard arm**, matching
//! `update()`'s own requirement (00-plan.md's "Exhaustiveness
//! requirement") -- a new `Dirty` variant without a handling arm here
//! must fail to compile.

use crate::dirty::Dirty;
use crate::model::Model;
use crate::ChatPanel;

pub fn sync(model: &Model, component: &ChatPanel, dirty: &[Dirty]) {
    for d in dirty {
        sync_one(model, component, d);
    }
}

fn sync_one(model: &Model, component: &ChatPanel, dirty: &Dirty) {
    match dirty {
        Dirty::Scalar(field) => sync_scalar(model, component, *field),
        Dirty::ThreadRow(_idx) => {
            // Same-shape in-place edit -- Phase 5 wires this to
            // `set_row_data` on the persistent thread VecModel instead of
            // a blanket rebuild.
        }
        Dirty::ThreadListDiff(_ops) => {
            // Phase 5: apply id-keyed RowOp::Insert/Remove/Move to the
            // persistent Rc<VecModel<ThreadItem>> via .insert()/.remove(),
            // never a full ModelRc replace (see 00-plan.md's known gap).
        }
        Dirty::MessageAppended { .. } => {}
        Dirty::MessagesDiff { .. } => {}
        Dirty::MessageStreamingDelta { .. } => {
            // Phase 6: id-keyed lookup at apply time, never a cached row
            // index (see 00-plan.md's streaming-rows known gap).
        }
        Dirty::Connection { .. } => {}
        Dirty::Error { .. } => {}
        Dirty::PendingRequest { .. } => {}
        Dirty::Terminal { .. } => {}
        Dirty::LocalTerminal => {}
        Dirty::Settings => {
            component.set_settings_scope(model.settings_scope.clone().into());
        }
        Dirty::SkillsListDiff(_ops) => {}
        Dirty::SkillRow(_idx) => {}
        Dirty::Capabilities { .. } => {}
    }
}

fn sync_scalar(model: &Model, component: &ChatPanel, field: crate::dirty::ScalarField) {
    use crate::dirty::ScalarField;
    match field {
        ScalarField::SelectedThread => {
            component.set_selected_thread(model.selected_thread as i32);
        }
        ScalarField::ComposeText => {}
        ScalarField::SettingsOpen => {
            component.set_settings_open(model.settings_open);
        }
        ScalarField::SettingsScope => {
            component.set_settings_scope(model.settings_scope.clone().into());
        }
        ScalarField::ExpandedTerminal => {}
        ScalarField::SearchQuery => {}
    }
}

// No unit tests against a live `ChatPanel` here: constructing one
// requires `slint::platform::set_platform` to have already run (the
// software-renderer platform `panel_rust_create` sets up once per
// process, see `lib.rs`'s `SpikePlatform`/`PLATFORM_WINDOW`) -- confirmed
// live, `ChatPanel::new()` panics ("No default Slint platform was
// selected") in a bare `cargo test` process with no such setup, and nothing
// else in this crate constructs one outside `panel_rust_create` either.
// This matches 00-plan.md's own Phase 3 verification wording ("live
// click-through", not a unit test) -- real coverage for `sync()` lands
// with Phase 4's real-host cutover, not here.
