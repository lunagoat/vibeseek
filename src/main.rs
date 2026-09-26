mod agent;
mod api;
mod cli;
mod config;
mod csvjob;
mod daemon;
mod download;
mod fmt;
mod history;
mod port;
mod quality;
mod search;
mod slskdcfg;
mod tui;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "vibeseek", version, about = "Soulseek from the terminal, powered by slskd", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(Args, Clone, Default)]
pub struct FilterArgs {
    /// Filter preset (lossless, lossy-ok, any, or your own from config.toml)
    #[arg(short, long)]
    pub preset: Option<String>,
    /// Allowed formats, comma separated (overrides the preset's), e.g. flac,mp3
    #[arg(short, long, value_delimiter = ',')]
    pub format: Option<Vec<String>>,
    /// Minimum bitrate for lossy files (kbps)
    #[arg(long)]
    pub min_bitrate: Option<u32>,
    /// Minimum bit depth for lossless files
    #[arg(long)]
    pub min_bitdepth: Option<u32>,
    /// Minimum sample rate for lossless files (Hz)
    #[arg(long)]
    pub min_samplerate: Option<u32>,
    /// Reject files that don't report the filtered attributes
    #[arg(long)]
    pub strict: bool,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Open the full-screen interface (default)
    Tui,
    /// Search the Soulseek network
    #[command(alias = "s")]
    Search {
        /// What to search for
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        #[command(flatten)]
        filter: FilterArgs,
        /// Group results by folder (albums)
        #[arg(short, long)]
        albums: bool,
        /// How many results to show
        #[arg(short = 'n', long, default_value_t = 40)]
        limit: usize,
        /// Seconds to wait for results
        #[arg(short, long)]
        timeout: Option<u64>,
        /// Print JSON instead of a table
        #[arg(long)]
        json: bool,
        /// Immediately download these results, e.g. -d 1 or -d 1,3-5
        #[arg(short, long)]
        download: Option<String>,
        /// Folder to download into (with -d)
        #[arg(short, long)]
        output: Option<String>,
    },
    /// Download results from the last search by number, e.g. `vibeseek get 1 3-5`
    #[command(alias = "g")]
    Get {
        #[arg(required = true, num_args = 1..)]
        selection: Vec<String>,
        /// Folder to download into (default: slskd's downloads dir)
        #[arg(short, long)]
        output: Option<String>,
        /// Wait and show progress until the downloads finish
        #[arg(short, long)]
        wait: bool,
    },
    /// Show / manage your downloads
    #[command(alias = "dl")]
    Downloads {
        #[command(subcommand)]
        action: Option<TransferAction>,
        /// Live-updating view
        #[arg(short, long)]
        watch: bool,
        /// Include finished transfers
        #[arg(short, long)]
        all: bool,
    },
    /// Show / manage people downloading from you
    #[command(alias = "ul")]
    Uploads {
        #[command(subcommand)]
        action: Option<TransferAction>,
        /// Live-updating view
        #[arg(short, long)]
        watch: bool,
        /// Include finished transfers
        #[arg(short, long)]
        all: bool,
    },
    /// Upload history and stats
    #[command(alias = "stats")]
    History {
        /// Only this user
        #[arg(short, long)]
        user: Option<String>,
        /// Time window, e.g. 24h, 7d, 30d (default: all time)
        #[arg(short, long)]
        since: Option<String>,
        /// Rows per section
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
    },
    /// Ban a user from downloading from you (also cancels their uploads)
    Ban { user: String },
    /// Lift a ban
    Unban { user: String },
    /// List banned users
    Bans,
    /// Batch download from a CSV (Spotify exports etc.), like sockseek
    Csv(CsvArgs),
    /// Show or set the Soulseek listen port (auto-detects ProtonVPN forwarding)
    Port {
        /// Port to use; omit to auto-detect from the VPN
        port: Option<u16>,
        /// Only print the detected / current port
        #[arg(long)]
        show: bool,
    },
    /// Control the slskd background service
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Background helper: port sync, upload history, moving finished downloads
    Agent {
        #[command(subcommand)]
        action: Option<AgentAction>,
    },
    /// Connection and share status
    Status,
    /// Show config file location (or open it in $EDITOR with --edit)
    Config {
        #[arg(long)]
        edit: bool,
    },
}

#[derive(Subcommand, Clone)]
pub enum TransferAction {
    /// Cancel transfers by number (from the last listing), `all`, or `user:NAME`
    Cancel {
        #[arg(required = true, num_args = 1..)]
        which: Vec<String>,
    },
    /// Remove finished transfers from the list
    Clear,
    /// Re-queue failed downloads (downloads only)
    Retry {
        /// Numbers from the last listing, or omit for all failed
        which: Vec<String>,
    },
}

#[derive(Subcommand, Clone, Copy)]
pub enum DaemonAction {
    Start,
    Stop,
    Restart,
    Status,
    /// Follow slskd's log
    Logs,
}

#[derive(Subcommand, Clone, Copy)]
pub enum AgentAction {
    /// Install + start the agent as a systemd user service
    Install,
    /// Stop and remove the agent service
    Uninstall,
    /// Run in the foreground (what the service runs)
    Run,
}

#[derive(Args, Clone)]
pub struct CsvArgs {
    /// CSV file
    pub file: String,
    /// Output folder (default: <downloads>/<csv name>)
    #[arg(short, long)]
    pub output: Option<String>,
    #[command(flatten)]
    pub filter: FilterArgs,
    /// Preset to fall back to when nothing passes the main filter (e.g. lossy-ok)
    #[arg(long)]
    pub fallback: Option<String>,
    /// Only search and show what would be downloaded
    #[arg(long)]
    pub dry_run: bool,
    /// Queue downloads and exit without waiting for them to finish
    #[arg(long)]
    pub no_wait: bool,
    /// Retry rows that previously weren't found or failed
    #[arg(long)]
    pub retry: bool,
    /// Forget previous progress for this CSV and start over
    #[arg(long)]
    pub restart: bool,
    /// Only process the first N pending rows
    #[arg(short = 'n', long)]
    pub number: Option<usize>,
    /// Skip the first N rows
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Show progress for this CSV and exit
    #[arg(long)]
    pub status: bool,
    #[arg(long)]
    pub title_col: Option<String>,
    #[arg(long)]
    pub artist_col: Option<String>,
    #[arg(long)]
    pub album_col: Option<String>,
    #[arg(long)]
    pub length_col: Option<String>,
    /// Treat every row as an album download
    #[arg(long)]
    pub albums: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = cli::run(cli).await {
        eprintln!("\x1b[31merror:\x1b[0m {e:#}");
        std::process::exit(1);
    }
}
