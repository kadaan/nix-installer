use tracing::{span, Span};

use crate::action::base::create_or_merge_nix_config::CreateOrMergeNixConfigError;
use crate::action::base::{CreateDirectory, CreateOrMergeNixConfig};
use crate::action::{
    Action, ActionDescription, ActionError, ActionErrorKind, ActionTag, StatefulAction,
};
use crate::parse_ssl_cert;
use crate::settings::{CommonSettings, UrlOrPathOrString};
use indexmap::map::Entry;
use std::path::PathBuf;
use crate::cli::CURRENT_USERNAME;

const NIX_CONF_FOLDER: &str = "/etc/nix";
const NIX_CONF: &str = "/etc/nix/nix.conf";

/**
Place the `/etc/nix.conf` file
 */
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct PlaceNixConfiguration {
    create_directory: StatefulAction<CreateDirectory>,
    create_or_merge_nix_config: StatefulAction<CreateOrMergeNixConfig>,
}

impl PlaceNixConfiguration {
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn plan(settings: &CommonSettings) -> Result<StatefulAction<Self>, ActionError> {
        let nix_build_group_name = settings.nix_build_group_name.clone();
        let proxy = settings.proxy.clone();
        let ssl_cert_file = settings.ssl_cert_file.clone();
        let extra_conf = settings.extra_conf.clone();
        let force = settings.force;
        let mut extra_conf_text = vec![];
        for extra in extra_conf {
            let buf = match &extra {
                UrlOrPathOrString::Url(url) => match url.scheme() {
                    "https" | "http" => {
                        let mut buildable_client = reqwest::Client::builder();
                        if let Some(proxy) = &proxy {
                            buildable_client = buildable_client.proxy(
                                reqwest::Proxy::all(proxy.clone())
                                    .map_err(ActionErrorKind::Reqwest)
                                    .map_err(Self::error)?,
                            )
                        }
                        if let Some(ssl_cert_file) = &ssl_cert_file {
                            let ssl_cert =
                                parse_ssl_cert(ssl_cert_file).await.map_err(Self::error)?;
                            buildable_client = buildable_client.add_root_certificate(ssl_cert);
                        }
                        let client = buildable_client
                            .build()
                            .map_err(ActionErrorKind::Reqwest)
                            .map_err(Self::error)?;
                        let req = client
                            .get(url.clone())
                            .build()
                            .map_err(ActionErrorKind::Reqwest)
                            .map_err(Self::error)?;
                        let res = client
                            .execute(req)
                            .await
                            .map_err(ActionErrorKind::Reqwest)
                            .map_err(Self::error)?;
                        res.text()
                            .await
                            .map_err(ActionErrorKind::Reqwest)
                            .map_err(Self::error)?
                    },
                    "file" => tokio::fs::read_to_string(url.path())
                        .await
                        .map_err(|e| ActionErrorKind::Read(PathBuf::from(url.path()), e))
                        .map_err(Self::error)?,
                    _ => return Err(Self::error(ActionErrorKind::UnknownUrlScheme)),
                },
                UrlOrPathOrString::Path(path) => tokio::fs::read_to_string(path)
                    .await
                    .map_err(|e| ActionErrorKind::Read(PathBuf::from(path), e))
                    .map_err(Self::error)?,
                UrlOrPathOrString::String(string) => string.clone(),
            };
            extra_conf_text.push(buf)
        }

        let extra_conf = extra_conf_text.join("\n");
        let mut nix_config = nix_config_parser::NixConfig::parse_string(extra_conf, None)
            .map_err(CreateOrMergeNixConfigError::ParseNixConfig)
            .map_err(Self::error)?;
        let settings = nix_config.settings_mut();

        settings.insert("build-users-group".to_string(), nix_build_group_name.clone());
        let experimental_features = ["nix-command", "flakes", "repl-flake"];
        match settings.entry("experimental-features".to_string()) {
            Entry::Occupied(mut slot) => {
                let slot_mut = slot.get_mut();
                for experimental_feature in experimental_features {
                    if !slot_mut.contains(experimental_feature) {
                        *slot_mut += " ";
                        *slot_mut += experimental_feature;
                    }
                }
            },
            Entry::Vacant(slot) => {
                let _ = slot.insert(experimental_features.join(" "));
            },
        };

        // https://github.com/DeterminateSystems/nix-installer/issues/449#issuecomment-1551782281
        #[cfg(not(target_os = "macos"))]
        settings.insert("auto-optimise-store".to_string(), "true".to_string());

        settings.insert(
            "bash-prompt-prefix".to_string(),
            "(nix:$name)\\040".to_string(),
        );
        settings.insert("max-jobs".to_string(), "auto".to_string());
        if let Some(ssl_cert_file) = ssl_cert_file {
            let ssl_cert_file_canonical = ssl_cert_file
                .canonicalize()
                .map_err(|e| Self::error(ActionErrorKind::Canonicalize(ssl_cert_file, e)))?;
            settings.insert(
                "ssl-cert-file".to_string(),
                ssl_cert_file_canonical.display().to_string(),
            );
        }
        settings.insert(
            "extra-nix-path".to_string(),
            "nixpkgs=flake:nixpkgs".to_string(),
        );
        settings.insert(
            "upgrade-nix-store-path-url".to_string(),
            "https://install.determinate.systems/nix-upgrade/stable/universal".to_string(),
        );
        settings.insert(
            "keep-derivations".to_string(),
            "false".to_string(),
        );
        settings.insert(
            "keep-outputs".to_string(),
            "false".to_string(),
        );

        let create_directory = CreateDirectory::plan(NIX_CONF_FOLDER, CURRENT_USERNAME.get().unwrap().to_string(), nix_build_group_name.clone(), 0o0755, force)
            .await
            .map_err(Self::error)?;
        let create_or_merge_nix_config = CreateOrMergeNixConfig::plan(NIX_CONF, nix_config)
            .await
            .map_err(Self::error)?;
        Ok(Self {
            create_directory,
            create_or_merge_nix_config,
        }
        .into())
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "place_nix_configuration")]
impl Action for PlaceNixConfiguration {
    fn action_tag() -> ActionTag {
        ActionTag("place_nix_configuration")
    }
    fn tracing_synopsis(&self) -> String {
        format!("Place the Nix configuration in `{NIX_CONF}`")
    }

