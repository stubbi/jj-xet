//! `jj` with large files stored in Xet.
//!
//! This is the stock jj CLI plus an `xet` working copy type. In a workspace
//! using it, large files are chunked into a Xet CAS on snapshot and only
//! pointer files are committed; checkout reconstructs them.

mod oid_map;
mod pointer;
mod remote;
mod vendor;
mod xet;

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::Command;

use futures::AsyncReadExt as _;
use jj_cli::cli_util::BoxedAsyncCliDispatch;
use jj_cli::cli_util::CliRunner;
use jj_cli::cli_util::CommandHelper;
use jj_cli::cli_util::RevisionArg;
use jj_cli::command_error::CommandError;
use jj_cli::command_error::internal_error;
use jj_cli::command_error::user_error;
use jj_cli::git_util::absolute_git_url;
use jj_cli::ui::Ui;
use jj_lib::backend::TreeValue;
use jj_lib::config::ConfigGetResultExt as _;
use jj_lib::file_util::persist_temp_file;
use jj_lib::git::get_git_repo;
use jj_lib::lock::FileLock;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::workspace::WorkingCopyFactories;
use tempfile::NamedTempFile;

use crate::pointer::MAX_POINTER_SIZE;
use crate::pointer::Pointer;
use crate::vendor::local_working_copy::LocalWorkingCopy;
use crate::vendor::local_working_copy::LocalWorkingCopyFactory;
use crate::vendor::local_working_copy::LockedLocalWorkingCopy;
use crate::xet::XetFilter;

#[derive(clap::Parser, Clone, Debug)]
enum CustomCommand {
    #[command(subcommand)]
    Xet(XetCommand),
}

/// Store large files in Xet
#[derive(clap::Subcommand, Clone, Debug)]
enum XetCommand {
    Init(InitArgs),
    Status(StatusArgs),
}

/// Switch this workspace to the Xet working copy
///
/// From now on, files of at least `xet.min-size` bytes (default 1 MiB) or
/// matching `xet.patterns` are stored in Xet and committed as pointer files.
/// Pointer files already on disk, e.g. after `jj git clone`, are replaced by
/// their contents. Run it again to retry fetching contents.
#[derive(clap::Args, Clone, Debug)]
struct InitArgs {}

/// List files stored in Xet
#[derive(clap::Args, Clone, Debug)]
struct StatusArgs {
    /// The revision to list
    #[arg(long, short, default_value = "@")]
    revision: RevisionArg,
}

async fn run_custom_command(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    command: CustomCommand,
) -> Result<(), CommandError> {
    match command {
        CustomCommand::Xet(XetCommand::Init(InitArgs {})) => cmd_init(ui, command_helper).await,
        CustomCommand::Xet(XetCommand::Status(args)) => cmd_status(ui, command_helper, &args).await,
    }
}

async fn cmd_init(ui: &mut Ui, command_helper: &CommandHelper) -> Result<(), CommandError> {
    // Don't snapshot with the old working copy type, which would commit any
    // new large files directly.
    let workspace_command = command_helper.workspace_helper_no_snapshot(ui).await?;
    // Fail on bad config before converting, so the workspace stays loadable.
    let filter = XetFilter::from_settings(workspace_command.settings())?;
    let state_dir = workspace_command
        .workspace_root()
        .join(".jj")
        .join("working_copy");
    let type_path = state_dir.join("type");
    let old_type = {
        // The same lock the working copy takes, so no concurrent jj process
        // snapshots with the old type while we switch.
        let _lock = FileLock::lock(state_dir.join("working_copy.lock")).map_err(internal_error)?;
        let old_type = fs::read_to_string(&type_path).map_err(internal_error)?;
        match old_type.as_str() {
            LocalWorkingCopy::NAME => {}
            "local" => {
                // The vendored working copy shares the stock state format.
                // Written atomically: a torn type file makes the workspace
                // unloadable.
                let mut temp_file = NamedTempFile::new_in(&state_dir).map_err(internal_error)?;
                temp_file
                    .write_all(LocalWorkingCopy::NAME.as_bytes())
                    .map_err(internal_error)?;
                persist_temp_file(temp_file, &type_path).map_err(internal_error)?;
            }
            other => {
                return Err(user_error(format!(
                    "Cannot convert a working copy of type '{other}'"
                )));
            }
        }
        old_type
    };
    drop(workspace_command);

    let mut workspace_command = command_helper.workspace_helper(ui).await?;
    let op_id = workspace_command.repo().op_id().clone();
    let (mut locked_ws, _wc_commit) = workspace_command.start_working_copy_mutation().await?;
    let locked_wc = locked_ws
        .locked_wc()
        .downcast_mut::<LockedLocalWorkingCopy>()
        .ok_or_else(|| internal_error("Working copy is not the Xet working copy"))?;
    let paths = locked_wc
        .materialize_xet_pointers()
        .map_err(internal_error)?;
    locked_ws.finish(op_id).await?;

    if old_type != LocalWorkingCopy::NAME {
        writeln!(
            ui.status(),
            "Switched this workspace to the Xet working copy (storage: {})",
            filter.location()
        )?;
    }
    for path in &paths {
        writeln!(
            ui.status(),
            "Fetched {}",
            workspace_command.format_file_path(path)
        )?;
    }
    Ok(())
}

