mod config;
mod context;
mod daemon;
mod desktop;
mod error;
mod hermes;
mod ollama;
mod sprite;
mod tools;
mod vision;

use clap::{Parser, Subcommand};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::error::GremlinError;
use crate::ollama::Ollama;
use crate::sprite::{register_sprite_tools, SpriteSystem};
use crate::tools::ToolRegistry;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "gremlin", about = "Local-first AI orchestration daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// One-shot: ask Gremlin a question (tries daemon first, falls back to direct)
    Ask {
        /// The question or request
        message: Vec<String>,
    },

    /// Start the Gremlin daemon (listens on Unix socket for queries)
    Daemon,

    /// Initialize Gremlin — create config directory and default config
    Init {
        /// Force overwrite existing config
        #[arg(long)]
        force: bool,
    },

    /// Check that Ollama is reachable and the configured model is available
    Check,

    /// Install Gremlin as a systemd user service (Linux only)
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install the systemd user service (enables auto-start on login)
    Install,
    /// Remove the systemd user service
    Uninstall,
    /// Show service status
    Status,
}

#[tokio::main]
async fn main() -> Result<(), GremlinError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("gremlin=info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Init { force } => {
            init_config(force)?;
        }
        Command::Check => {
            check().await?;
        }
        Command::Service { action } => {
            handle_service(action)?;
        }
        Command::Daemon => {
            run_daemon().await?;
        }
        Command::Ask { message } => {
            let msg = message.join(" ");
            if msg.is_empty() {
                eprintln!("gremlin: no question provided");
                std::process::exit(1);
            }
            ask(&msg).await?;
        }
    }

    Ok(())
}

fn init_config(force: bool) -> Result<(), GremlinError> {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.config"))
        .join("gremlin");

    let config_path = config_dir.join("config.toml");

    if config_path.exists() && !force {
        info!("Config already exists at {}", config_path.display());
        info!("Use `gremlin init --force` to overwrite");
        return Ok(());
    }

    std::fs::create_dir_all(&config_dir)?;
    let config = Config::default();
    config.save()?;
    info!("Created config at {}", config_path.display());
    println!("Gremlin initialized. Edit ~/.config/gremlin/config.toml to configure.");
    Ok(())
}

async fn check() -> Result<(), GremlinError> {
    let config = Config::load()?;
    let ollama = Ollama::new(&config.ollama);

    println!("Checking Ollama at {}...", config.ollama.url);
    match ollama.health_check().await {
        Ok(()) => println!("  ✅ Ollama is reachable"),
        Err(e) => {
            error!("  ❌ Ollama is not reachable: {e}");
            return Err(e.into());
        }
    }

    match ollama.model_exists(&config.model.name).await {
        Ok(true) => println!("  ✅ Model '{}' is available", config.model.name),
        Ok(false) => {
            println!("  ❌ Model '{}' not found. Pull it with: ollama pull {}", config.model.name, config.model.name);
            println!(
                "     Available models: {:?}",
                ollama.list_models().await?.iter().map(|m| &m.name).collect::<Vec<_>>()
            );
        }
        Err(e) => eprintln!("  ❌ Failed to check model: {e}"),
    }

    // Check vision model availability if configured
    let vision_model = config.vision
        .as_ref()
        .map(|v| v.model.clone())
        .unwrap_or_else(|| "llama3.2-vision:11b".to_string());

    let vision_available = crate::vision::vision_model_available(
        &config.ollama.url,
        &vision_model,
    ).await;

    if vision_available {
        println!("  ✅ Vision model '{}' is available (screenshots enabled)", vision_model);
    } else {
        println!("  ⚠️  Vision model '{}' not found — screenshots won't work", vision_model);
        println!("     Pull it with: ollama pull {}", vision_model);
    }

    Ok(())
}

/// Run the daemon — blocks until killed.
async fn run_daemon() -> Result<(), GremlinError> {
    let config = Config::load()?;
    let ollama = Ollama::new(&config.ollama);
    let mut tools = ToolRegistry::new();

    // Initialize sprite system — gracefully skip if assets are missing
    let default_sprite = crate::config::SpriteConfig::default();
    let sprite_config = config.sprite.as_ref().unwrap_or(&default_sprite);
    let assets_dir = std::path::PathBuf::from(&sprite_config.assets_dir);
    let initial_state = &sprite_config.initial_state;
    match SpriteSystem::new(assets_dir.to_str().unwrap_or("assets/sprites"), initial_state) {
        Ok(sprite_system) => {
            let sprite_system = Arc::new(sprite_system);
            sprite_system.spawn_ticker();
            register_sprite_tools(&mut tools, sprite_system);
            info!("Sprite system loaded ({})", assets_dir.display());
        }
        Err(e) => {
            info!("Sprite system skipped — assets not found at {}: {e}", assets_dir.display());
            info!("Run `gremlin generate-sprites` to create sprite assets, or place them manually.");
        }
    }

    // Verify Ollama is reachable before starting
    ollama.health_check().await.map_err(|e| {
        error!("Ollama not reachable. Start Ollama first: ollama serve");
        e
    })?;

    info!("Ollama connected. Model: {}", config.model.name);

    daemon::run_daemon(config, ollama, tools).await
}

