//! Command line interface. The structure and the service subcommands
//! come from pigeons (https://github.com/n0-computer/pigeons); the host,
//! connect, allow, deny and id commands are new.
//!
//! SPDX-License-Identifier: MIT OR Apache-2.0

use std::{
    io,
    net::{IpAddr, SocketAddr},
    str::FromStr,
};

use anyhow::bail;
use clap::{ArgAction, Args, Parser, Subcommand};
use iroh::{EndpointId, RelayUrl};
use remoteadb::{
    Config, HostOptions, Paths, SavedHost, ServiceParams, TunnelBuilder, adb_listening,
    install_service, load_or_create_key, resolve_binary_path, resolve_target, restart_service,
    run_host, service_log, service_ticket, uninstall_service,
};
use tokio::signal;

const RELAY_URL_HELP: &str = "private relay server to use (repeatable; added to the config's)";

#[derive(Parser, Debug)]
#[command(
    name = "remoteadb",
    about = "adb to a device plugged into another machine, over iroh with private relays"
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Share this machine's adb server with allowlisted clients
    Host(HostArgs),
    /// Open a local port that reaches a remote host's adb server
    Connect(ConnectArgs),
    /// Save a remote host under a name for `connect`
    Add(AddArgs),
    /// List saved hosts
    List,
    /// Forget a saved host
    Remove(RemoveArgs),
    /// Allow a client endpoint ID to use this host
    Allow(AllowArgs),
    /// Stop allowing a client endpoint ID
    Deny(AllowArgs),
    /// Print this machine's endpoint ID (give it to a host admin to be allowed)
    Id(IdArgs),
    /// Install or manage the host as a system service
    Service {
        #[command(subcommand)]
        op: ServiceCmd,
    },
    /// Print the version number
    Version,
    /// Print the paths used for config, key, and ticket
    Paths,
    /// Internal: entry point used by the Windows service control manager
    #[cfg(target_os = "windows")]
    #[command(hide = true)]
    RunService(HostArgs),
}

#[derive(Subcommand, Clone, Debug)]
pub enum ServiceCmd {
    /// Install the host as a service that starts at boot
    Install {
        /// Port the adb server listens on
        #[arg(long)]
        adb_port: Option<u16>,

        #[arg(long, value_name = "URL", help = RELAY_URL_HELP, action = ArgAction::Append)]
        relay_url: Vec<String>,

        /// Linux only: install as a user service (systemctl --user) instead of
        /// a system one, so adb runs as you and shares your adb keys
        #[arg(long, default_value_t = false)]
        user: bool,
    },
    /// Uninstall the service
    Uninstall {
        /// Linux only: uninstall the user service
        #[arg(long, default_value_t = false)]
        user: bool,
    },
    /// Restart the running service
    Restart {
        /// Linux only: restart the user service
        #[arg(long, default_value_t = false)]
        user: bool,
    },
    /// Show whether the service is running and its ticket
    Status,
    /// Show service logs
    Log {
        /// Linux only: logs of the user service
        #[arg(long, default_value_t = false)]
        user: bool,
    },
}

#[derive(Args, Clone, Debug)]
pub struct HostArgs {
    /// Port the local adb server listens on
    #[arg(long)]
    pub adb_port: Option<u16>,

    /// Path to the adb binary used to start the server
    #[arg(long)]
    pub adb_path: Option<String>,

    /// Do not start or supervise an adb server; expect one to be running
    #[arg(long, default_value_t = false)]
    pub no_adb: bool,

    /// Allow this client endpoint ID (repeatable; added to the config's)
    #[arg(long, value_name = "ENDPOINT_ID", action = ArgAction::Append)]
    pub allow: Vec<String>,

    /// Use a throwaway identity instead of the persistent key
    #[arg(short, long, default_value_t = false)]
    pub ephemeral: bool,

    /// Use the machine-wide config and key (default when running elevated)
    #[arg(long, default_value_t = false)]
    pub system: bool,

    #[arg(long, value_name = "URL", help = RELAY_URL_HELP, action = ArgAction::Append)]
    pub relay_url: Vec<String>,
}

impl From<HostArgs> for HostOptions {
    fn from(args: HostArgs) -> Self {
        Self {
            adb_port: args.adb_port,
            adb_path: args.adb_path,
            no_adb: args.no_adb,
            allow: args.allow,
            ephemeral: args.ephemeral,
            system: args.system,
            relay_url: args.relay_url,
        }
    }
}

#[derive(Args, Clone, Debug)]
pub struct ConnectArgs {
    /// Ticket, endpoint ID, or the name of a saved host
    #[arg()]
    pub target: String,

