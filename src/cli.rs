use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "stitch",
    version,
    about = "A dotfile manager that symlinks your configs into place"
)]
pub struct Cli {
    /// Path to the stitch repo to operate on. Overrides the STITCH_REPO env
    /// var and the upward cwd walk. Ignored by `init` (which is cwd-anchored).
    #[arg(long, global = true, value_name = "PATH")]
    pub repo: Option<String>,

    /// Emit structured JSON output instead of human-readable text.
    #[arg(long, global = true)]
    pub json: bool,

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

        /// Validate and execute operations from a previously captured plan
        #[arg(long, value_name = "FILE")]
        plan: Option<String>,
    },

    /// Capture an executable plan of what apply would do
    Plan {
        /// Limit captured operations to these stores (repeatable)
        #[arg(short, long = "only")]
        only: Vec<String>,

        /// Preview .bak backup behavior (what `apply --force` would do)
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

        /// Exit 14 when safe changes are needed; conflicts retain their existing codes
        #[arg(long)]
        exit_code: bool,
    },

    /// List all configured stores
    List,

    /// Add a path to stitch: move existing content into the repo and link back,
    /// or create an empty store if the path doesn't exist yet. Multiple paths
    /// trigger bulk mode (simple adds only, no per-store flags).
    Add {
        /// Target path(s) to manage (e.g. ~/.config/nvim). Multiple paths
        /// trigger bulk add mode.
        #[arg(required = true, num_args = 1..)]
        paths: Vec<String>,

        /// Override the derived store name (default: basename, leading dot
        /// stripped). Only valid with a single path.
        #[arg(short, long)]
        name: Option<String>,

        /// Files to link individually (repeatable; only when creating a new
        /// store and only with a single path)
        #[arg(short, long = "files", value_name = "FILE")]
        files: Vec<String>,

        /// Glob patterns (repeatable; only when creating a new store and only
        /// with a single path)
        #[arg(short, long = "patterns", value_name = "PATTERN")]
        patterns: Vec<String>,

        /// Create PATH as a single empty file (PATH must not exist). Only
        /// valid with a single path.
        #[arg(long)]
        file: bool,

        /// Adopt an existing regular file into an existing file-mode store.
        /// Only valid with a single path.
        #[arg(long, value_name = "STORE")]
        to: Option<String>,

        /// Register the target path with an explicit repo-relative source
        /// (the fan-in flow): one file, many names/homes. Nothing is moved or
        /// copied. Only valid with a single path.
        #[arg(long, value_name = "REPO-PATH")]
        source: Option<String>,

        /// Preview without making changes
        #[arg(long)]
        dry_run: bool,
    },

    /// Remove a store and its symlinks
    Remove {
        /// Store name
        name: String,

        /// Preview without removing anything
        #[arg(long)]
        dry_run: bool,

        /// Proceed even when other stores' `sources` reference files inside
        /// this store's directory; the referenced source files are retained
        /// in place and reported.
        #[arg(long)]
        force: bool,
    },

    /// Open stitch.toml (or an entry's repo source) in $EDITOR
    Edit {
        /// Store name or target path. Opens the repo source (the `.tmpl` for a
        /// templated entry, the plain file otherwise) — never the staged render.
        /// Omit to open `stitch.toml`.
        entry: Option<String>,

        /// Print the resolved repo source path instead of opening $EDITOR.
        /// Useful for agents that open files with their own tools.
        #[arg(long)]
        print_path: bool,
    },

    /// Run health checks
    Doctor,

    /// Scan for existing repo-pointing symlinks and import them into state
    Import {
        /// Directories to scan for links (repeatable, full depth).
        /// Default: ~ (top-level dotfiles only), ~/.config, ~/.local/share.
        #[arg(long = "scan-dir", value_name = "DIR")]
        scan_dirs: Vec<String>,

        /// Preview without writing state
        #[arg(long)]
        dry_run: bool,
    },

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

    /// Render a .tmpl to stdout (read-only — no staging write, no link touch)
    Render {
        /// Store and source file, e.g. `git/gitconfig.tmpl`.
        spec: String,
    },

    /// Show the fully-resolved desired state for this host (read-only)
    Explain {
        /// Only show stores whose `when` matches this host
        #[arg(long)]
        active_only: bool,
    },

    /// Emit the canonical agent JSON schema (docs/agent-schema.json)
    Schema,

    /// Investigate a single target path: what owns it, its source, link state
    Why {
        /// Target path to investigate (e.g. ~/.bashrc or /home/user/.config/nvim)
        target: String,
    },

    /// Show the audit log of mutating operations (.stitch/log.jsonl)
    Log {
        /// Only show the last N entries
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn init_parses() {
        let cli = parse(&["stitch", "init"]).unwrap();
        assert!(matches!(cli.command, Commands::Init));
        assert!(!cli.json);
        assert!(cli.repo.is_none());
    }

    #[test]
    fn global_flags_before_subcommand() {
        let cli = parse(&["stitch", "--json", "--repo", "/tmp/repo", "list"]).unwrap();
        assert!(cli.json);
        assert_eq!(cli.repo.as_deref(), Some("/tmp/repo"));
        assert!(matches!(cli.command, Commands::List));
    }

    #[test]
    fn global_flags_after_subcommand() {
        // clap global args work on either side
        let cli = parse(&["stitch", "list", "--json"]).unwrap();
        assert!(cli.json);
        assert!(cli.repo.is_none());
        let cli = parse(&["stitch", "list", "--repo", "/tmp/repo"]).unwrap();
        assert_eq!(cli.repo.as_deref(), Some("/tmp/repo"));
        assert!(!cli.json);
        let cli = parse(&["stitch", "list", "--json", "--repo", "/tmp/repo"]).unwrap();
        assert!(cli.json);
        assert_eq!(cli.repo.as_deref(), Some("/tmp/repo"));
        // reversed order after subcommand
        let cli = parse(&["stitch", "list", "--repo", "/tmp/repo", "--json"]).unwrap();
        assert!(cli.json);
        assert_eq!(cli.repo.as_deref(), Some("/tmp/repo"));
    }

    #[test]
    fn apply_defaults() {
        let cli = parse(&["stitch", "apply"]).unwrap();
        match cli.command {
            Commands::Apply {
                only,
                dry_run,
                force,
                plan,
            } => {
                assert!(only.is_empty());
                assert!(!dry_run);
                assert!(!force);
                assert!(plan.is_none());
            }
            _ => panic!("expected apply"),
        }
    }

    #[test]
    fn apply_with_all_flags() {
        let cli = parse(&[
            "stitch",
            "apply",
            "--only",
            "a",
            "--only",
            "b",
            "--dry-run",
            "--force",
            "--plan",
            "plan.json",
        ])
        .unwrap();
        match cli.command {
            Commands::Apply {
                only,
                dry_run,
                force,
                plan,
            } => {
                assert_eq!(only, vec!["a", "b"]);
                assert!(dry_run);
                assert!(force);
                assert_eq!(plan.as_deref(), Some("plan.json"));
            }
            _ => panic!("expected apply"),
        }
    }

    #[test]
    fn apply_only_short_flag() {
        let cli = parse(&["stitch", "apply", "-o", "shells"]).unwrap();
        match cli.command {
            Commands::Apply { only, .. } => assert_eq!(only, vec!["shells"]),
            _ => panic!(),
        }
    }

    #[test]
    fn plan_parses() {
        let cli = parse(&["stitch", "plan", "--only", "a", "--force"]).unwrap();
        match cli.command {
            Commands::Plan { only, force } => {
                assert_eq!(only, vec!["a"]);
                assert!(force);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn status_with_and_without_name() {
        let cli = parse(&["stitch", "status"]).unwrap();
        assert!(matches!(cli.command, Commands::Status { name: None }));
        let cli = parse(&["stitch", "status", "nvim"]).unwrap();
        match cli.command {
            Commands::Status { name } => assert_eq!(name.as_deref(), Some("nvim")),
            _ => panic!(),
        }
    }

    #[test]
    fn diff_parses_flags() {
        let cli = parse(&["stitch", "diff", "--exit-code", "--force", "-o", "a"]).unwrap();
        match cli.command {
            Commands::Diff {
                only,
                force,
                exit_code,
            } => {
                assert_eq!(only, vec!["a"]);
                assert!(force);
                assert!(exit_code);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn list_parses() {
        let cli = parse(&["stitch", "list"]).unwrap();
        assert!(matches!(cli.command, Commands::List));
    }

    #[test]
    fn add_parses_minimal_and_full() {
        let cli = parse(&["stitch", "add", "~/path"]).unwrap();
        match cli.command {
            Commands::Add {
                paths,
                name,
                files,
                patterns,
                file,
                to,
                source,
                dry_run,
            } => {
                assert_eq!(paths, vec!["~/path"]);
                assert!(name.is_none());
                assert!(files.is_empty());
                assert!(patterns.is_empty());
                assert!(!file);
                assert!(to.is_none());
                assert!(source.is_none());
                assert!(!dry_run);
            }
            _ => panic!(),
        }
        let cli = parse(&[
            "stitch",
            "add",
            "~/path",
            "--name",
            "s",
            "-f",
            "a",
            "-p",
            "b",
            "--file",
            "--to",
            "store",
            "--dry-run",
        ])
        .unwrap();
        match cli.command {
            Commands::Add {
                paths,
                name,
                files,
                patterns,
                file,
                to,
                source,
                dry_run,
            } => {
                assert_eq!(name.as_deref(), Some("s"));
                assert_eq!(files, vec!["a"]);
                assert_eq!(patterns, vec!["b"]);
                assert!(file);
                assert_eq!(to.as_deref(), Some("store"));
                assert!(source.is_none());
                assert!(dry_run);
                assert_eq!(paths, vec!["~/path"]);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn add_parses_bulk_multiple_paths() {
        let cli = parse(&["stitch", "add", "~/path1", "~/path2", "~/path3"]).unwrap();
        match cli.command {
            Commands::Add { paths, .. } => {
                assert_eq!(paths, vec!["~/path1", "~/path2", "~/path3"]);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn remove_parses() {
        let cli = parse(&["stitch", "remove", "mystore"]).unwrap();
        match cli.command {
            Commands::Remove {
                name,
                dry_run,
                force,
            } => {
                assert_eq!(name, "mystore");
                assert!(!dry_run);
                assert!(!force);
            }
            _ => panic!(),
        }
        let cli = parse(&["stitch", "remove", "s", "--dry-run"]).unwrap();
        match cli.command {
            Commands::Remove { dry_run, .. } => assert!(dry_run),
            _ => panic!(),
        }
    }

    #[test]
    fn edit_parses_with_and_without_entry() {
        let cli = parse(&["stitch", "edit"]).unwrap();
        match cli.command {
            Commands::Edit { entry, print_path } => {
                assert!(entry.is_none());
                assert!(!print_path);
            }
            _ => panic!(),
        }
        let cli = parse(&["stitch", "edit", "nvim/init.lua"]).unwrap();
        match cli.command {
            Commands::Edit { entry, print_path } => {
                assert_eq!(entry.as_deref(), Some("nvim/init.lua"));
                assert!(!print_path);
            }
            _ => panic!(),
        }
        let cli = parse(&["stitch", "edit", "nvim/init.lua", "--print-path"]).unwrap();
        match cli.command {
            Commands::Edit { entry, print_path } => {
                assert_eq!(entry.as_deref(), Some("nvim/init.lua"));
                assert!(print_path);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn doctor_parses() {
        let cli = parse(&["stitch", "doctor"]).unwrap();
        assert!(matches!(cli.command, Commands::Doctor));
    }

    #[test]
    fn import_parses() {
        let cli = parse(&["stitch", "import", "--scan-dir", "/tmp", "--dry-run"]).unwrap();
        match cli.command {
            Commands::Import { scan_dirs, dry_run } => {
                assert_eq!(scan_dirs, vec!["/tmp"]);
                assert!(dry_run);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn migrate_parses() {
        let cli = parse(&["stitch", "migrate", "--dry-run"]).unwrap();
        assert!(matches!(cli.command, Commands::Migrate { dry_run: true }));
    }

    #[test]
    fn prune_parses_and_alias() {
        let cli = parse(&["stitch", "prune", "--yes", "--scan-dir", "/a"]).unwrap();
        match cli.command {
            Commands::Prune {
                yes,
                scan_dirs,
                dry_run,
            } => {
                assert!(yes);
                assert_eq!(scan_dirs, vec!["/a"]);
                assert!(!dry_run);
            }
            _ => panic!(),
        }
        // alias gc
        let cli = parse(&["stitch", "gc"]).unwrap();
        assert!(matches!(cli.command, Commands::Prune { .. }));
        // short -y
        let cli = parse(&["stitch", "prune", "-y"]).unwrap();
        match cli.command {
            Commands::Prune { yes, .. } => assert!(yes),
            _ => panic!(),
        }
    }

    #[test]
    fn render_parses() {
        let cli = parse(&["stitch", "render", "git/gitconfig.tmpl"]).unwrap();
        match cli.command {
            Commands::Render { spec } => assert_eq!(spec, "git/gitconfig.tmpl"),
            _ => panic!(),
        }
    }

    #[test]
    fn explain_parses() {
        let cli = parse(&["stitch", "explain"]).unwrap();
        match cli.command {
            Commands::Explain { active_only } => assert!(!active_only),
            _ => panic!(),
        }
        let cli = parse(&["stitch", "explain", "--active-only"]).unwrap();
        match cli.command {
            Commands::Explain { active_only } => assert!(active_only),
            _ => panic!(),
        }
    }

    #[test]
    fn schema_parses() {
        let cli = parse(&["stitch", "schema"]).unwrap();
        assert!(matches!(cli.command, Commands::Schema));
    }

    #[test]
    fn why_parses() {
        let cli = parse(&["stitch", "why", "~/.bashrc"]).unwrap();
        match cli.command {
            Commands::Why { target } => assert_eq!(target, "~/.bashrc"),
            _ => panic!(),
        }
    }

    #[test]
    fn log_parses() {
        let cli = parse(&["stitch", "log"]).unwrap();
        match cli.command {
            Commands::Log { limit } => assert!(limit.is_none()),
            _ => panic!(),
        }
        let cli = parse(&["stitch", "log", "--limit", "10"]).unwrap();
        match cli.command {
            Commands::Log { limit } => assert_eq!(limit, Some(10)),
            _ => panic!(),
        }
    }

    #[test]
    fn missing_required_arg_fails() {
        assert!(parse(&["stitch", "add"]).is_err());
        assert!(parse(&["stitch", "remove"]).is_err());
        assert!(parse(&["stitch", "render"]).is_err());
    }

    #[test]
    fn unknown_subcommand_fails() {
        assert!(parse(&["stitch", "unknown"]).is_err());
    }
}
