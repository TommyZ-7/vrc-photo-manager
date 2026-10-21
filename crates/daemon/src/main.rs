use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "vrc-photo-daemon",
    version,
    about = "VRC Photo Manager background daemon"
)]
struct Args {
    /// Print the resolved data directory and exit.
    #[arg(long)]
    print_data_dir: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,vrc_photo_daemon=debug".into()),
        )
        .init();

    let args = Args::parse();
    if args.print_data_dir {
        println!(
            "{}",
            vrc_photo_core::data_dir().unwrap_or_default().display()
        );
        return Ok(());
    }

    tracing::info!("{} daemon starting", vrc_photo_core::APP_NAME);
    Ok(())
}
