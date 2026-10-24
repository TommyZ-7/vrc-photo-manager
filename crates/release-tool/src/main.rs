use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use vrc_photo_release_tool::{
    build_manifest, collect_files, load_secret_key, sign_file, verify_file,
};

#[derive(Debug, Parser)]
#[command(
    name = "vrc-photo-update-manifest",
    about = "Sign release artifacts and build the update manifest"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Build latest.json from dist artifacts and their .minisig files.
    Generate {
        #[arg(long)]
        dist: PathBuf,
        #[arg(long)]
        tag: String,
        #[arg(long)]
        repo: String,
        #[arg(long)]
        notes_url: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Sign artifacts in a dist directory and/or explicit files.
    Sign {
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        dist: Option<PathBuf>,
        #[arg(long = "file")]
        files: Vec<PathBuf>,
    },
    /// Verify artifacts against the production public key(s).
    Verify {
        #[arg(long = "public-key")]
        public_keys: Vec<String>,
        #[arg(long)]
        dist: Option<PathBuf>,
        #[arg(long = "file")]
        files: Vec<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Generate {
            dist,
            tag,
            repo,
            notes_url,
            output,
        } => {
            let manifest = build_manifest(&dist, &tag, &repo, &notes_url)?;
            if let Some(parent) = output.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&output, serde_json::to_vec_pretty(&manifest)?)
                .with_context(|| format!("cannot write {}", output.display()))?;
            println!(
                "wrote {} for {} ({} platforms)",
                output.display(),
                manifest.version,
                manifest.platforms.len()
            );
        }
        Command::Sign { key, dist, files } => {
            let files = collect_files(dist.as_deref(), files)?;
            if files.is_empty() {
                bail!("no files to sign");
            }
            let key = load_secret_key(&key)?;
            for file in files {
                let signature = sign_file(&key, &file)?;
                println!("signed {}", signature.display());
            }
        }
        Command::Verify {
            public_keys,
            dist,
            files,
        } => {
            if public_keys.is_empty() {
                bail!("at least one --public-key is required");
            }
            let files = collect_files(dist.as_deref(), files)?;
            if files.is_empty() {
                bail!("no files to verify");
            }
            for file in files {
                verify_file(&file, &public_keys)
                    .with_context(|| format!("verification failed for {}", file.display()))?;
                println!("verified {}", file.display());
            }
        }
    }
    Ok(())
}
