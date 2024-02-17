use crate::{action::{
    Action, ActionDescription, ActionError, ActionErrorKind,
    ActionState, ActionTag, StatefulAction,
}, execute_command, os::darwin::DiskUtilApfsListOutput};
use rand::Rng;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use owo_colors::OwoColorize;
use std::io::{Cursor, Error, stdout, Stdout, Write};
use tokio::process::Command;
use tracing::{span, Span};
use simple_home_dir::*;
use term::{TerminfoTerminal};
use crate::os::darwin::DiskUtilInfoOutput;

use super::CreateApfsVolume;

/**
Encrypt an APFS volume
 */
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct EncryptApfsVolume {
    disk: PathBuf,
    name: String,
}

impl EncryptApfsVolume {
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn plan(
        disk: impl AsRef<Path>,
        name: impl AsRef<str>,
        planned_create_apfs_volume: &StatefulAction<CreateApfsVolume>,
    ) -> Result<StatefulAction<Self>, ActionError> {
        let name = name.as_ref().to_owned();
        let disk = disk.as_ref().to_path_buf();

        let mut command = Command::new("/usr/bin/security");
        command.args(["find-generic-password", "-a"]);
        // command.arg(&name);
        command.arg("<VolumeUUID>");
        command.arg("-s");
        command.arg("<VolumeUUID>");
        // command.arg("Nix Store");
        command.arg("-l");
        command.arg("Nix Store");
        // command.arg(&format!("{} encryption password", disk.display()));
        command.arg("-D");
        command.arg("Encrypted volume password");
        command.process_group(0);
        command.stdin(Stdio::null());
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
        if command
            .status()
            .await
            .map_err(|e| Self::error(ActionErrorKind::command(&command, e)))?
            .success()
        {
            // The user has a password matching what we would create.
            if planned_create_apfs_volume.state == ActionState::Completed {
                // We detected a created volume already, and a password exists, so we can keep using that and skip doing anything
                return Ok(StatefulAction::completed(Self { name, disk }));
            }

            // Ask the user to remove it
            return Err(Self::error(EncryptApfsVolumeError::ExistingPasswordFound(
                name, disk,
            )));
        } else if planned_create_apfs_volume.state == ActionState::Completed {
            // The user has a volume already created, but a password not set. This means we probably can't decrypt the volume.
            return Err(Self::error(
                EncryptApfsVolumeError::MissingPasswordForExistingVolume(name, disk),
            ));
        }

        // Ensure if the disk already exists, that it's encrypted
        let output =
            execute_command(Command::new("/usr/sbin/diskutil").args(["apfs", "list", "-plist"]))
                .await
                .map_err(Self::error)?;

        let parsed: DiskUtilApfsListOutput =
            plist::from_bytes(&output.stdout).map_err(Self::error)?;
        for container in parsed.containers {
            for volume in container.volumes {
                if volume.name.as_ref() == Some(&name) {
                    if volume.encryption {
                        return Err(Self::error(
                            EncryptApfsVolumeError::ExistingVolumeNotEncrypted(name, disk),
                        ));
                    } else {
                        return Ok(StatefulAction::completed(Self { disk, name }));
                    }
                }
            }
        }

        Ok(StatefulAction::uncompleted(Self { name, disk }))
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "encrypt_volume")]
impl Action for EncryptApfsVolume {
    fn action_tag() -> ActionTag {
        ActionTag("encrypt_apfs_volume")
    }
    fn tracing_synopsis(&self) -> String {
        format!(
            "Encrypt volume `{}` on disk `{}`",
            self.name,
            self.disk.display()
        )
    }

    fn tracing_span(&self) -> Span {
        span!(
            tracing::Level::DEBUG,
            "encrypt_volume",
            disk = tracing::field::display(self.disk.display()),
        )
    }

    fn execute_description(&self) -> Vec<ActionDescription> {
        vec![ActionDescription::new(self.tracing_synopsis(), vec![])]
    }

