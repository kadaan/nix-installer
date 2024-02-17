use serde::{Deserialize, Serialize};
use tracing::{span, Span};

use std::path::PathBuf;
use nix::unistd::{chown, Uid};
use tokio::{
    fs::{remove_file, OpenOptions},
    io::AsyncWriteExt,
    process::Command,
};

use crate::{
    action::{Action, ActionDescription, ActionError, ActionErrorKind, ActionTag, StatefulAction},
    execute_command,
};

use simple_home_dir::*;
use tokio::fs::create_dir;
use crate::cli::CURRENT_UID;

/** Create a plist for a `launchctl` service to run nix-store --gc
 */
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct CreateNixGcService {
    root: PathBuf,
    path: PathBuf,
    service_label: String,
    needs_bootout: bool,
}

impl CreateNixGcService {
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn plan() -> Result<StatefulAction<Self>, ActionError> {
        let launchd_service_root = home_dir().unwrap().display().to_string() + "/Library/LaunchAgents";
        let launchd_service_path = launchd_service_root.clone() + "/org.nixos.nix-gc.plist";
        let mut this = Self {
            root: PathBuf::from(launchd_service_root),
            path: PathBuf::from(launchd_service_path),
            service_label: "org.nixos.nix-gc".into(),
            needs_bootout: false,
        };

        // If the service is currently loaded or running, we need to unload it during execute (since we will then recreate it and reload it)
        // This `launchctl` command may fail if the service isn't loaded
        let launchd_domain = format!("gui/{}", CURRENT_UID.get().unwrap());
        let mut check_loaded_command = Command::new("launchctl");
        check_loaded_command.process_group(0);
        check_loaded_command.arg("print");
        check_loaded_command.arg(format!("{}/{}", launchd_domain, this.service_label));
        tracing::trace!(
            command = format!("{:?}", check_loaded_command.as_std()),
            "Executing"
        );
        let check_loaded_output = check_loaded_command
            .output()
            .await
            .map_err(|e| ActionErrorKind::command(&check_loaded_command, e))
            .map_err(Self::error)?;
        this.needs_bootout = check_loaded_output.status.success();
        if this.needs_bootout {
            tracing::debug!(
                "Detected loaded service `{}` which needs unload before replacing `{}`",
                this.service_label,
                this.path.display(),
            );
        }

        if this.path.exists() {
            let discovered_plist: LaunchctlGcPlist =
                plist::from_file(&this.path).map_err(Self::error)?;
            let expected_plist = generate_plist(&this.service_label)
                .await
                .map_err(Self::error)?;
            if discovered_plist != expected_plist {
                tracing::trace!(
                    ?discovered_plist,
                    ?expected_plist,
                    "Parsed plists not equal"
                );
                return Err(Self::error(CreateNixGcServiceError::DifferentPlist {
                    expected: expected_plist,
                    discovered: discovered_plist,
                    path: this.path.clone(),
                }));
            }

            tracing::debug!("Creating file `{}` already complete", this.path.display());
            return Ok(StatefulAction::completed(this));
        }

        Ok(StatefulAction::uncompleted(this))
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "create_nix_gc_service")]
impl Action for CreateNixGcService {
    fn action_tag() -> ActionTag {
        ActionTag("create_nix_gc_service")
    }
    fn tracing_synopsis(&self) -> String {
        format!(
            "{maybe_unload} a `launchctl` plist to garbage collect nix store",
            maybe_unload = if self.needs_bootout {
                "Unload, then recreate"
            } else {
                "Create"
            }
        )
    }

    fn tracing_span(&self) -> Span {
        let span = span!(
            tracing::Level::DEBUG,
            "create_nix_gc_service",
            path = tracing::field::display(self.path.display()),
            buf = tracing::field::Empty,
        );

        span
    }

