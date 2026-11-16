use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Result, bail};
use clap::Args;
use dialoguer::Select;

#[derive(Debug, Args)]
pub struct LiveContainerArgs {
    /// Connected iPhone/iPad UDID (prompts when omitted)
    #[arg(short = 'u', long)]
    pub udid: Option<String>,
    /// IPA to deploy as a new guest; does not replace existing apps or saves
    #[arg(
        long,
        value_name = "PATH",
        required_unless_present = "list",
        conflicts_with = "list"
    )]
    pub ipa: Option<PathBuf>,
    /// Actual installed LiveContainer bundle identifier, including signing suffix
    #[arg(long, value_name = "ID", conflicts_with = "list")]
    pub bundle_id: Option<String>,
    /// List LiveContainer installations on the selected device
    #[arg(long)]
    pub list: bool,
}

pub async fn execute(args: LiveContainerArgs) -> Result<()> {
    let device = super::device::select_device(args.udid).await?;
    let targets = device.livecontainer_targets().await?;
    if targets.is_empty() {
        bail!(
            "No LiveContainer installation found. Install and open LiveContainer on this device first."
        );
    }
    if args.list {
        for target in targets {
            println!("{target}");
        }
        return Ok(());
    }
    let target = if let Some(id) = args.bundle_id {
        targets
            .iter()
            .find(|t| t.bundle_id == id)
            .ok_or_else(|| anyhow::anyhow!("LiveContainer bundle ID not found: {id}"))?
    } else if targets.len() == 1 {
        &targets[0]
    } else {
        &targets[Select::new()
            .items(&targets)
            .with_prompt("Select LiveContainer")
            .interact()?]
    };
    let ipa = args
        .ipa
        .ok_or_else(|| anyhow::anyhow!("Specify --ipa PATH"))?;
    println!("Deploying to {target}. Keep LiveContainer closed until the transfer finishes.");
    let cancel = Arc::new(AtomicBool::new(false));
    let signal_cancel = cancel.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.store(true, Ordering::Relaxed);
            eprintln!("Cancelling transfer and cleaning up temporary files...");
        }
    });
    let mut last_percent = None;
    let result = device
        .install_to_livecontainer(&ipa, &target.bundle_id, Some(cancel), |p| {
            let percent = if p.total_bytes == 0 {
                0
            } else {
                p.transferred_bytes.saturating_mul(100) / p.total_bytes
            };
            if last_percent != Some(percent) {
                eprintln!(
                    "{percent}% — {} / {} bytes",
                    p.transferred_bytes, p.total_bytes
                );
                last_percent = Some(percent);
            }
        })
        .await;
    signal.abort();
    let result = result?;
    println!(
        "Deployed {} to Documents/Applications/{}",
        result.app_name, result.relative_bundle_path
    );
    println!(
        "Fully close and reopen LiveContainer, then tap the new app. LiveContainer patches and signs it on first launch using its configured certificate or JIT setup."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::commands::Cli;
    use clap::Parser;

    #[test]
    fn list_and_deploy_are_distinct_actions() {
        assert!(Cli::try_parse_from(["plumesign", "livecontainer", "--list"]).is_ok());
        assert!(Cli::try_parse_from(["plumesign", "livecontainer", "--ipa", "game.ipa"]).is_ok());
        assert!(Cli::try_parse_from(["plumesign", "livecontainer"]).is_err());
        assert!(
            Cli::try_parse_from(["plumesign", "livecontainer", "--list", "--ipa", "game.ipa"])
                .is_err()
        );
    }
}
