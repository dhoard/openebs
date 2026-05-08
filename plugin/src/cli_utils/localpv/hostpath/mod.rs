pub(crate) mod volume;

use clap::Parser;
use plugin::{resources::utils::OutputFormat, ExecuteOperation};
use snafu::Snafu;

/// LocalPV Hostpath operations.
#[derive(Parser, Debug)]
pub enum Operations {
    /// Gets localpv-hostpath resources.
    #[clap(subcommand)]
    Get(HosthpathGet),
}

#[derive(Parser, Debug)]
pub struct Hostpath {
    #[command(subcommand)]
    pub ops: Operations,
    #[command(flatten)]
    pub cli_args: CliArgs,
}

#[derive(Parser, Debug)]
#[group(skip)]
pub struct CliArgs {
    #[clap(skip)]
    pub ctx: crate::cli_utils::K8sCtxArgs,

    /// The Output, viz yaml, json.
    #[clap(global = true, default_value = OutputFormat::None.as_ref(), short, long)]
    pub output: OutputFormat,
}

#[async_trait::async_trait(?Send)]
impl ExecuteOperation for Operations {
    type Args = CliArgs;
    type Error = Error;

    async fn execute(&self, cli_args: &CliArgs) -> Result<(), Error> {
        match self {
            Operations::Get(hostpathget) => {
                hostpathget.execute(cli_args).await?;
            }
        }
        Ok(())
    }
}

/// Get commands for localpv-hostpath.
#[derive(clap::Subcommand, Debug)]
pub enum HosthpathGet {
    /// Gets a specific localpv-hostpath volume.
    Volume(GetVolumeArg),
    /// Lists all localpv-hostpath volumes.
    Volumes(GetVolumesArg),
}

/// Argument used when getting a hostpath volume.
#[derive(Debug, Clone, clap::Args)]
pub struct GetVolumeArg {
    /// Volume id of the volume.
    volume: String,
}

/// Argument used when listing localpv-hostpath volumes.
#[derive(Debug, Clone, clap::Args)]
pub struct GetVolumesArg {
    /// Lists localpv-hostpath volumes from a specific node if set.
    node_id: Option<String>,
}

#[async_trait::async_trait(?Send)]
impl ExecuteOperation for HosthpathGet {
    type Args = CliArgs;
    type Error = Error;

    async fn execute(&self, cli_args: &CliArgs) -> Result<(), Error> {
        let client = cli_args
            .ctx
            .client()
            .await
            .map_err(|source| Error::Generic { source })?;
        match self {
            HosthpathGet::Volume(volume_arg) => {
                volume::volume(cli_args, volume_arg, client).await?;
            }
            HosthpathGet::Volumes(volumes_arg) => {
                volume::volumes(cli_args, volumes_arg, client).await?;
            }
        }
        Ok(())
    }
}

/// Error for localpv-hostpath stem.
#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("{source}"))]
    Generic { source: anyhow::Error },
    #[snafu(display("{source}"))]
    Kube { source: kube::Error },
}
