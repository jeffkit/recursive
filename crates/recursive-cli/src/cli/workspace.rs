//! `recursive workspace` — multi-tenant workspace (project) registry (#135).
//!
//! A thin CLI over [`recursive::workspace::WorkspaceRegistry`]: register a
//! directory as a workspace, index its sessions (headers only), archive it
//! under an admission gate, and remove it without touching the disk.

use std::path::PathBuf;

use anyhow::Context;
use clap::Subcommand;
use recursive::workspace::{ArchivePolicy, StoreActivityProbe, WorkspaceRecord, WorkspaceRegistry};

#[derive(Subcommand, Debug)]
pub(crate) enum WorkspaceCmd {
    /// List registered workspaces. Archived ones are hidden unless --all.
    List {
        /// Include archived workspaces.
        #[arg(long)]
        all: bool,
    },
    /// Register a directory as a workspace (realpath is the identity key).
    Add {
        /// Directory to register.
        path: PathBuf,
        /// Display name (defaults to the directory's base name).
        #[arg(long)]
        name: Option<String>,
    },
    /// Show one workspace and its header-only session index.
    Show {
        /// Canonical id, name, or a path that resolves to the workspace.
        workspace: String,
    },
    /// Archive a workspace. Refuses if work is still running unless --stop.
    Archive {
        /// Canonical id, name, or a path that resolves to the workspace.
        workspace: String,
        /// Persist the archive first, then stop the work that can be stopped
        /// from here (a scheduled wakeup), instead of refusing when active work
        /// is present. Work this process cannot reach is reported as left
        /// running.
        #[arg(long)]
        stop: bool,
    },
    /// Clear a workspace's archive flag.
    Unarchive {
        /// Canonical id, name, or a path that resolves to the workspace.
        workspace: String,
    },
    /// Remove a workspace from the registry. Never deletes the directory or
    /// its session history.
    Remove {
        /// Canonical id, name, or a path that resolves to the workspace.
        workspace: String,
    },
    /// Refresh the header-only session index for a workspace.
    Index {
        /// Canonical id, name, or a path that resolves to the workspace.
        workspace: String,
    },
    /// Complete any interrupted registry mutation and report the result.
    Recover,
}

pub(crate) fn run(cmd: WorkspaceCmd) -> anyhow::Result<()> {
    let registry = WorkspaceRegistry::open().context("opening workspace registry")?;
    match cmd {
        WorkspaceCmd::List { all } => {
            let mut records = registry.list().context("listing workspaces")?;
            if !all {
                records.retain(|r| !r.archived);
            }
            if records.is_empty() {
                println!("No workspaces registered.");
                println!("hint: `recursive workspace add <path>` to register a project");
                return Ok(());
            }
            println!("Workspaces ({}):", records.len());
            for record in &records {
                let state = if record.archived {
                    "archived"
                } else {
                    "active"
                };
                println!(
                    "  {}  [{}]  «{}»  ({} sessions)",
                    record.id,
                    state,
                    record.name,
                    record.sessions.len()
                );
            }
            Ok(())
        }
        WorkspaceCmd::Add { path, name } => {
            let record = registry
                .create(&path, name)
                .context("registering workspace")?;
            println!("Registered {} as «{}».", record.id, record.name);
            Ok(())
        }
        WorkspaceCmd::Show { workspace } => {
            let record = registry.get(&workspace).context("looking up workspace")?;
            print_workspace(&record);
            Ok(())
        }
        WorkspaceCmd::Archive { workspace, stop } => {
            let policy = if stop {
                ArchivePolicy::StopThenArchive
            } else {
                ArchivePolicy::Admit
            };
            let probe = StoreActivityProbe;
            match registry.archive(&workspace, &probe, policy) {
                Ok(outcome) => {
                    println!("Archived {}.", outcome.workspace);
                    for item in &outcome.active_work {
                        // Only claim a stop the probe actually performed: work
                        // it cannot reach (e.g. a live turn) is left running.
                        let verb = if outcome.stopped.contains(item) {
                            "stopped"
                        } else {
                            "left running"
                        };
                        println!(
                            "  {verb} {} {} ({})",
                            item.kind.as_str(),
                            item.id,
                            item.detail
                        );
                    }
                    println!("The directory and its history were left untouched.");
                    Ok(())
                }
                Err(recursive::Error::WorkspaceActiveWork { details, .. }) => {
                    anyhow::bail!(
                        "workspace has active work: {details}\n\
                         re-run with --stop to archive first and then stop the work"
                    )
                }
                Err(e) => Err(e).context("archiving workspace"),
            }
        }
        WorkspaceCmd::Unarchive { workspace } => {
            let record = registry
                .set_archived(&workspace, false)
                .context("unarchiving workspace")?;
            println!("Unarchived {}.", record.id);
            Ok(())
        }
        WorkspaceCmd::Remove { workspace } => {
            let removed = registry.remove(&workspace).context("removing workspace")?;
            println!("Removed {} from the registry.", removed.id);
            println!("The directory and its session history were left untouched.");
            Ok(())
        }
        WorkspaceCmd::Index { workspace } => {
            let count = registry
                .index_sessions(&workspace)
                .context("indexing sessions")?;
            println!("Indexed {count} session header(s).");
            Ok(())
        }
        WorkspaceCmd::Recover => {
            let report = registry.recover().context("recovering registry")?;
            if report.is_clean() {
                println!("Registry is clean; no interrupted operations.");
            } else {
                println!(
                    "Recovered: {} applied, {} already applied, {} marker(s) cleared.",
                    report.applied, report.already_applied, report.cleared
                );
            }
            Ok(())
        }
    }
}

fn print_workspace(record: &WorkspaceRecord) {
    let state = if record.archived {
        "archived"
    } else {
        "active"
    };
    println!("{}", record.id);
    println!("  name:       {}", record.name);
    println!("  display:    {}", record.display);
    println!("  state:      {state}");
    println!("  created:    {}", record.created_at);
    if record.sessions.is_empty() {
        println!("  sessions:   (not indexed; run `recursive workspace index`)");
    } else {
        println!("  sessions ({}):", record.sessions.len());
        for header in &record.sessions {
            let label = header
                .last_prompt
                .as_deref()
                .unwrap_or(header.goal.as_str());
            println!("    {}  [{}]  {}", header.session_id, header.status, label);
        }
    }
}