async fn cmd_status(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    args: &StatusArgs,
) -> Result<(), CommandError> {
    let workspace_command = command_helper.workspace_helper(ui).await?;
    let commit = workspace_command
        .resolve_single_rev(ui, &args.revision)
        .await?;
    let store = workspace_command.repo().store();
    // Pointers are small, so with the git backend, object headers rule out
    // most files without reading them.
    let git_repo = get_git_repo(store).ok();
    let mut formatter = ui.stdout_formatter();
    for (path, value) in commit.tree().entries() {
        let value = value?;
        let is_conflict = !value.is_resolved();
        for id in value.adds().flatten().filter_map(|value| match value {
            TreeValue::File { id, .. } => Some(id),
            _ => None,
        }) {
            if let Some(repo) = &git_repo
                && let Ok(oid) = gix::ObjectId::try_from(id.as_bytes())
                && repo
                    .find_header(oid)
                    .is_ok_and(|header| header.size() >= MAX_POINTER_SIZE as u64)
            {
                continue;
            }
            let mut head = vec![];
            store
                .read_file(&path, id)
                .await?
                .take(MAX_POINTER_SIZE as u64)
                .read_to_end(&mut head)
                .await
                .map_err(internal_error)?;
            if let Some(pointer) = Pointer::parse(&head) {
                writeln!(
                    formatter,
                    "{} {:>12} {}{}",
                    &pointer.oid.as_str()[..12],
                    pointer.size,
                    workspace_command.format_file_path(&path),
                    if is_conflict { " (conflict side)" } else { "" }
                )?;
            }
        }
    }
    Ok(())
}

/// Mirrors how `jj git clone` names the destination when none is given.
fn clone_destination_for_source(source: &str) -> Option<&str> {
    let destination = source.strip_suffix(".git").unwrap_or(source);
    let destination = destination.strip_suffix('/').unwrap_or(destination);
    destination
        .rsplit_once(&['/', '\\', ':'][..])
        .map(|(_, name)| name)
}

fn creates_workspace(command_helper: &CommandHelper) -> bool {
    matches!(
        command_helper
            .matches()
            .subcommand()
            .map(|(name, sub)| (name, sub.subcommand_name())),
        Some(("git", Some("init" | "clone")) | ("workspace", Some("add")))
    )
}

/// The workspace that this command creates, if any.
fn created_workspace(command_helper: &CommandHelper) -> Option<PathBuf> {
    let arg = |matches: &clap::ArgMatches, id: &str| {
        matches.try_get_one::<String>(id).ok().flatten().cloned()
    };
    let destination = match command_helper.matches().subcommand()? {
        ("git", git) => match git.subcommand()? {
            ("init", init) => arg(init, "destination"),
            ("clone", clone) => arg(clone, "destination").or_else(|| {
                // Same derivation as `jj git clone`, which resolves local
                // paths first.
                let source = absolute_git_url(command_helper.cwd(), &arg(clone, "source")?).ok()?;
                clone_destination_for_source(&source).map(str::to_owned)
            }),
            _ => None,
        },
        ("workspace", workspace) => match workspace.subcommand()? {
            ("add", add) => arg(add, "destination"),
            _ => None,
        },
        _ => None,
    }?;
    Some(command_helper.cwd().join(destination))
}

/// Converts workspaces right after `jj git init`, `jj git clone` and
/// `jj workspace add` create them, unless `xet.auto-init` is false.
async fn auto_init(
    ui: &mut Ui,
    command_helper: &CommandHelper,
    old_dispatch: BoxedAsyncCliDispatch<'_>,
) -> Result<(), CommandError> {
    old_dispatch.call(ui, command_helper).await?;
    if !creates_workspace(command_helper) {
        return Ok(());
    }
    let enabled = command_helper
        .settings()
        .get_bool("xet.auto-init")
        .optional()?
        .unwrap_or(true);
    if !enabled {
        return Ok(());
    }
    let Some(root) = created_workspace(command_helper) else {
        // Never silently leave a new workspace committing large files raw.
        return Err(user_error(
            "The workspace was created, but jj-xet could not tell where. Run `jj xet init` in it",
        ));
    };
    // A separate process loads the new workspace like any other command.
    let status = Command::new(std::env::current_exe().map_err(internal_error)?)
        .args(["xet", "init"])
        .current_dir(&root)
        .status()
        .map_err(internal_error)?;
    if !status.success() {
        return Err(user_error(format!(
            "The workspace was created, but switching it to Xet failed. Run `jj xet init` in {}",
            root.display()
        )));
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    let mut working_copy_factories = WorkingCopyFactories::new();
    working_copy_factories.insert(
        LocalWorkingCopy::NAME.to_owned(),
        Box::new(LocalWorkingCopyFactory {}),
    );
    CliRunner::init()
        .add_working_copy_factories(working_copy_factories)
        .add_dispatch_hook(auto_init)
        .add_subcommand(run_custom_command)
        .run()
        .into()
}
