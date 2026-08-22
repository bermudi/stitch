//! Config module: authored (`stitch.toml`), generated (`.stitch/state.toml`),
//! and the load-time merged view.
//!
//! Submodules:
//! - [`types`] — struct/enum definitions and constants
//! - [`error`] — `ConfigError`
//! - [`paths`] — home expansion, root discovery, fragment/name validation
//! - [`state`] — atomic writes, file validation, state lock
//! - [`load`] — loading, parsing, merging, validation, normalization
//! - [`legacy`] — v0.2 migration layout and split

mod error;
mod legacy;
mod load;
mod paths;
mod state;
mod types;

pub use error::ConfigError;
pub use legacy::{LegacyConfig, split_legacy};
#[allow(unused_imports)]
pub use load::{
    ConfigSnapshot, validate_fragments, validate_globs, validate_merged, validate_merged_with_repo,
    validate_target,
};
pub use paths::{expand_home, find_root, is_safe_fragment, is_store_name};
pub use state::{StateLock, atomic_write, validate_atomic_write_target};
pub use types::{
    AUTHORED_TEMPLATE, Config, GeneratedState, GeneratedStore, GeneratedTarget, Hooks, Loaded,
    Store, WhenClause,
};

pub(crate) use load::{hash_config_bytes, revalidate_config_hash};
pub(crate) use paths::{canonical_home, canonical_target_for_comparison, normalized_target_path};
pub(crate) use state::{validate_authored_file, validate_state_file, validate_stitch_dir};

#[cfg(test)]
pub(crate) use paths::{TestHomeGuard, set_test_home, test_home_guard};
#[cfg(test)]
pub(crate) use types::TargetEntry;
