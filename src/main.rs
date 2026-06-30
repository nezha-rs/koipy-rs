use anyhow::Result;
use clap::{Parser, Subcommand};
use koipy_rs::app::KoipyApp;
use koipy_rs::config::KoipyConfig;
use koipy_rs::progress::ProgressReport;
use koipy_rs::task::{TaskKind, TaskRequest};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "koipy-rs", version, about = "Rust rewrite of koipy 1.0")]
struct Cli {
    #[arg(short, long, default_value = "config.yaml")]
    config: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the Rust rewrite implementation progress.
    Progress,
    /// Validate configuration and show loaded runtime summary.
    Check,
    /// Fetch and normalize a subscription without starting a Telegram bot.
    Test {
        url: String,
        #[arg(long, default_value = "")]
        include: String,
        #[arg(long, default_value = "")]
        exclude: String,
        #[arg(long, value_enum, default_value_t = TaskKind::Test)]
        kind: TaskKind,
    },
    /// Start the bot service. The Telegram transport is intentionally isolated behind handlers.
    Serve,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Progress => {
            println!("{}", ProgressReport::current().render_markdown());
            Ok(())
        }
        Command::Check => {
            let cfg = KoipyConfig::from_path(&cli.config)?;
            println!("{}", cfg.summary());
            Ok(())
        }
        Command::Test {
            url,
            include,
            exclude,
            kind,
        } => {
            let cfg = KoipyConfig::from_path(&cli.config)?;
            let app = KoipyApp::new(cfg);
            let request = TaskRequest::new_url(kind, url)
                .with_include(include)
                .with_exclude(exclude);
            let outcome = app.prepare_task(request).await?;
            println!("{}", outcome.summary());
            Ok(())
        }
        Command::Serve => {
            let cfg = KoipyConfig::from_path(&cli.config)?;
            let app = KoipyApp::new(cfg);
            app.serve().await
        }
    }
}
