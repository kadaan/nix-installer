use std::io::Cursor;
use std::path::{Path, PathBuf};

use tokio::process::Command;
use tracing::{span, Span};

use crate::action::{ActionError, ActionTag, StatefulAction};
use crate::execute_command;

use crate::action::{Action, ActionDescription};
use crate::os::darwin::DiskUtilInfoOutput;

/**
Mount an APFS volume
 */
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct MountApfsVolume {
    disk: PathBuf,
    name: String,
}

impl MountApfsVolume {
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn plan(
        disk: impl AsRef<Path>,
        name: String,
    ) -> Result<StatefulAction<Self>, ActionError> {
        let disk = disk.as_ref().to_owned();
        Ok(Self { disk, name }.into())
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "unmount_volume")]
impl Action for MountApfsVolume {
    fn action_tag() -> ActionTag {
        ActionTag("mount_apfs_volume")
    }
    fn tracing_synopsis(&self) -> String {
        format!("Mount the `{}` APFS volume", self.name)
    }

    fn tracing_span(&self) -> Span {
        span!(
            tracing::Level::DEBUG,
            "mount_volume",
            disk = tracing::field::display(self.disk.display()),
            name = self.name,
        )
    }

    fn execute_description(&self) -> Vec<ActionDescription> {
        vec![ActionDescription::new(self.tracing_synopsis(), vec![])]
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn execute(&mut self) -> Result<(), ActionError> {
        let Self { disk: _, name } = self;

        let currently_unmounted = {
            let buf = execute_command(
                Command::new("/usr/sbin/diskutil")
                    .process_group(0)
                    .args(["info", "-plist"])
                    .arg(&name)
                    .stdin(std::process::Stdio::null()),
            )
            .await
            .map_err(Self::error)?
            .stdout;
            let the_plist: DiskUtilInfoOutput =
                plist::from_reader(Cursor::new(buf)).map_err(Self::error)?;

            the_plist.mount_point.is_none()
        };

        if !currently_unmounted {
            execute_command(
                Command::new("/usr/sbin/diskutil")
                    .process_group(0)
                    .args(["mount"])
                    .arg(name)
                    .stdin(std::process::Stdio::null()),
            )
            .await
            .map_err(Self::error)?;
        } else {
            tracing::debug!("Volume was already mounted, can skip mounting")
        }

        Ok(())
    }

    fn revert_description(&self) -> Vec<ActionDescription> {
        vec![ActionDescription::new(self.tracing_synopsis(), vec![])]
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn revert(&mut self) -> Result<(), ActionError> {
        let Self { disk: _, name } = self;

        let currently_unmounted = {
            let buf = execute_command(
                Command::new("/usr/sbin/diskutil")
                    .process_group(0)
                    .args(["info", "-plist"])
                    .arg(&name)
                    .stdin(std::process::Stdio::null()),
            )
            .await
            .map_err(Self::error)?
            .stdout;
            let the_plist: DiskUtilInfoOutput =
                plist::from_reader(Cursor::new(buf)).map_err(Self::error)?;

            the_plist.mount_point.is_none()
        };

        if !currently_unmounted {
            execute_command(
                Command::new("/usr/sbin/diskutil")
                    .process_group(0)
                    .args(["mount"])
                    .arg(name)
                    .stdin(std::process::Stdio::null()),
            )
            .await
            .map_err(Self::error)?;
        } else {
            tracing::debug!("Volume was already mounted, can skip mounting")
        }

        Ok(())
    }
}
