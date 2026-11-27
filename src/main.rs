use anyhow::Context;
use clap::{Parser, Subcommand};
use roost::server::{AppState, Config, channels, connect, owner, router, scheduler};

#[derive(Parser)]
#[command(
    version,
    about = "Self-hosted two-way agent ↔ iPhone messenger: the server, and the CLI agents use to reach their human"
)]
struct Cli {
    /// SQLite database file (server commands).
    #[arg(long, env = "ROOST_DB", default_value = "roost.db")]
    db: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once at startup
enum Command {
    #[command(flatten)]
    Agent(roost::client::Command),
    /// Run the HTTP server.
    Serve {
        #[arg(long, env = "ROOST_BIND", default_value = "127.0.0.1:8080")]
        bind: String,
        /// Public base URL agents and the PWA use (returned by /claim).
        #[arg(long, env = "ROOST_BASE_URL")]
        base_url: String,
        /// Bearer token for the temporary human API (/api/human), until passkeys exist.
        #[arg(long, env = "ROOST_ADMIN_TOKEN")]
        admin_token: Option<String>,
        /// HMAC secret for reply_to webhook signatures.
        #[arg(long, env = "ROOST_CALLBACK_SECRET")]
        callback_secret: Option<String>,
        #[arg(long, env = "ROOST_TIMEZONE", default_value = "Europe/Stockholm")]
        timezone: String,
        /// File holding the VAPID private key (PEM). Without it a key is generated and kept in the database.
        #[arg(long, env = "ROOST_VAPID_KEY_FILE")]
        vapid_key_file: Option<String>,
        /// VAPID `sub` claim, a mailto: or https: URL. Defaults to the base URL.
        #[arg(long, env = "ROOST_VAPID_SUBJECT")]
        vapid_subject: Option<String>,
        /// Name a second server, like `dev`: its app is "roost dev" with an orange icon, so it can't be mistaken for the real one.
        #[arg(long, env = "ROOST_INSTANCE", value_parser = instance_name)]
        instance: Option<String>,
    },
    /// Manage the owner's passkeys.
    Passkey {
        #[command(subcommand)]
        command: PasskeyCommand,
    },
    /// Manage channels.
    Channel {
        #[command(subcommand)]
        command: ChannelCommand,
    },
}

#[derive(Subcommand)]
enum ChannelCommand {
    /// Create a channel and print its first claim code.
    Create { name: String },
    /// Mint a fresh claim code for an existing channel.
    ClaimCode { id: i64 },
    /// Delete a channel with its notifications and messages; its agent key stops working.
    Delete { id: i64 },
}

#[derive(Subcommand)]
enum PasskeyCommand {
    /// Print a one-time link (15 min) that enrolls a new passkey.
    Enroll {
        /// The public base URL, to print a clickable link.
        #[arg(long, env = "ROOST_BASE_URL")]
        base_url: Option<String>,
    },
    /// List enrolled passkeys.
    List,
    /// Remove a passkey and sign out its sessions.
    Remove { id: i64 },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Agent commands exit 1 on a usage error, not clap's 2 (2 means "the call failed").
    let cli = Cli::try_parse().unwrap_or_else(|e| {
        let code = if e.use_stderr() { 1 } else { 0 };
        let _ = e.print();
        std::process::exit(code);
    });
    let command = match cli.command {
        Command::Agent(cmd) => std::process::exit(roost::client::run(cmd).await),
        other => other,
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,sqlx=warn".into()),
        )
        .init();
    let db = connect(&cli.db).await?;

    match command {
        Command::Serve {
            bind,
            base_url,
            admin_token,
            callback_secret,
            timezone,
            vapid_key_file,
            vapid_subject,
            instance,
        } => {
            let vapid_private_pem = vapid_key_file
                .map(|f| std::fs::read_to_string(&f).with_context(|| format!("reading {f}")))
                .transpose()?;
            let config = Config {
                base_url: base_url.trim_end_matches('/').to_string(),
                admin_token,
                callback_secret,
                timezone,
                vapid_private_pem,
                vapid_subject,
                instance,
            };
            let listener = tokio::net::TcpListener::bind(&bind)
                .await
                .with_context(|| format!("binding {bind}"))?;
            tracing::info!("roost listening on {bind} as {}", config.base_url);
            let state = AppState::new(db, config).await?;
            scheduler::spawn(state.clone());
            axum::serve(listener, router(state)).await?;
        }
        Command::Channel {
            command: ChannelCommand::Create { name },
        } => {
            let (id, code) = channels::create(&db, &name).await?;
            println!(
                "channel {id} \"{name}\"\nclaim code {} (expires {})",
                code.code, code.expires_at
            );
        }
        Command::Channel {
            command: ChannelCommand::Delete { id },
        } => {
            anyhow::ensure!(channels::delete(&db, id).await?, "no channel {id}");
            println!("deleted channel {id}");
        }
        Command::Passkey {
            command: PasskeyCommand::Enroll { base_url },
        } => {
            let (code, expires_at) = owner::mint_enrollment_code(&db).await?;
            match base_url {
                Some(b) => println!("{}/enroll#{code}", b.trim_end_matches('/')),
                None => println!("enrollment code {code}"),
            }
            println!("(single use, expires {expires_at})");
        }
        Command::Passkey {
            command: PasskeyCommand::List,
        } => {
            for r in sqlx::query!(
                r#"SELECT id as "id!", label, created_at, last_used_at FROM passkeys ORDER BY id"#
            )
            .fetch_all(&db)
            .await?
            {
                println!(
                    "{}\t{}\tcreated {}\tlast used {}",
                    r.id,
                    r.label,
                    r.created_at,
                    r.last_used_at.as_deref().unwrap_or("never")
                );
            }
        }
        Command::Passkey {
            command: PasskeyCommand::Remove { id },
        } => {
            let n = sqlx::query!("DELETE FROM passkeys WHERE id = ?", id)
                .execute(&db)
                .await?
                .rows_affected();
            anyhow::ensure!(n == 1, "no passkey {id}");
            println!("removed passkey {id}");
        }
        Command::Agent(_) => unreachable!("handled above"),
        Command::Channel {
            command: ChannelCommand::ClaimCode { id },
        } => {
            channels::load(&db, id)
                .await?
                .with_context(|| format!("no channel {id}"))?;
            let code = channels::mint_claim_code(&db, id).await?;
            println!("claim code {} (expires {})", code.code, code.expires_at);
        }
    }
    Ok(())
}

fn instance_name(s: &str) -> Result<String, String> {
    let ok =
        !s.is_empty() && s.len() <= 16 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    ok.then(|| s.to_string())
        .ok_or_else(|| "use up to 16 letters, digits or dashes".into())
}