    #[tracing::instrument(level = "debug", skip_all, fields(
        disk = %self.disk.display(),
    ))]
    async fn execute(&mut self) -> Result<(), ActionError> {
        let Self { disk: _, name } = self;

        // Generate a random password.
        let password: String = {
            const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ\
                                abcdefghijklmnopqrstuvwxyz\
                                    0123456789)(*&^%$#@!~";
            const PASSWORD_LEN: usize = 32;
            let mut rng = rand::thread_rng();

            (0..PASSWORD_LEN)
                .map(|_| {
                    let idx = rng.gen_range(0..CHARSET.len());
                    CHARSET[idx] as char
                })
                .collect()
        };

        // let disk_str = disk.to_str().expect("Could not turn disk into string"); /* Should not reasonably ever fail */

        execute_command(Command::new("/usr/sbin/diskutil").arg("mount").arg(&name))
            .await
            .map_err(Self::error)?;

        let keychain = format!("{}/Library/Keychains/login.keychain-db", home_dir().unwrap().display());

        let volume_uuid = {
            let buf = execute_command(
                Command::new("/usr/sbin/diskutil")
                    .process_group(0)
                    .args(["info", "-plist"])
                    .arg(name.as_str())
                    .stdin(std::process::Stdio::null()),
            )
                .await
                .map_err(Self::error)?
                .stdout;
            let the_plist: DiskUtilInfoOutput =
                plist::from_reader(Cursor::new(buf)).map_err(Self::error)?;

            the_plist.volume_uuid
        };

        // Add the password to the user keychain so they can unlock it later.
        execute_command(
            Command::new("/usr/bin/security").process_group(0).args([
                "add-generic-password",
                "-a",
                volume_uuid.as_str(),
                // name.as_str(),
                "-s",
                volume_uuid.as_str(),
                // "Nix Store",
                "-l",
                "Nix Store",
                // format!("{} encryption password", disk_str).as_str(),
                "-D",
                "Encrypted volume password",
                "-j",
                "Added automatically by the Nix installer",
                "-w",
                password.as_str(),
                "-T",
                "/System/Applications/Utilities/Disk Utility.app",
                "-T",
                "/System/Library/CoreServices/APFSUserAgent",
                "-T",
                "/System/Library/CoreServices/CSUserAgent",
                "-T",
                "/usr/bin/security",
                keychain.as_str(),
            ]),
        )
        .await
        .map_err(Self::error)?;

        let stdout = stdout();
        let mut term =
            term::terminfo::TerminfoTerminal::new(stdout).ok_or(Self::error(ActionErrorKind::CouldNotGetTerminal))?;
        let help_message = format!(" \n {}{}\n", "HELP: ".cyan(), "The Login keychain password is needed to configure the 'Nix Store' item with ACLs allowing APFSUserAgent to mount the 'Nix Store' volume at login.".bold());
        write_line(&mut term, help_message).map_err(|e| Self::error(ActionErrorKind::TerminalWrite(e)))?;

        let mut login_keychain_password;
        let prompt_message = format!(" {} Login Keychain Password: ", "?".cyan());
        let verify_message = format!(" {} Verify Password: ", "?".cyan());
        let error_message = format!(" {} Passwords do not match.  Try again...\n\n", "!".red());
        loop {
            login_keychain_password = prompt_password(&mut term, prompt_message.clone()).map_err(|e| Self::error(ActionErrorKind::TerminalPasswordPrompt(e)))?;
            let login_keychain_verification = prompt_password(&mut term, verify_message.clone()).map_err(|e| Self::error(ActionErrorKind::TerminalPasswordPrompt(e)))?;
            if login_keychain_password != login_keychain_verification {
                write_line(&mut term, error_message.clone()).map_err(|e| Self::error(ActionErrorKind::TerminalWrite(e)))?;
            } else {
                break;
            }
        }

        // Add additional ACLs to the keychain so that it can be used by APFSUserAgent at boot to mount the volume
        execute_command(
            Command::new("/usr/bin/security").process_group(0).args([
                "set-generic-password-partition-list",
                "-a",
                volume_uuid.as_str(),
                "-s",
                volume_uuid.as_str(),
                "-l",
                "Nix Store",
                "-D",
                "Encrypted volume password",
                "-j",
                "Added automatically by the Nix installer",
                "-k",
                login_keychain_password.as_str(),
                "-S",
                "apple-tool:,apple:",
                keychain.as_str(),
            ])
            .stdout(Stdio::piped())
            .stdin(Stdio::piped())
        )
        .await
        .map_err(Self::error)?;

        // Encrypt the mounted volume
        execute_command(Command::new("/usr/sbin/diskutil").process_group(0).args([
            "apfs",
            "encryptVolume",
            name.as_str(),
            "-user",
            "disk",
            "-passphrase",
            password.as_str(),
        ]))
        .await
        .map_err(Self::error)?;

        // execute_command(
        //     Command::new("/usr/sbin/diskutil")
        //         .process_group(0)
        //         .arg("unmount")
        //         .arg("force")
        //         .arg(&name),
        // )
        // .await
        // .map_err(Self::error)?;

        Ok(())
    }

    fn revert_description(&self) -> Vec<ActionDescription> {
        vec![ActionDescription::new(
            format!(
                "Remove encryption keys for volume `{}`",
                self.disk.display()
            ),
            vec![],
        )]
    }

    #[tracing::instrument(level = "debug", skip_all, fields(
        disk = %self.disk.display(),
    ))]
    async fn revert(&mut self) -> Result<(), ActionError> {
        let volume_uuid = {
            let buf = execute_command(
                Command::new("/usr/sbin/diskutil")
                    .process_group(0)
                    .args(["info", "-plist"])
                    .arg(self.name.as_str())
                    .stdin(std::process::Stdio::null()),
            )
                .await
                .map_err(Self::error)?
                .stdout;
            let the_plist: DiskUtilInfoOutput =
                plist::from_reader(Cursor::new(buf)).map_err(Self::error)?;

            the_plist.volume_uuid
        };

        // TODO: This seems very rough and unsafe
        execute_command(
            Command::new("/usr/bin/security").process_group(0).args([
                "delete-generic-password",
                "-a",
                volume_uuid.as_str(),
                // name.as_str(),
                "-s",
                volume_uuid.as_str(),
                // "Nix Store",
                "-l",
                "Nix Store",
                "-D",
                "Encrypted volume password",
                "-j",
                "Added automatically by the Nix installer",
            ]),
        )
        .await
        .map_err(Self::error)?;

        Ok(())
    }
}

