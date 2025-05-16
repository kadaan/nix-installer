pub mod action;
#[cfg(feature = "cli")]
pub mod cli;
#[cfg(feature = "diagnostics")]
pub mod diagnostics;
mod error;
mod os;
mod plan;
pub mod planner;
pub mod self_test;
pub mod settings;

use std::{ffi::OsStr, path::Path, process::Output};

pub use error::NixInstallerError;
pub use plan::InstallPlan;
use planner::BuiltinPlanner;

use reqwest::Certificate;
use tokio::process::Command;

use crate::action::{Action, ActionErrorKind};

#[tracing::instrument(level = "debug", skip_all, fields(command = %format!("{:?}", command.as_std())))]
async fn execute_command(command: &mut Command) -> Result<Output, ActionErrorKind> {
    tracing::trace!("Executing");
    let output = command
        .output()
        .await
        .map_err(|e| ActionErrorKind::command(command, e))?;
    match output.status.success() {
        true => Ok(output),
        false => Err(ActionErrorKind::command_output(command, output)),
    }
}

#[tracing::instrument(level = "debug", skip_all, fields(
    k = %k.as_ref().to_string_lossy(),
    v = %v.as_ref().to_string_lossy(),
))]
fn set_env(k: impl AsRef<OsStr>, v: impl AsRef<OsStr>) {
    tracing::trace!("Setting env");
    std::env::set_var(k.as_ref(), v.as_ref());
}

async fn parse_ssl_cert(ssl_cert_file: &Path) -> Result<Certificate, CertificateError> {
    let cert_buf = tokio::fs::read(ssl_cert_file)
        .await
        .map_err(|e| CertificateError::Read(ssl_cert_file.to_path_buf(), e))?;
    // We actually try them since things could be `.crt` and `pem` format or `der` format
    let cert = if let Ok(cert) = Certificate::from_pem(cert_buf.as_slice()) {
        cert
    } else if let Ok(cert) = Certificate::from_der(cert_buf.as_slice()) {
        cert
    } else {
        return Err(CertificateError::UnknownCertFormat);
    };
    Ok(cert)
}

#[derive(Debug, thiserror::Error)]
pub enum CertificateError {
    #[error(transparent)]
    Reqwest(reqwest::Error),
    #[error("Read path `{0}`")]
    Read(std::path::PathBuf, #[source] std::io::Error),
    #[error("Unknown certificate format, `der` and `pem` supported")]
    UnknownCertFormat,
}
