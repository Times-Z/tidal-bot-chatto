use anyhow::{Context, Result};
use bot::{Bot, BotConfig};
use chatto::Client as ChattoClient;
use config::AppConfig;
use std::env;
use tidal::Client as TidalClient;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        error!(error = %err, "fatal error");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    init_tracing();

    let config_path = env::args()
        .nth(1)
        .unwrap_or_else(|| "config.json".to_owned());

    let cfg = AppConfig::load_from_path(&config_path)
        .with_context(|| format!("failed to load config from {config_path}"))?;

    // Chatto 0.5: bots authenticate with a named API key created in
    // Server Admin -> Bots. Old human session tokens are no longer accepted.
    if !cfg.chatto_token.starts_with("cht_BK_") {
        warn!("chatto_token does not look like a bot API key (expected cht_BK_...)");
    }

    let chatto_client = ChattoClient::new(&cfg.chatto_url, &cfg.chatto_token);
    let tidal_client = TidalClient::new(&cfg.tidal_token_path, &cfg.tidal_quality)
        .await
        .context("failed to initialize tidal client")?;
    let bot_cfg = BotConfig {
        rooms: cfg.rooms.clone(),
        poll_interval: cfg.poll_interval,
        bot_name: if cfg.bot_name.trim().is_empty() {
            "tidal_bot".to_owned()
        } else {
            cfg.bot_name.clone()
        },
        volume: cfg.volume,
        sample_rate: cfg.sample_rate,
        default_lyrics: cfg.default_lyrics,
        thread_replies: cfg.thread_replies,
    };
    let bot = Bot::new(
        bot_cfg,
        cfg.livekit_url.clone(),
        chatto_client.clone(),
        tidal_client.clone(),
    );

    info!(version = bot::version(), "chatto-bot-tidal starting");

    info!(
        rooms = ?cfg.rooms,
        poll_interval = ?cfg.poll_interval,
        "configuration loaded"
    );
    info!(
        base_url = chatto_client.base_url(),
        "chatto client initialized"
    );
    info!(
        quality = tidal_client.selected_quality(),
        "tidal client initialized"
    );
    info!("chatto-bot-tidal rust runtime bootstrap complete");

    tokio::select! {
        run_result = bot.run() => {
            run_result.context("bot runtime failed")?;
        }
        signal_result = tokio::signal::ctrl_c() => {
            signal_result.context("failed to listen for ctrl-c signal")?;
            info!("shutdown signal received");
            bot.shutdown();
        }
    }

    Ok(())
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .compact()
        .init();
}
