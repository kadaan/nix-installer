use crate::action::base::{create_or_insert_into_file, CreateDirectory, CreateOrInsertIntoFile};
use crate::action::{Action, ActionDescription, ActionError, ActionErrorKind, ActionTag, StatefulAction};
use crate::planner::ShellProfileLocations;

use std::path::Path;
use simple_home_dir::home_dir;
use tokio::task::JoinSet;
use tracing::{span, Instrument, Span};
use crate::cli::CURRENT_USERNAME;

const PROFILE_NIX_FILE_SHELL: &str = "/nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh";
// const PROFILE_NIX_FILE_FISH: &str = "/nix/var/nix/profiles/default/etc/profile.d/nix-daemon.fish";

/**
Configure any detected shell profiles to include Nix support
 */
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct ConfigureShellProfile {
    locations: ShellProfileLocations,
    create_directories: Vec<StatefulAction<CreateDirectory>>,
    create_or_insert_into_files: Vec<StatefulAction<CreateOrInsertIntoFile>>,
}

impl ConfigureShellProfile {
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn plan(
        locations: ShellProfileLocations,
    ) -> Result<StatefulAction<Self>, ActionError> {
        let mut create_or_insert_files = Vec::default();
        let mut create_directories = Vec::default();

        let shell_buf = format!(
            "\n\
            # Nix\n\
            if [ -e '{PROFILE_NIX_FILE_SHELL}' ]; then\n\
            {inde}. '{PROFILE_NIX_FILE_SHELL}'\n\
            fi\n\
            # End Nix\n
        \n",
            inde = "    ", // indent
        );

        let zshrc_content = String::from("[[ -d \"${HOME}/.zshrc.d\" ]] && for zshrc in \"${HOME}\"/.zshrc.d/.*; source \"$zshrc\"");
        let zshrc_path_str = format!("{}/.zshrc", home_dir().unwrap().display().to_string());
        let zshrc_path = Path::new(zshrc_path_str.as_str().into());

        if zshrc_path.exists() {
            let zshrc_buf = tokio::fs::read_to_string(&zshrc_path)
                .await
                .map_err(|e| Self::error(ActionErrorKind::Read(zshrc_path.to_path_buf(), e)))?;

            if ! zshrc_buf.contains(&zshrc_content) {
                create_or_insert_files.push(
                    CreateOrInsertIntoFile::plan(
                        zshrc_path,
                        None,
                        None,
                        0o644,
                        zshrc_content,
                        create_or_insert_into_file::Position::End,
                    )
                    .await
                    .map_err(Self::error)?,
                );
            }
        }

        let path = format!("{}/.zshrc.d/.nixrc", home_dir().unwrap().display().to_string());
        let profile_target_path = Path::new(path.as_str().into());
        if let Some(parent) = profile_target_path.parent() {
            // Some tools (eg `nix-darwin`) create symlinks to these files, don't write to them if that's the case.
            if !profile_target_path.is_symlink() {
                if !parent.exists() {
                    create_directories.push(
                        CreateDirectory::plan(
                            parent,
                            Some(CURRENT_USERNAME.get().unwrap().to_string()),
                            Some(String::from("staff")),
                            0o0755,
                            false)
                        .await
                        .map_err(Self::error)?,
                    );
                }

                create_or_insert_files.push(
                    CreateOrInsertIntoFile::plan(
                        profile_target_path,
                        Some(CURRENT_USERNAME.get().unwrap().to_string()),
                        Some(String::from("staff")),
                        0o644,
                        shell_buf.to_string(),
                        create_or_insert_into_file::Position::Beginning,
                    )
                    .await
                    .map_err(Self::error)?,
                );
            }
        }

        Ok(Self {
            locations,
            create_directories,
            create_or_insert_into_files: create_or_insert_files,
        }
        .into())
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "configure_shell_profile")]
impl Action for ConfigureShellProfile {
    fn action_tag() -> ActionTag {
        ActionTag("configure_shell_profile")
    }
    fn tracing_synopsis(&self) -> String {
        "Configure the shell profiles".to_string()
    }

    fn tracing_span(&self) -> Span {
        span!(tracing::Level::DEBUG, "configure_shell_profile",)
    }

    fn execute_description(&self) -> Vec<ActionDescription> {
        vec![ActionDescription::new(
            self.tracing_synopsis(),
            vec!["Update shell profiles to import Nix".to_string()],
        )]
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn execute(&mut self) -> Result<(), ActionError> {
        for create_directory in &mut self.create_directories {
            create_directory.try_execute().await?;
        }

        let mut set = JoinSet::new();
        let mut errors = vec![];

        for (idx, create_or_insert_into_file) in
            self.create_or_insert_into_files.iter_mut().enumerate()
        {
            let span = tracing::Span::current().clone();
            let mut create_or_insert_into_file_clone = create_or_insert_into_file.clone();
            let _abort_handle = set.spawn(async move {
                create_or_insert_into_file_clone
                    .try_execute()
                    .instrument(span)
                    .await
                    .map_err(Self::error)?;
                Result::<_, ActionError>::Ok((idx, create_or_insert_into_file_clone))
            });
        }

        while let Some(result) = set.join_next().await {
            match result {
                Ok(Ok((idx, create_or_insert_into_file))) => {
                    self.create_or_insert_into_files[idx] = create_or_insert_into_file
                },
                Ok(Err(e)) => errors.push(e),
                Err(e) => return Err(Self::error(e))?,
            };
        }

        if !errors.is_empty() {
            if errors.len() == 1 {
                return Err(Self::error(errors.into_iter().next().unwrap()))?;
            } else {
                return Err(Self::error(ActionErrorKind::MultipleChildren(
                    errors.into_iter().collect(),
                )));
            }
        }

        Ok(())
    }

    fn revert_description(&self) -> Vec<ActionDescription> {
        vec![ActionDescription::new(
            "Unconfigure the shell profiles".to_string(),
            vec!["Update shell profiles to no longer import Nix".to_string()],
        )]
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn revert(&mut self) -> Result<(), ActionError> {
        let mut set = JoinSet::new();
        let mut errors = vec![];

        for (idx, create_or_insert_into_file) in
            self.create_or_insert_into_files.iter_mut().enumerate()
        {
            let mut create_or_insert_file_clone = create_or_insert_into_file.clone();
            let _abort_handle = set.spawn(async move {
                create_or_insert_file_clone.try_revert().await?;
                Result::<_, _>::Ok((idx, create_or_insert_file_clone))
            });
        }

        while let Some(result) = set.join_next().await {
            match result {
                Ok(Ok((idx, create_or_insert_into_file))) => {
                    self.create_or_insert_into_files[idx] = create_or_insert_into_file
                },
                Ok(Err(e)) => errors.push(e),
                // This is quite rare and generally a very bad sign.
                Err(e) => return Err(e).map_err(|e| Self::error(ActionErrorKind::from(e)))?,
            };
        }

        for create_directory in self.create_directories.iter_mut() {
            if let Err(err) = create_directory.try_revert().await {
                errors.push(err);
            }
        }

        if errors.is_empty() {
            Ok(())
        } else if errors.len() == 1 {
            Err(errors
                .into_iter()
                .next()
                .expect("Expected 1 len Vec to have at least 1 item"))
        } else {
            Err(Self::error(ActionErrorKind::MultipleChildren(errors)))
        }
    }
}