fn write_line(term: &mut TerminfoTerminal<Stdout>, help_message: String) -> std::io::Result<()> {
    term.write_all(help_message.as_bytes())?;
    term.flush()?;
    Ok(())
}

fn prompt_password(term: &mut TerminfoTerminal<Stdout>, prompt_message: String) -> Result<String, Error> {
    loop {
        write_line(term, prompt_message.clone())?;
        // term.attr(Attr::Secure)?;
        let password = rpassword::read_password().unwrap();
        // let line = match read_line() {
        //     Ok(n) => {
        //         term.reset()?;
        //         n.unwrap_or_default()
        //     },
        //     Err(err) => {
        //         term.reset()?;
        //         return Err(err);
        //     },
        // };
        if !password.is_empty() {
            return Ok(password);
        }
    }
}

// fn read_line() -> Result<Option<String>, Error> {
//     let stdin = stdin();
//     let stdin = stdin.lock();
//     let mut lines = stdin.lines();
//     lines.next().transpose()
// }

#[derive(thiserror::Error, Debug)]
pub enum EncryptApfsVolumeError {
    #[error("The keychain has an existing password for a non-existing \"{0}\" volume on disk `{1}`, consider removing the password with `sudo security delete-generic-password  -a \"{0}\" -s \"Nix Store\" -l \"{1} encryption password\" -D \"Encrypted volume password\"`. Note that it's possible to have several passwords stored, so you may need to run this command several times until receiving the message `The specified item could not be found in the keychain.`")]
    ExistingPasswordFound(String, PathBuf),
    #[error("The keychain lacks a password for the already existing \"{0}\" volume on disk `{1}`, consider removing the volume with `diskutil apfs deleteVolume \"{0}\"` (if you receive error -69888, you may need to run `sudo launchctl bootout system/org.nixos.darwin-store` and `sudo launchctl bootout system/org.nixos.nix-daemon` first)")]
    MissingPasswordForExistingVolume(String, PathBuf),
    #[error("The existing APFS volume \"{0}\" on disk `{1}` is not encrypted but it should be, consider removing the volume with `diskutil apfs deleteVolume \"{0}\"` (if you receive error -69888, you may need to run `sudo launchctl bootout system/org.nixos.darwin-store` and `sudo launchctl bootout system/org.nixos.nix-daemon` first)")]
    ExistingVolumeNotEncrypted(String, PathBuf),
}

impl From<EncryptApfsVolumeError> for ActionErrorKind {
    fn from(val: EncryptApfsVolumeError) -> Self {
        ActionErrorKind::Custom(Box::new(val))
    }
}
