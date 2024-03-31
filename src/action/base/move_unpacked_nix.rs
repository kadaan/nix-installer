use std::{
    os::unix::prelude::PermissionsExt,
    path::{Path, PathBuf},
};
use nix::unistd::{chown, Gid, Uid};

use tracing::{span, Span};
use uzers::get_group_by_name;
use walkdir::WalkDir;

use crate::action::{
    Action, ActionDescription, ActionError, ActionErrorKind, ActionTag, StatefulAction,
};
use crate::cli::CURRENT_UID;
use crate::settings::{CommonSettings, SCRATCH_DIR};

pub(crate) const DEST: &str = "/nix/";

/**
Move an unpacked Nix at `src` to `/nix`
*/
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct MoveUnpackedNix {
    unpacked_path: PathBuf,
    nix_build_group_name: String,
}

impl MoveUnpackedNix {
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn plan(settings: &CommonSettings) -> Result<StatefulAction<Self>, ActionError> {
        // Note: Do NOT try to check for the src/dest since the installer creates those
        let unpacked_path = PathBuf::from(SCRATCH_DIR);
        let nix_build_group_name = settings.nix_build_group_name.clone();

        Ok(Self {
            unpacked_path,
            nix_build_group_name,
        }.into())
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "mount_unpacked_nix")]
impl Action for MoveUnpackedNix {
    fn action_tag() -> ActionTag {
        ActionTag("move_unpacked_nix")
    }
    fn tracing_synopsis(&self) -> String {
        "Move the downloaded Nix into `/nix`".to_string()
    }

    fn tracing_span(&self) -> Span {
        span!(
            tracing::Level::DEBUG,
            "mount_unpacked_nix",
            src = tracing::field::display(self.unpacked_path.display()),
            dest = DEST,
        )
    }

    fn execute_description(&self) -> Vec<ActionDescription> {
        vec![ActionDescription::new(
            "Move the downloaded Nix into `/nix`".to_string(),
            vec![format!(
                "Nix is being downloaded to `{}` and should be in `/nix`",
                self.unpacked_path.display(),
            )],
        )]
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn execute(&mut self) -> Result<(), ActionError> {
        let Self { unpacked_path, nix_build_group_name } = self;

        // This is the `nix-$VERSION` folder which unpacks from the tarball, not a nix derivation
        let found_nix_paths = glob::glob(&format!("{}/nix-*", unpacked_path.display()))
            .map_err(|e| Self::error(MoveUnpackedNixError::from(e)))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| Self::error(MoveUnpackedNixError::from(e)))?;
        if found_nix_paths.len() != 1 {
            return Err(Self::error(ActionErrorKind::MalformedBinaryTarball));
        }
        let found_nix_path = found_nix_paths.into_iter().next().unwrap();
        let src_store = found_nix_path.join("store");
        let mut src_store_listing = tokio::fs::read_dir(src_store.clone())
            .await
            .map_err(|e| ActionErrorKind::ReadDir(src_store.clone(), e))
            .map_err(Self::error)?;
        let dest_store = Path::new(DEST).join("store");
        if dest_store.exists() {
            if !dest_store.is_dir() {
                return Err(Self::error(ActionErrorKind::PathWasNotDirectory(
                    dest_store.clone(),
                )))?;
            }
        } else {
            tokio::fs::create_dir(&dest_store)
                .await
                .map_err(|e| ActionErrorKind::CreateDirectory(dest_store.clone(), e))
                .map_err(Self::error)?;

            let gid = Some(Gid::from_raw(get_group_by_name(nix_build_group_name)
                .ok_or(ActionErrorKind::NoGroup(nix_build_group_name.to_string()))
                .map_err(Self::error)?
                .gid()));

            chown(&dest_store, Some(Uid::from_raw(*CURRENT_UID.get().unwrap())), gid)
                .map_err(|e| ActionErrorKind::Chown(dest_store.clone(), e))
                .map_err(Self::error)?;
        }

        while let Some(entry) = src_store_listing
            .next_entry()
            .await
            .map_err(|e| ActionErrorKind::ReadDir(src_store.clone(), e))
            .map_err(Self::error)?
        {
            let entry_dest = dest_store.join(entry.file_name());
            if entry_dest.exists() {
                tracing::trace!(src = %entry.path().display(), dest = %entry_dest.display(), "Removing already existing package");
                tokio::fs::remove_dir_all(&entry_dest)
                    .await
                    .map_err(|e| ActionErrorKind::Remove(entry_dest.clone(), e))
                    .map_err(Self::error)?;
            }
            tracing::trace!(src = %entry.path().display(), dest = %entry_dest.display(), "Renaming");
            tokio::fs::rename(&entry.path(), &entry_dest)
                .await
                .map_err(|e| ActionErrorKind::Rename(entry.path(), entry_dest.to_owned(), e))
                .map_err(Self::error)?;

            for entry_item in WalkDir::new(&entry_dest)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| !e.file_type().is_symlink())
            {
                let path = entry_item.path();

                let mut perms = path
                    .metadata()
                    .map_err(|e| ActionErrorKind::GetMetadata(path.to_owned(), e))
                    .map_err(Self::error)?
                    .permissions();
                perms.set_readonly(true);

                tokio::fs::set_permissions(path, perms.clone())
                    .await
                    .map_err(|e| {
                        ActionErrorKind::SetPermissions(
                            perms.mode(),
                            entry_item.path().to_owned(),
                            e,
                        )
                    })
                    .map_err(Self::error)?;
            }

            // Leave a back link where we copied from since later we may need to know which packages we actually transferred
            // eg, know which `nix` version we installed when curing a user with several versions installed
            tokio::fs::symlink(&entry_dest, entry.path())
                .await
                .map_err(|e| ActionErrorKind::Symlink(entry_dest.to_owned(), entry.path(), e))
                .map_err(Self::error)?;
        }

        Ok(())
    }

    fn revert_description(&self) -> Vec<ActionDescription> {
        vec![/* Deliberately empty -- this is a noop */]
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn revert(&mut self) -> Result<(), ActionError> {
        // Noop
        Ok(())
    }
}

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum MoveUnpackedNixError {
    #[error("Glob pattern error")]
    GlobPatternError(
        #[from]
        #[source]
        glob::PatternError,
    ),
    #[error("Glob globbing error")]
    GlobGlobError(
        #[from]
        #[source]
        glob::GlobError,
    ),
}

impl From<MoveUnpackedNixError> for ActionErrorKind {
    fn from(val: MoveUnpackedNixError) -> Self {
        ActionErrorKind::Custom(Box::new(val))
    }
}
