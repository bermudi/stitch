//! Store subsystem: apply execution, plan conversion, shared resolution,
//! status, and doctor. Re-export facade preserves the previous
//! `crate::store::*` import surface so callers outside this module are
//! unaffected by the split.

mod apply;
mod doctor;
mod plan_compute;
mod resolve;
mod status;

pub use apply::{
    ApplyAction, ApplyOpts, apply_all, apply_store, compute_plan, has_active_template_sources,
    store_target_dirs, sweep_boundaries,
};
pub(crate) use apply::{apply_added_plain_file, preflight_add_target, store_resolves_source};

pub use doctor::{DoctorFinding, DoctorResult, Severity, doctor};

pub(crate) use resolve::{
    LinkTargets, check_link_name_collisions, check_link_path_collisions,
    collect_reconciliation_keeps, collect_store_link_targets, resolve_link_source,
    resolve_target_names,
};

pub use status::{StatusEntry, status_all};