    fn execute_description(&self) -> Vec<ActionDescription> {
        vec![ActionDescription::new(self.tracing_synopsis(), vec![])]
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn execute(&mut self) -> Result<(), ActionError> {
        let Self {
            root,
            path,
            service_label,
            needs_bootout,
        } = self;

        if !root.exists() {
            create_dir(&root)
                .await
                .map_err(|e| ActionErrorKind::CreateDirectory(root.clone(), e))
                .map_err(Self::error)?;
        }

        let launchd_domain = format!("gui/{}", CURRENT_UID.get().unwrap());
        if *needs_bootout {
            execute_command(
                Command::new("launchctl")
                    .process_group(0)
                    .arg("bootout")
                    .arg(format!("{launchd_domain}/{service_label}")),
            )
            .await
            .map_err(Self::error)?;
        }

        let generated_plist = generate_plist(service_label).await.map_err(Self::error)?;

        let mut options = OpenOptions::new();
        options.create(true).write(true).read(true);

        let mut file = options
            .open(&path)
            .await
            .map_err(|e| Self::error(ActionErrorKind::Open(path.to_owned(), e)))?;

        let mut buf = Vec::new();
        plist::to_writer_xml(&mut buf, &generated_plist).map_err(Self::error)?;
        file.write_all(&buf)
            .await
            .map_err(|e| Self::error(ActionErrorKind::Write(path.to_owned(), e)))?;

        chown(path, Some(Uid::from_raw(*CURRENT_UID.get().unwrap())), None)
            .map_err(|e| ActionErrorKind::Chown(path.clone(), e))
            .map_err(Self::error)?;

        execute_command(
            Command::new("launchctl")
                .process_group(0)
                .arg("bootstrap")
                .arg(launchd_domain)
                .arg(path)
                .stdin(std::process::Stdio::null()),
        )
        .await
        .map_err(Self::error)?;

        Ok(())
    }

    fn revert_description(&self) -> Vec<ActionDescription> {
        vec![ActionDescription::new(
            format!("Delete file `{}`", self.path.display()),
            vec![format!("Delete file `{}`", self.path.display())],
        )]
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn revert(&mut self) -> Result<(), ActionError> {
        remove_file(&self.path)
            .await
            .map_err(|e| Self::error(ActionErrorKind::Remove(self.path.to_owned(), e)))?;

        Ok(())
    }
}

/// This function must be able to operate at both plan and execute time.
async fn generate_plist(service_label: &str) -> Result<LaunchctlGcPlist, ActionErrorKind> {
    let log_err_file_path = format!("{}/Library/Logs/nix-gc.err.log", home_dir().unwrap().display().to_string());
    let log_out_file_path = format!("{}/Library/Logs/nix-gc.log", home_dir().unwrap().display().to_string());
    let plist = LaunchctlGcPlist {
        start_calendar_interval: StartCalendarIntervalOpts {
            hour: 4,
            minute: 0,
            weekday: 7
        },
        label: service_label.into(),
        program_arguments: vec![
            "/bin/sh".into(),
            "-c".into(),
            "/bin/wait4path /nix/var/nix/profiles/default/bin/nix-store && /nix/var/nix/profiles/default/bin/nix-store --gc".into(),
        ],
        standard_error_path: log_err_file_path.into(),
        standard_out_path: log_out_file_path.into(),
    };
    Ok(plist)
}

#[derive(Deserialize, Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct LaunchctlGcPlist {
    label: String,
    program_arguments: Vec<String>,
    standard_error_path: String,
    standard_out_path: String,
    start_calendar_interval: StartCalendarIntervalOpts,
}

#[derive(Deserialize, Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct StartCalendarIntervalOpts {
    hour: i8,
    minute: i8,
    weekday: i8,
}

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum CreateNixGcServiceError {
    #[error(
        "`{path}` exists and contains content different than expected. Consider removing the file."
    )]
    DifferentPlist {
        expected: LaunchctlGcPlist,
        discovered: LaunchctlGcPlist,
        path: PathBuf,
    },
}

impl From<CreateNixGcServiceError> for ActionErrorKind {
    fn from(val: CreateNixGcServiceError) -> Self {
        ActionErrorKind::Custom(Box::new(val))
    }
}
