use anyhow::Result;
use crate::config::Config;

pub async fn run(_cfg: Config) -> Result<()> {
    anyhow::bail!("TUI not built yet")
}
