use crate::lan_api::Client as LanClient;
use crate::service::device::Device;
use crate::service::http::run_http_server;
use crate::service::state::StateHandle;
use anyhow::Context;
use chrono::Utc;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::Duration;

#[derive(clap::Parser, Debug)]
pub struct ServeCommand {
    /// The port on which the HTTP API will listen
    #[arg(long, default_value_t = 8056)]
    http_port: u16,
}

async fn poll_single_device(state: &StateHandle, device: &Device) -> anyhow::Result<()> {
    let now = Utc::now();

    let needs_update = match device.device_state() {
        None => true,
        Some(state) => now - state.updated > chrono::Duration::seconds(900),
    };

    if !needs_update {
        return Ok(());
    }

    // Don't interrogate via HTTP if we can use the LAN.
    // If we have LAN and the device is stale, it is likely
    // offline and there is little sense in burning up request
    // quota to the platform API for it
    if device.lan_device.is_some() {
        log::trace!("LAN-available device {device} needs a status update; it's likely offline.");
        return Ok(());
    }

    if let Some(client) = state.get_platform_client().await {
        if let Some(info) = &device.http_device_info {
            let http_state = client
                .get_device_state(info)
                .await
                .context("get_device_state")?;
            log::trace!("updated state for {device}");
            state
                .device_mut(&device.sku, &device.id)
                .await
                .set_http_device_state(http_state);
        }
    } else {
        log::trace!(
            "device {device} needs a status update, but there is no platform client available"
        );
    }

    Ok(())
}

async fn periodic_state_poll(state: StateHandle) -> anyhow::Result<()> {
    tokio::time::sleep(Duration::from_secs(20)).await;
    loop {
        for d in state.devices().await {
            if let Err(err) = poll_single_device(&state, &d).await {
                log::error!("while polling {d}: {err:#}");
            }
        }

        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

impl ServeCommand {
    pub async fn run(&self, args: &crate::Args) -> anyhow::Result<()> {
        let state = Arc::new(crate::service::state::State::new());

        // First, use the HTTP APIs to determine the list of devices and
        // their names.

        if let Ok(client) = args.api_args.api_client() {
            log::info!("Querying platform API for device list");
            for info in client.get_devices().await? {
                let mut device = state.device_mut(&info.sku, &info.device).await;
                device.set_http_device_info(info);
            }

            state.set_platform_client(client).await;
        }
        if let Ok(client) = args.undoc_args.api_client() {
            log::info!("Querying undocumented API for device + room list");
            let acct = client.login_account().await?;
            let info = client.get_device_list(&acct.token).await?;
            let mut group_by_id = HashMap::new();
            for group in info.groups {
                group_by_id.insert(group.group_id, group.group_name);
            }
            for entry in info.devices {
                let mut device = state.device_mut(&entry.sku, &entry.device).await;
                let room_name = group_by_id.get(&entry.group_id).map(|name| name.as_str());
                device.set_undoc_device_info(entry, room_name);
            }

            // TODO: subscribe to AWS IoT mqtt

            state.set_undoc_client(client).await;
        }

        // Now start discovery

        let options = args.lan_disco_args.to_disco_options();
        if !options.is_empty() {
            log::info!("Starting LAN discovery");
            let state = state.clone();
            let (client, mut scan) = LanClient::new(options).await?;

            state.set_lan_client(client.clone()).await;

            tokio::spawn(async move {
                while let Some(lan_device) = scan.recv().await {
                    state
                        .device_mut(&lan_device.sku, &lan_device.device)
                        .await
                        .set_lan_device(lan_device.clone());

                    if let Ok(status) = client.query_status(&lan_device).await {
                        state
                            .device_mut(&lan_device.sku, &lan_device.device)
                            .await
                            .set_lan_device_status(status);
                    }
                }
            });
        }

        // Start periodic status polling
        {
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(err) = periodic_state_poll(state).await {
                    log::error!("periodic_state_poll: {err:#}");
                }
            });
        }

        // TODO: start advertising on local mqtt

        run_http_server(state.clone(), self.http_port).await
    }
}
