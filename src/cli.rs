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

    /// Add a path to stitch: move existing content into the repo and link back,
    /// or create an empty store if the path doesn't exist yet
    Add {
        /// Target path to manage (e.g. ~/.config/nvim)
        path: String,

        /// Override the derived store name (default: basename, leading dot stripped)
        #[arg(short, long)]
        name: Option<String>,

        /// Files to link individually (repeatable; only when creating a new store)
        #[arg(short, long = "files", value_name = "FILE")]
        files: Vec<String>,

        /// Glob patterns (repeatable; only when creating a new store)
        #[arg(short, long = "patterns", value_name = "PATTERN")]
        patterns: Vec<String>,

        /// Preview without making changes
        #[arg(long)]
        dry_run: bool,
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

    /// Remove symlinks pointing into this repo that no store references
    #[command(alias = "gc")]
    Prune {
        /// Directories to scan for orphaned links (repeatable, full depth).
        /// Default: ~ (top-level dotfiles only), ~/.config, ~/.local/share.
        #[arg(long = "scan-dir", value_name = "DIR")]
        scan_dirs: Vec<String>,

        /// Preview only — list what would be removed (also the default behavior)
        #[arg(long)]
        dry_run: bool,

        /// Remove the orphaned links (default is list-only)
        #[arg(short = 'y', long = "yes")]
        yes: bool,
    },
}
