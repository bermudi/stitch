use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "stitch",
    version,
    about = "A dotfile manager that symlinks your configs into place"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize a new stitch config in the current directory
    Init,

    /// Reconcile symlinks to match config
    Apply {
        /// Only apply these stores (repeatable)
        #[arg(short, long = "only")]
        only: Vec<String>,

        /// Preview without making changes
        #[arg(long)]
        dry_run: bool,

        /// Auto-create .bak backups for conflicts
        #[arg(long)]
        force: bool,
    },

    /// Show symlink state for all stores
    Status {
        /// Show status for a specific store
        name: Option<String>,
    },

    /// Preview what apply would do
    Diff {
        /// Only diff these stores (repeatable)
        #[arg(short, long = "only")]
        only: Vec<String>,

        /// Preview .bak backup behavior (what `apply --force` would do)
        #[arg(long)]
        force: bool,
    },

    /// List all configured stores
    List,

    /// Move an existing file/dir into the repo and symlink back
    Adopt {
        /// Path to adopt
        path: String,

        /// Override the derived store name
        #[arg(short, long)]
        name: Option<String>,

        /// Preview without making changes
        #[arg(long)]
        dry_run: bool,
    },

    /// Create a new store entry
    Add {
        /// Store name
        name: String,

        /// Target path (or pass positionally)
        #[arg(group = "target_input")]
        target: Option<String>,

        /// Target path (or pass positionally)
        #[arg(short, long = "target", group = "target_input", value_name = "TARGET")]
        target_flag: Option<String>,

        /// Files to link individually (repeatable)
        #[arg(short, long = "files", value_name = "FILE")]
        files: Vec<String>,

        /// Glob patterns (repeatable)
        #[arg(short, long = "patterns", value_name = "PATTERN")]
        patterns: Vec<String>,
    },

    /// Remove a store and its symlinks
    Remove {
        /// Store name
        name: String,
    },

    /// Open config in $EDITOR
    Edit,

    /// Run health checks
    Doctor,

    /// Split a v0.2 .stitch/config.toml into stitch.toml + .stitch/state.toml
    Migrate {
        /// Preview the planned files without writing
        #[arg(long)]
        dry_run: bool,
    },
}