    /// Local port to expose the remote adb server on
    #[arg(short, long)]
    pub port: Option<u16>,

    /// Local address to bind
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: IpAddr,

    #[arg(long, value_name = "URL", help = RELAY_URL_HELP, action = ArgAction::Append)]
    pub relay_url: Vec<String>,
}

#[derive(Args, Clone, Debug)]
pub struct AddArgs {
    /// Name to save the host under
    #[arg(long)]
    pub name: String,

    /// Local port to expose it on when connecting
    #[arg(short, long)]
    pub port: Option<u16>,

    /// Ticket or endpoint ID of the host
    #[arg()]
    pub ticket: String,
}

#[derive(Args, Clone, Debug)]
pub struct RemoveArgs {
    #[arg()]
    pub name: String,
}

#[derive(Args, Clone, Debug)]
pub struct AllowArgs {
    /// Client endpoint ID
    #[arg()]
    pub id: String,

    /// Edit the machine-wide config (what a system service reads)
    #[arg(long, default_value_t = false)]
    pub system: bool,
}

#[derive(Args, Clone, Debug)]
pub struct IdArgs {
    /// Show the machine-wide identity instead of the user's
    #[arg(long, default_value_t = false)]
    pub system: bool,
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        bail!("host name cannot be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
    {
        bail!(
            "host name '{name}' contains invalid characters (use letters, digits, hyphens, dots, or underscores)"
        );
    }
    Ok(())
}

async fn run_connect(args: ConnectArgs) -> anyhow::Result<()> {
    let paths = Paths::user()?;
    let config = Config::load().await?;

    let (target, saved_port) = match config.hosts.get(&args.target) {
        Some(saved) => (saved.ticket.clone(), saved.port),
        None => (args.target.clone(), None),
    };
    let port = args
        .port
        .or(saved_port)
        .unwrap_or_else(|| config.adb_port());
    let bind = SocketAddr::new(args.bind, port);

    let secret_key = load_or_create_key(&paths.key).await?;
    let builder = TunnelBuilder::new(secret_key)
        .relay_urls(args.relay_url.iter().map(String::as_str))?
        .relay_urls(config.relay_urls.iter().map(String::as_str))?;
    let remote = resolve_target(&target, &builder.relay_urls)?;
    let tunnel = builder.build().await?;

    println!("remoteadb client");
    println!();
    println!("  my endpoint id: {}", tunnel.id());
    println!("  host:           {}", remote.id);
    println!("  local adb port: {bind}");
    println!();
    println!("  export ADB_SERVER_SOCKET=tcp:{bind}");
    println!("  adb devices");
    println!();
    println!(
        "  (if the host rejects you, have its admin run: remoteadb allow {})",
        tunnel.id()
    );

    let serve = tunnel.serve_adb_client(remote, bind);
    tokio::select! {
        res = serve => {
            if let Err(err) = res {
                eprintln!("error: {err:#}");
            }
        }
        _ = signal::ctrl_c() => {
            println!("shutting down...");
        }
    }
    tunnel.close().await
}

async fn edit_allowlist(args: AllowArgs, allow: bool) -> anyhow::Result<()> {
    let id = EndpointId::from_str(args.id.trim())
        .map_err(|e| anyhow::anyhow!("invalid endpoint ID '{}': {e}", args.id))?;
    let paths = Paths::resolve(args.system)?;
    let mut config = Config::load_at(&paths).await?;
    let changed = if allow {
        config.allowed_clients.insert(id.to_string())
    } else {
        config.allowed_clients.remove(&id.to_string())
    };
    config.store_to(&paths.config).await?;
    let verb = if allow { "allowed" } else { "denied" };
    if changed {
        println!("{verb} {id} in {}", paths.config.display());
    } else {
        println!("{id} was already {verb} in {}", paths.config.display());
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("remoteadb=info")),
        )
        .with_writer(io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Host(args) => run_host(args.into(), signal::ctrl_c()).await,
        #[cfg(target_os = "windows")]
        Cmd::RunService(args) => remoteadb::run_windows_service(args.into()).await,
        Cmd::Connect(args) => run_connect(args).await,
        Cmd::Add(args) => {
            let name = args.name.trim().to_string();
            validate_name(&name)?;
            // Validate the target now so a typo is caught here, not at connect.
            let config = Config::load().await?;
            let relays: Vec<RelayUrl> = config
                .relay_urls
                .iter()
                .map(|u| RelayUrl::from_str(u).map_err(|e| anyhow::anyhow!("{u}: {e}")))
                .collect::<Result<_, _>>()?;
            if let Err(err) = resolve_target(&args.ticket, &relays) {
                eprintln!("warning: {err:#}");
            }
            let paths = Paths::user()?;
            let mut user = Config::load_at(&paths).await?;
            user.hosts.insert(
                name.clone(),
                SavedHost {
                    ticket: args.ticket.trim().to_string(),
                    port: args.port,
                },
            );
            user.store_to(&paths.config).await?;
            println!("saved host '{name}'");
            println!();
            println!("  connect with: remoteadb connect {name}");
            Ok(())
        }
        Cmd::List => {
            let config = Config::load().await?;
            if config.hosts.is_empty() {
                println!("no saved hosts. Add one with: remoteadb add --name <NAME> <TICKET>");
            } else {
                for (name, host) in &config.hosts {
                    let port = host
                        .port
                        .map(|p| format!(" (port {p})"))
                        .unwrap_or_default();
                    println!("  {name:<20} {}{port}", host.ticket);
                }
            }
            Ok(())
        }
        Cmd::Remove(args) => {
            let paths = Paths::user()?;
            let mut user = Config::load_at(&paths).await?;
            if user.hosts.remove(&args.name).is_none() {
                bail!("no saved host '{}'", args.name);
            }
            user.store_to(&paths.config).await?;
            println!("removed host '{}'", args.name);
            Ok(())
        }
        Cmd::Allow(args) => edit_allowlist(args, true).await,
        Cmd::Deny(args) => edit_allowlist(args, false).await,
        Cmd::Id(args) => {
            let paths = Paths::resolve(args.system)?;
            let key = load_or_create_key(&paths.key).await?;
            println!("{}", key.public());
            Ok(())
        }
        Cmd::Version => {
            println!("remoteadb v{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Cmd::Paths => {
            let user = Paths::user()?;
            println!("user config:   {}", user.config.display());
            println!("user key:      {}", user.key.display());
            println!("user ticket:   {}", user.ticket.display());
            if let Ok(system) = Paths::system() {
                println!("system config: {}", system.config.display());
                println!("system key:    {}", system.key.display());
                println!("system ticket: {}", system.ticket.display());
            }
            Ok(())
        }
        Cmd::Service { op } => match op {
            ServiceCmd::Install {
                adb_port,
                relay_url,
                user,
            } => {
                let config = Config::load().await?;
                if relay_url.is_empty() && config.relay_urls.is_empty() {
                    bail!(
                        "no relay configured: pass --relay-url or set relay_urls in {}",
                        if user {
                            Paths::user()?.config.display().to_string()
                        } else {
                            Paths::system()?.config.display().to_string()
                        }
                    );
                }
                let binary_path = resolve_binary_path(user)?;
                if !user && !self_runas::is_elevated() {
                    self_runas::admin()?;
                    return Ok(());
                }
                install_service(ServiceParams {
                    adb_port: adb_port.unwrap_or_else(|| config.adb_port()),
                    relay_url,
                    binary_path,
                    user,
                })
                .await?;
                println!("remoteadb service installed.");
                if user {
                    println!(
                        "  tip: `loginctl enable-linger $USER` keeps it running when you log out."
                    );
                }
                println!("  check it with: remoteadb service status");
                Ok(())
            }
            ServiceCmd::Uninstall { user } => {
                if !user && !self_runas::is_elevated() {
                    self_runas::admin()?;
                    return Ok(());
                }
                uninstall_service(user).await?;
                println!("remoteadb service uninstalled.");
                Ok(())
            }
            ServiceCmd::Restart { user } => {
                if !user && !self_runas::is_elevated() {
                    self_runas::admin()?;
                    return Ok(());
                }
                restart_service(user).await?;
                println!("remoteadb service restarted.");
                Ok(())
            }
            ServiceCmd::Status => {
                match service_ticket().await {
                    Some((id, ticket)) => {
                        let port = Config::load().await.map(|c| c.adb_port()).unwrap_or(5037);
                        let listening = adb_listening(port).await;
                        println!("host:        ticket published");
                        println!(
                            "adb server:  {}",
                            if listening {
                                format!("listening on 127.0.0.1:{port}")
                            } else {
                                format!("not listening on 127.0.0.1:{port}")
                            }
                        );
                        println!();
                        println!("  endpoint id: {id}");
                        println!("  ticket:      {ticket}");
                        println!();
                        println!("  on the client: remoteadb connect {ticket}");
                    }
                    None => {
                        println!(
                            "host:        no ticket found (service not installed or not started yet)"
                        );
                    }
                }
                Ok(())
            }
            ServiceCmd::Log { user } => {
                service_log(user)?;
                Ok(())
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_validated() {
        assert!(validate_name("buildbox-1").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("has space").is_err());
    }
}
