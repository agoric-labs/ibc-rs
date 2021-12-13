use tracing::{debug, error};

use ibc::core::{
    ics02_client::client_state::{ClientState, IdentifiedAnyClientState},
    ics03_connection::connection::IdentifiedConnectionEnd,
    ics04_channel::channel::State as ChannelState,
    ics24_host::identifier::ChainId,
};

use crate::{
    chain::{counterparty::connection_state_on_destination, handle::ChainHandle},
    config::Config,
    object::{Channel, Client, Connection, Object, Packet},
    registry::SharedRegistry,
    supervisor::error::Error as SupervisorError,
    worker::WorkerMap,
};

use super::{
    scan::{ChainScan, ChainsScan, ChannelScan, ClientScan, ConnectionScan},
    Error, RwArc,
};

/// A context for spawning workers within the [`crate::supervisor::Supervisor`].
pub struct SpawnContext<'a, Chain: ChainHandle> {
    config: RwArc<Config>,
    registry: SharedRegistry<Chain>,
    workers: &'a mut WorkerMap,
}

impl<'a, Chain: ChainHandle + 'static> SpawnContext<'a, Chain> {
    pub fn new(
        config: RwArc<Config>,
        registry: SharedRegistry<Chain>,
        workers: &'a mut WorkerMap,
    ) -> Self {
        Self {
            config,
            registry,
            workers,
        }
    }

    pub fn spawn_workers(&mut self, scan: ChainsScan) {
        for chain_scan in scan.chains {
            match chain_scan {
                Ok(chain_scan) => self.spawn_workers_for_chain(chain_scan),
                Err(e) => error!("failed to spawn worker for a chain, reason: {}", e), // TODO: Show chain id
            }
        }
    }

    pub fn spawn_workers_for_chain(&mut self, scan: ChainScan) {
        let chain = match self.registry.get_or_spawn(&scan.chain_id) {
            Ok(chain_handle) => chain_handle,
            Err(e) => {
                error!(
                    "skipping workers for chain {}, reason: failed to spawn chain runtime with error: {}",
                    scan.chain_id, e
                );

                return;
            }
        };

        println!("W: Chain: {}", chain.id());

        for (_, client_scan) in scan.clients {
            self.spawn_workers_for_client(chain.clone(), client_scan);
        }
    }

    pub fn spawn_workers_for_client(&mut self, chain: Chain, client_scan: ClientScan) {
        for (_, connection_scan) in client_scan.connections {
            self.spawn_workers_for_connection(chain.clone(), &client_scan.client, connection_scan);
        }
    }

    pub fn spawn_workers_for_connection(
        &mut self,
        chain: Chain,
        client: &IdentifiedAnyClientState,
        connection_scan: ConnectionScan,
    ) {
        let connection_id = connection_scan.id().clone();

        match self.spawn_connection_workers(
            chain.clone(),
            client.clone(),
            connection_scan.connection,
        ) {
            Ok(()) => debug!(
                "done spawning workers for connection {} on chain {}",
                connection_id,
                chain.id(),
            ),
            Err(e) => error!(
                "skipped workers for connection {} on chain {}, reason: {}",
                connection_id,
                chain.id(),
                e
            ),
        }

        for (channel_id, channel_scan) in connection_scan.channels {
            match self.spawn_workers_for_channel(chain.clone(), client, channel_scan) {
                Ok(()) => debug!(
                    "done spawning workers for chain {} and channel {}",
                    chain.id(),
                    channel_id,
                ),
                Err(e) => error!(
                    "skipped workers for chain {} and channel {} due to error {}",
                    chain.id(),
                    channel_id,
                    e
                ),
            }
        }
    }

    fn spawn_connection_workers(
        &mut self,
        chain: Chain,
        client: IdentifiedAnyClientState,
        connection: IdentifiedConnectionEnd,
    ) -> Result<(), Error> {
        let config_conn_enabled = self
            .config
            .read()
            .expect("poisoned lock")
            .mode
            .connections
            .enabled;

        let counterparty_chain = self
            .registry
            .get_or_spawn(&client.client_state.chain_id())
            .map_err(Error::spawn)?;

        let conn_state_src = connection.connection_end.state;
        let conn_state_dst = connection_state_on_destination(&connection, &counterparty_chain)?;

        debug!(
            "connection {} on chain {} is: {:?}, state on dest. chain ({}) is: {:?}",
            connection.connection_id,
            chain.id(),
            conn_state_src,
            counterparty_chain.id(),
            conn_state_dst
        );

        println!("W:     Connection: {}", connection.connection_id);

        if conn_state_src.is_open() && conn_state_dst.is_open() {
            debug!(
                "connection {} on chain {} is already open, not spawning Connection worker",
                connection.connection_id,
                chain.id()
            );
        } else if config_conn_enabled
            && !conn_state_dst.is_open()
            && conn_state_dst.less_or_equal_progress(conn_state_src)
        {
            // create worker for connection handshake that will advance the remote state
            let connection_object = Object::Connection(Connection {
                dst_chain_id: client.client_state.chain_id(),
                src_chain_id: chain.id(),
                src_connection_id: connection.connection_id,
            });

            self.workers
                .spawn(
                    chain,
                    counterparty_chain,
                    &connection_object,
                    &self.config.read().expect("poisoned lock"),
                )
                .then(|| {
                    debug!(
                        "spawning Connection worker: {}",
                        connection_object.short_name()
                    );
                });
        }

        Ok(())
    }

    /// Spawns all the [`Worker`](crate::worker::Worker)s that will
    /// handle a given channel for a given source chain.
    pub fn spawn_workers_for_channel(
        &mut self,
        chain: Chain,
        client: &IdentifiedAnyClientState,
        channel_scan: ChannelScan,
    ) -> Result<(), Error> {
        let mode = &self.config.read().expect("poisoned lock").mode;

        let counterparty_chain = self
            .registry
            .get_or_spawn(&client.client_state.chain_id())
            .map_err(SupervisorError::spawn)?;

        let chan_state_src = channel_scan.channel.channel_end.state;
        let chan_state_dst = channel_scan
            .counterparty
            .as_ref()
            .map_or(ChannelState::Uninitialized, |c| c.channel_end.state);

        debug!(
            "channel {} on chain {} is: {}; state on dest. chain ({}) is: {}",
            channel_scan.id(),
            chain.id(),
            chan_state_src,
            counterparty_chain.id(),
            chan_state_dst
        );

        if (mode.clients.enabled || mode.packets.enabled)
            && chan_state_src.is_open()
            && chan_state_dst.is_open()
        {
            if mode.clients.enabled {
                // Spawn the client worker
                let client_object = Object::Client(Client {
                    dst_client_id: client.client_id.clone(),
                    dst_chain_id: chain.id(),
                    src_chain_id: client.client_state.chain_id(),
                });

                println!("W:   Client: {}", client.client_id);

                self.workers
                    .spawn(
                        counterparty_chain.clone(),
                        chain.clone(),
                        &client_object,
                        &self.config.read().expect("poisoned lock"),
                    )
                    .then(|| debug!("spawned Client worker: {}", client_object.short_name()));
            }

            if mode.packets.enabled {
                let has_packets = || {
                    !channel_scan
                        .unreceived_packets_on_counterparty(&chain, &counterparty_chain)
                        .unwrap_or_default()
                        .is_empty()
                };

                let has_acks = || {
                    !channel_scan
                        .unreceived_acknowledgements_on_counterparty(&chain, &counterparty_chain)
                        .unwrap_or_default()
                        .is_empty()
                };

                // If there are any outstanding packets or acks to send, spawn the worker
                if true || has_packets() || has_acks() {
                    // Create the Packet object and spawn worker
                    let path_object = Object::Packet(Packet {
                        dst_chain_id: counterparty_chain.id(),
                        src_chain_id: chain.id(),
                        src_channel_id: channel_scan.id().clone(),
                        src_port_id: channel_scan.channel.port_id.clone(),
                    });

                    println!("W:       Channel: {}", channel_scan.id());

                    self.workers
                        .spawn(
                            chain.clone(),
                            counterparty_chain.clone(),
                            &path_object,
                            &self.config.read().expect("poisoned lock"),
                        )
                        .then(|| debug!("spawned Packet worker: {}", path_object.short_name()));
                }
            }
        } else if mode.channels.enabled
            && !chan_state_dst.is_open()
            && chan_state_dst.less_or_equal_progress(chan_state_src)
        {
            // create worker for channel handshake that will advance the remote state
            let channel_object = Object::Channel(Channel {
                dst_chain_id: counterparty_chain.id(),
                src_chain_id: chain.id(),
                src_channel_id: channel_scan.id().clone(),
                src_port_id: channel_scan.channel.port_id,
            });

            self.workers
                .spawn(
                    chain,
                    counterparty_chain,
                    &channel_object,
                    &self.config.read().expect("poisoned lock"),
                )
                .then(|| debug!("spawned Channel worker: {}", channel_object.short_name()));
        }

        Ok(())
    }

    pub fn shutdown_workers_for_chain(&mut self, chain_id: &ChainId) {
        let affected_workers = self.workers.objects_for_chain(chain_id);
        for object in affected_workers {
            self.workers.shutdown_worker(&object);
        }
    }
}