    fn tracing_span(&self) -> Span {
        span!(tracing::Level::DEBUG, "place_nix_configuration",)
    }

    fn execute_description(&self) -> Vec<ActionDescription> {
        let Self {
            create_or_merge_nix_config,
            create_directory,
        } = self;

        let mut explanation = vec![
            "This file is read by the Nix daemon to set its configuration options at runtime."
                .to_string(),
        ];

        if let Some(val) = create_directory.describe_execute().first() {
            explanation.push(val.description.clone())
        }
        for val in create_or_merge_nix_config.describe_execute().iter() {
            explanation.push(val.description.clone())
        }

        vec![ActionDescription::new(self.tracing_synopsis(), explanation)]
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn execute(&mut self) -> Result<(), ActionError> {
        self.create_directory
            .try_execute()
            .await
            .map_err(Self::error)?;
        self.create_or_merge_nix_config
            .try_execute()
            .await
            .map_err(Self::error)?;

        Ok(())
    }

    fn revert_description(&self) -> Vec<ActionDescription> {
        vec![ActionDescription::new(
            format!("Remove the Nix configuration in `{NIX_CONF}`"),
            vec![
                "This file is read by the Nix daemon to set its configuration options at runtime."
                    .to_string(),
            ],
        )]
    }

    #[tracing::instrument(level = "debug", skip_all)]
    async fn revert(&mut self) -> Result<(), ActionError> {
        let mut errors = vec![];
        if let Err(err) = self.create_or_merge_nix_config.try_revert().await {
            errors.push(err);
        }
        if let Err(err) = self.create_directory.try_revert().await {
            errors.push(err);
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
