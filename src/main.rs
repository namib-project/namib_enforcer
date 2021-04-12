#![warn(clippy::all, clippy::style, clippy::pedantic)]
#![allow(dead_code, clippy::module_name_repetitions, clippy::filter_map)]

#[macro_use]
extern crate log;

use std::sync::Arc;

use dotenv::dotenv;
use tokio::{fs, fs::OpenOptions};

use crate::{
    rpc::rpc_client::current_rpc_context,
    services::{controller_name::apply_secure_name_config, firewall_service::FirewallService},
};
use error::Result;
use namib_shared::{firewall_config::EnforcerConfig, rpc::NamibRpcClient};
use std::{env, net::SocketAddr, path::Path, thread};
use tokio::sync::RwLock;

mod dhcp;
mod error;
mod rpc;
mod services;
mod uci;

/// Default location for the file containing the last received enforcer configuration.
const DEFAULT_CONFIG_STATE_FILE: &str = "/etc/namib/state.json";

pub struct Enforcer {
    pub client: Option<NamibRpcClient>,
    pub addr: Option<SocketAddr>,
    pub config: EnforcerConfig,
}

impl Enforcer {
    /// Applies a new enforcer configuration and persists it to the filesystem for the next start.
    pub(crate) async fn apply_new_config(&mut self, config: EnforcerConfig) {
        self.config = config;
        persist_config(&self.config).await;
    }
}

/// Persists a given enforcer configuration to the filesystem at the location specified by the `NAMIB_CONFIG_STATE_FILE`
/// environment variable (or `DEFAULT_CONFIG_STATE_FILE` if the environment variable is not set).
async fn persist_config(config: &EnforcerConfig) {
    let config_state_path =
        env::var("NAMIB_CONFIG_STATE_FILE").unwrap_or_else(|_| String::from(DEFAULT_CONFIG_STATE_FILE));
    let config_state_path = Path::new(config_state_path.as_str());
    if let Some(parent_dir) = config_state_path.parent() {
        fs::create_dir_all(&parent_dir)
            .await
            .unwrap_or_else(|e| warn!("Error while creating config state parent directory: {:?}", e));
    };
    match serde_json::to_vec(&config) {
        Ok(serialised_bytes) => {
            fs::write(config_state_path, serialised_bytes).await.map_or_else(
                |e| warn!("Error while persisting config state: {:?}", e),
                |_| {
                    debug!(
                        "Persisted configuration at path \"{}\"",
                        config_state_path.to_string_lossy()
                    );
                },
            );
        },
        Err(e) => {
            warn!("Error while serialising config state: {:?}", e);
        },
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    dotenv().ok();
    env_logger::init();

    info!(
        "Starting in {} mode",
        if services::is_system_mode() { "SYSTEM" } else { "USER" }
    );
    if !services::is_system_mode() {
        fs::create_dir_all("config").await?;
        OpenOptions::new()
            .write(true)
            .create(true)
            .open("config/firewall")
            .await?;
    }

    // Attempt to read last persisted enforcer state.
    info!("Reading last saved enforcer state");
    let config_state_path =
        env::var("NAMIB_CONFIG_STATE_FILE").unwrap_or_else(|_| DEFAULT_CONFIG_STATE_FILE.to_string());
    let config: Option<EnforcerConfig> = match fs::read(config_state_path)
        .await
        .map(|state_bytes| serde_json::from_slice(state_bytes.as_slice()))
    {
        Ok(Ok(v)) => Some(v),
        Err(err) => {
            warn!("Error while reading config state file: {:?}", err);
            None
        },
        Ok(Err(err)) => {
            warn!("Error while deserializing config state file: {:?}", err);
            None
        },
    };

    // Restore enforcer config if persisted file could be restored, otherwise wait for the enforcer
    // to provide an initial configuration.
    // Create enforcer instance with provided RPC Client if initial config has been retrieved, with no RPC Client (yet) otherwise.
    let enforcer = if let Some(config) = config {
        info!("Successfully restored last persisted config");
        Arc::new(RwLock::new(Enforcer {
            client: None,
            addr: None,
            config,
        }))
    } else {
        info!("Retrieving initial config from NAMIB Controller");
        let (client, addr) = rpc::rpc_client::run().await?;
        let config = client
            .heartbeat(current_rpc_context(), None)
            .await?
            .expect("no initial config sent from controller");
        persist_config(&config).await;
        info!("Successfully retrieved initial configuration from NAMIB controller");
        Arc::new(RwLock::new(Enforcer {
            client: Some(client),
            addr: Some(addr),
            config,
        }))
    };

    // Instantiate DNS resolver service.
    let mut dns_service = services::dns::DnsService::new().unwrap();

    // Instantiate firewall service with DNS watcher.
    let watcher = dns_service.create_watcher();
    let fw_service = Arc::new(FirewallService::new(enforcer.clone(), watcher));
    fw_service.apply_current_config().await?;

    // If the RPC client was not already retrieved while getting the initial config, get it now.
    if enforcer.read().await.client.is_none() {
        let (client, addr) = rpc::rpc_client::run().await?;
        let mut enf_lock = enforcer.write().await;
        enf_lock.client = Some(client);
        enf_lock.addr = Some(addr);
    }

    // Enforcer is now guaranteed to have an RPC client and a server address.
    {
        let enforcer_read_lock = enforcer.read().await;
        apply_secure_name_config(
            &enforcer_read_lock.config.secure_name(),
            enforcer_read_lock.addr.unwrap(),
        )?;
    }

    let heartbeat_task = rpc::rpc_client::heartbeat(enforcer.clone(), fw_service.clone());

    let dhcp_event_task = dhcp::dhcp_event_listener::listen_for_dhcp_events(enforcer.clone());

    let dns_task = tokio::spawn(async move {
        dns_service.auto_refresher_task().await;
    });
    let _log_watcher = thread::spawn(move || services::log_watcher::watch(&enforcer));

    let firewall_task = tokio::spawn(async move {
        fw_service.firewall_change_watcher().await;
    });

    let ((), (), dns_result, firewall_result) = tokio::join!(heartbeat_task, dhcp_event_task, dns_task, firewall_task);
    dns_result.and(firewall_result)?;
    Ok(())
}
