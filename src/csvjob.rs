use anyhow::Result;
use crate::config::Config;
use crate::CsvArgs;

pub async fn run(_cfg: &Config, _args: CsvArgs) -> Result<()> {
    anyhow::bail!("csv not built yet")
}
