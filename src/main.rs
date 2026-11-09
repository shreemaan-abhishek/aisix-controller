use std::path::PathBuf;
use std::time::Duration;

use aisix_controller::crd::Kind;
use aisix_controller::reconcile::{self, Settings};
use aisix_controller::schema;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    version,
    about = "Reconciles AISIX custom resources into an AISIX gateway"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the controller against the current cluster.
    Run {
        /// Namespace to watch. Defaults to the controller's own.
        #[arg(long, env = "POD_NAMESPACE")]
        namespace: String,
        /// Secret the gateway reads its resources file from.
        #[arg(long, default_value = "aisix-resources")]
        secret_name: String,
        /// Key of the resources file within that Secret.
        #[arg(long, default_value = "resources.yaml")]
        secret_key: String,
        /// Gateway Deployment to roll out on change.
        #[arg(long, default_value = "aisix")]
        deployment_name: String,
        /// Full resync interval, in seconds.
        #[arg(long, default_value_t = 60)]
        resync_seconds: u64,
    },
    /// Write the CustomResourceDefinitions.
    Crds {
        #[arg(long, default_value = "deploy/crds")]
        out: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kube=warn".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Run {
            namespace,
            secret_name,
            secret_key,
            deployment_name,
            resync_seconds,
        } => {
            let client = kube::Client::try_default().await?;
            let settings = Settings {
                namespace,
                secret_name,
                secret_key,
                deployment_name,
                resync: Duration::from_secs(resync_seconds),
            };
            tokio::select! {
                r = reconcile::run(client, settings) => r,
                _ = tokio::signal::ctrl_c() => Ok(()),
            }
        }
        Command::Crds { out } => {
            std::fs::create_dir_all(&out)?;
            for kind in Kind::ALL {
                let path = out.join(schema::crd_file_name(kind));
                std::fs::write(&path, schema::crd_yaml(kind))?;
                println!("{}", path.display());
            }
            Ok(())
        }
    }
}