/// Ask Gremlin — tries the daemon first, falls back to one-shot mode.
async fn ask(message: &str) -> Result<(), GremlinError> {
    // Try the daemon first
    match daemon::send_to_daemon(message).await {
        Ok(response) => {
            println!("{response}");
            return Ok(());
        }
        Err(_) => {
            // Daemon not running — fall back to direct query
            info!("Daemon not running, using one-shot mode");
        }
    }

    let config = Config::load()?;
    let ollama = Ollama::new(&config.ollama);
    let mut tools = ToolRegistry::new();

    // Initialize sprite system for one-shot mode too (graceful skip if assets missing)
    let default_sprite = crate::config::SpriteConfig::default();
    let sprite_config = config.sprite.as_ref().unwrap_or(&default_sprite);
    let assets_dir = std::path::PathBuf::from(&sprite_config.assets_dir);
    let initial_state = &sprite_config.initial_state;
    if let Ok(sprite_system) = SpriteSystem::new(
        assets_dir.to_str().unwrap_or("assets/sprites"),
        initial_state,
    ) {
        register_sprite_tools(&mut tools, Arc::new(sprite_system));
    }

    info!("Asking: {}", message);

    match daemon::query(&config, &ollama, &tools, message, None).await {
        Ok(response) => {
            println!("{response}");
        }
        Err(e) => {
            error!("Query failed: {e}");
            return Err(e);
        }
    }

    Ok(())
}

// ── Systemd service management (Linux only) ──

#[cfg(unix)]
fn find_gremlin_binary() -> Result<std::path::PathBuf, GremlinError> {
    // Check common locations
    let candidates = [
        dirs::home_dir()
            .unwrap_or_default()
            .join(".cargo/bin/gremlin"),
        std::path::PathBuf::from("/usr/local/bin/gremlin"),
        std::path::PathBuf::from("/usr/bin/gremlin"),
    ];

    for path in &candidates {
        if path.exists() {
            return Ok(path.clone());
        }
    }

    // Try `which gremlin` as fallback
    if let Ok(out) = std::process::Command::new("which")
        .arg("gremlin")
        .output()
    {
        if out.status.success() {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(std::path::PathBuf::from(path));
            }
        }
    }

    Err(GremlinError::Tool(
        "gremlin binary not found. Build with `cargo build --release` and ensure \
         target/release/gremlin is in PATH or ~/.cargo/bin/.".into(),
    ))
}

#[cfg(unix)]
fn handle_service(action: ServiceAction) -> Result<(), GremlinError> {
    use std::process::Command;

    let service_dir = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.config"))
        .join("systemd/user");

    let service_path = service_dir.join("gremlin.service");

    match action {
        ServiceAction::Install => {
            // Find the gremlin binary — check cargo home first, then PATH
            let gremlin_bin = find_gremlin_binary()?;

            std::fs::create_dir_all(&service_dir)?;

            let service_content = include_str!("../config/gremlin.service").to_string();
            let home = dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("/home/user"));
            let service_content = service_content.replace(
                "%h/.cargo/bin/gremlin",
                &gremlin_bin.display().to_string(),
            );
            let service_content = service_content.replace("%h", &home.display().to_string());

            std::fs::write(&service_path, &service_content)?;

            Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status()
                .map_err(|e| GremlinError::Tool(format!("systemctl not available: {e}")))?;

            Command::new("systemctl")
                .args(["--user", "enable", "--now", "gremlin.service"])
                .status()
                .map_err(|e| GremlinError::Tool(format!("Failed to enable service: {e}")))?;

            println!("✅ Gremlin service installed and started");
            println!("   Check status: systemctl --user status gremlin");
            println!("   View logs:    journalctl --user -u gremlin -f");
            println!("   Socket:       $XDG_RUNTIME_DIR/gremlin.sock");
        }
        ServiceAction::Uninstall => {
            Command::new("systemctl")
                .args(["--user", "disable", "--now", "gremlin.service"])
                .status()
                .ok();

            if service_path.exists() {
                std::fs::remove_file(&service_path)?;
                Command::new("systemctl")
                    .args(["--user", "daemon-reload"])
                    .status()
                    .ok();
            }

            println!("✅ Gremlin service removed");
        }
        ServiceAction::Status => {
            let status = Command::new("systemctl")
                .args(["--user", "status", "gremlin.service"])
                .output();

            match status {
                Ok(out) => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    println!("{text}");
                    if !out.status.success() {
                        let err = String::from_utf8_lossy(&out.stderr);
                        eprintln!("{err}");
                    }
                }
                Err(e) => {
                    println!("systemctl not available: {e}");
                    println!("Check daemon manually: gremlin.sock should be at $XDG_RUNTIME_DIR/gremlin.sock");
                }
            }
        }
    }

    Ok(())
}

#[cfg(not(unix))]
fn handle_service(_action: ServiceAction) -> Result<(), GremlinError> {
    Err(GremlinError::Tool(
        "Service management is only available on Linux with systemd.".into(),
    ))
}