//  Copyright 2020, The Tari Project
//
//  Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
//  following conditions are met:
//
//  1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
//  disclaimer.
//
//  2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
//  following disclaimer in the documentation and/or other materials provided with the distribution.
//
//  3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
//  products derived from this software without specific prior written permission.
//
//  THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
//  INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
//  DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//  SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
//  SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
//  WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
//  USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
use super::{
    config::ConnectivityConfig,
    connection_pool::{ConnectionPool, ConnectionStatus},
    error::ConnectivityError,
    requester::{ConnectivityEvent, ConnectivityRequest, ConnectivitySelection},
    selection,
};
use crate::{
    connection_manager::ConnectionManagerRequester,
    peer_manager::NodeId,
    utils::datetime::format_duration,
    ConnectionManagerEvent,
    PeerConnection,
    PeerManager,
};
use futures::{
    channel::{mpsc, oneshot},
    stream::Fuse,
    StreamExt,
};
use log::*;
use std::{
    fmt,
    sync::Arc,
    time::{Duration, Instant},
};
use tari_shutdown::ShutdownSignal;
use tokio::{sync::broadcast, task, task::JoinHandle, time};

const LOG_TARGET: &str = "comms::connectivity::manager";

pub struct ConnectivityManager {
    pub config: ConnectivityConfig,
    pub request_rx: mpsc::Receiver<ConnectivityRequest>,
    pub event_tx: broadcast::Sender<Arc<ConnectivityEvent>>,
    pub connection_manager: ConnectionManagerRequester,
    pub peer_manager: Arc<PeerManager>,
    pub shutdown_signal: ShutdownSignal,
}

impl ConnectivityManager {
    pub fn create(self) -> ConnectivityManagerActor {
        ConnectivityManagerActor {
            config: self.config,
            status: ConnectivityStatus::Initializing,
            request_rx: self.request_rx.fuse(),
            connection_manager: self.connection_manager,
            peer_manager: self.peer_manager.clone(),
            event_tx: self.event_tx,

            eligible_peers: Vec::new(),

            shutdown_signal: Some(self.shutdown_signal),
            pool: ConnectionPool::new(),
        }
    }

    pub fn spawn(self) -> JoinHandle<()> {
        self.create().spawn()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectivityStatus {
    Initializing,
    Online,
    Degraded,
    Offline,
}

impl fmt::Display for ConnectivityStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

pub struct ConnectivityManagerActor {
    config: ConnectivityConfig,
    status: ConnectivityStatus,
    request_rx: Fuse<mpsc::Receiver<ConnectivityRequest>>,
    connection_manager: ConnectionManagerRequester,
    shutdown_signal: Option<ShutdownSignal>,
    peer_manager: Arc<PeerManager>,
    event_tx: broadcast::Sender<Arc<ConnectivityEvent>>,

    eligible_peers: Vec<NodeId>,
    pool: ConnectionPool,
}

impl ConnectivityManagerActor {
    pub fn spawn(self) -> JoinHandle<()> {
        task::spawn(Self::run(self))
    }

    pub async fn run(mut self) {
        info!(target: LOG_TARGET, "ConnectivityManager started");
        let mut shutdown_signal = self
            .shutdown_signal
            .take()
            .expect("ConnectivityManager initialized without a shutdown_signal");

        let mut connection_manager_events = self.connection_manager.get_event_subscription().fuse();

        let interval = self.config.connection_pool_refresh_interval;
        let mut ticker = time::interval_at(
            Instant::now()
                .checked_add(interval)
                .expect("connection_pool_refresh_interval cause overflow")
                .into(),
            interval,
        )
        .fuse();

        self.publish_event(ConnectivityEvent::ConnectivityStateInitialized);
        self.check_connectivity_status();

        loop {
            futures::select! {
                req = self.request_rx.select_next_some() => {
                    self.handle_request(req).await;
                },

                event = connection_manager_events.select_next_some() => {
                    if let Ok(event) = event {
                        self.handle_connection_manager_event(&event).await;
                    }
                },

                _ = ticker.next() => {
                    if let Err(err) = self.refresh_connection_pool().await {
                        error!(target: LOG_TARGET, "Error when refreshing connection pools: {:?}", err);
                    }
                },

                _ = shutdown_signal => {
                    info!(target: LOG_TARGET, "ConnectivityManager is shutting down because it received the shutdown signal");
                    break;
                }
            }
        }
    }

    async fn handle_request(&mut self, req: ConnectivityRequest) {
        use ConnectivityRequest::*;
        trace!(target: LOG_TARGET, "Request: {:?}", req);
        match req {
            AddEligiblePeers(node_ids) => {
                self.add_eligible_peers(node_ids).await;
            },
            SelectConnections(selection, reply_tx) => {
                self.select_connections(selection, reply_tx).await;
            },
            GetConnection(node_id, reply_tx) => {
                let _ = reply_tx.send(
                    self.pool
                        .get_connection(&node_id)
                        .filter(|conn| conn.is_connected())
                        .cloned(),
                );
            },
            GetAll(reply_tx) => {
                let _ = reply_tx.send(self.pool.all().into_iter().cloned().collect());
            },
            BanPeer(node_id, duration) => {
                if let Err(err) = self.ban_peer(&node_id, duration).await {
                    error!(target: LOG_TARGET, "Error when banning peer: {:?}", err);
                }
            },
        }
    }

    async fn refresh_connection_pool(&mut self) -> Result<(), ConnectivityError> {
        self.disconnect_unused_connections().await;

        // Attempt to connect all eligible peers: Failed, Disconnected or NotConnection will be dialed
        self.try_connect_eligible_peers().await?;

        // Remove disconnected/failed peers from the connection pool
        self.clean_connection_pool();

        Ok(())
    }

    async fn try_connect_eligible_peers(&mut self) -> Result<(), ConnectivityError> {
        for node_id in &self.eligible_peers {
            match self.pool.get_connection_status(node_id) {
                ConnectionStatus::Failed | ConnectionStatus::Disconnected => {
                    let status = self.pool.set_status(node_id, ConnectionStatus::Retrying);
                    info!(
                        target: LOG_TARGET,
                        "{} peer '{}' is eligible for connection. Retrying.", status, node_id
                    );
                    self.connection_manager.send_dial_peer(node_id.clone()).await?;
                },
                ConnectionStatus::NotConnected => {
                    let status = self.pool.set_status(node_id, ConnectionStatus::Connecting);
                    info!(
                        target: LOG_TARGET,
                        "{} peer '{}' is eligible for connection. Connecting.", status, node_id
                    );
                    self.connection_manager.send_dial_peer(node_id.clone()).await?;
                },
                _ => {},
            }
        }

        Ok(())
    }

    async fn disconnect_unused_connections(&mut self) {
        let connections = self.pool.get_unused_connections_mut();
        for conn in connections {
            if let Err(err) = conn.disconnect().await {
                warn!(
                    target: LOG_TARGET,
                    "Peer '{}' already disconnected. Error: {:?}",
                    conn.peer_node_id().short_str(),
                    err
                );
            }
        }
    }

    fn clean_connection_pool(&mut self) {
        let eligible_peers = self.eligible_peers.clone();
        let cleared_states = self.pool.filter_drain(|state| {
            (state.status() == ConnectionStatus::Failed || state.status() == ConnectionStatus::Disconnected) &&
                !eligible_peers.contains(state.node_id())
        });
        info!(
            target: LOG_TARGET,
            "Cleared {} disconnected/failed connection states",
            cleared_states.len()
        );
        debug!(
            target: LOG_TARGET,
            "Cleared connection states: {}",
            cleared_states
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    async fn select_connections(
        &self,
        selection: ConnectivitySelection,
        reply_tx: oneshot::Sender<Result<Vec<PeerConnection>, ConnectivityError>>,
    )
    {
        use ConnectivitySelection::*;
        debug!(target: LOG_TARGET, "Selection query: {:?}", selection);
        let conns = match selection {
            RandomNodes(n, exclude) => selection::select_random_nodes(&self.pool, n, &exclude),
            ClosestTo(dest_node_id, n) => {
                let connections = selection::select_closest(&self.pool, &dest_node_id);
                connections[0..n].to_vec()
            },
        };
        debug!(target: LOG_TARGET, "Selected {} connections(s)", conns.len());

        let _ = reply_tx.send(Ok(conns.into_iter().cloned().collect()));
    }

    async fn add_eligible_peers(&mut self, node_ids: Vec<NodeId>) {
        let pool = &mut self.pool;
        for node_id in node_ids {
            if !self.eligible_peers.contains(&node_id) {
                self.eligible_peers.push(node_id.clone());
            }

            match pool.insert(node_id.clone()) {
                ConnectionStatus::Failed => {
                    pool.set_status(&node_id, ConnectionStatus::Retrying);
                    if let Err(err) = self.connection_manager.send_dial_peer(node_id.clone()).await {
                        error!(
                            target: LOG_TARGET,
                            "Failed to send dial request to connection manager: {:?}", err
                        );
                        // Remove from this pool, it may be re-added later by the periodic connection refresh
                        pool.remove(&node_id);
                    }
                },
                ConnectionStatus::NotConnected | ConnectionStatus::Disconnected => {
                    pool.set_status(&node_id, ConnectionStatus::Connecting);
                    if let Err(err) = self.connection_manager.send_dial_peer(node_id.clone()).await {
                        error!(
                            target: LOG_TARGET,
                            "Failed to send dial request to connection manager: {:?}", err
                        );
                    }
                },
                status => info!(
                    target: LOG_TARGET,
                    "Eligible peer '{}' with connection status {} will not be retried", node_id, status
                ),
            }
        }
    }

    async fn handle_connection_manager_event(&mut self, event: &ConnectionManagerEvent) {
        use ConnectionManagerEvent::*;
        let (node_id, new_status) = match event {
            PeerDisconnected(node_id) => (&**node_id, ConnectionStatus::Disconnected),
            PeerConnected(conn) => {
                self.pool.insert_connection(conn.clone());
                self.check_connectivity_status();
                (conn.peer_node_id(), ConnectionStatus::Connected)
            },
            PeerConnectFailed(node_id, err) => {
                info!(
                    target: LOG_TARGET,
                    "Connection to peer '{}' failed because '{:?}'", node_id, err
                );
                (&**node_id, ConnectionStatus::Failed)
            },
            _ => return,
        };

        let old_status = self.pool.set_status(node_id, new_status);
        if old_status != new_status {
            info!(
                target: LOG_TARGET,
                "Peer connection for node '{}' transitioned from {} to {}", node_id, old_status, new_status
            );
        }

        let is_eligible = self.eligible_peers.contains(node_id);
        let node_id = node_id.clone();

        use ConnectionStatus::*;
        match (old_status, new_status) {
            (_, Connected) => {
                // This condition is more about `get_connection` returning an option than anything else.
                // The `PeerConnection` could conceivably be disconnected at any time so `unwrap`ing the
                // connection is incorrect.
                if let Some(conn) = self.pool.get_connection(&node_id).cloned() {
                    self.publish_event(ConnectivityEvent::PeerConnected(conn));
                }
            },
            (Connected, Disconnected) => {
                if is_eligible {
                    self.publish_event(ConnectivityEvent::EligiblePeerDisconnected(node_id));
                } else {
                    self.publish_event(ConnectivityEvent::PeerDisconnected(node_id));
                }
            },
            (_, Disconnected) => {},
            (_, Failed) => {
                if is_eligible {
                    self.publish_event(ConnectivityEvent::EligiblePeerConnectFailed(node_id));
                } else {
                    self.publish_event(ConnectivityEvent::PeerConnectFailed(node_id));
                }
            },
            _ => {
                error!(
                    target: LOG_TARGET,
                    "Unexpected connection status transition ({} to {}) for peer '{}'", old_status, new_status, node_id
                );
            },
        }

        self.check_connectivity_status();
    }

    fn check_connectivity_status(&mut self) {
        let min_peers = self.config.min_desired_peers;
        let num_connected = self.pool.count_connected();

        match num_connected {
            n if n >= min_peers => {
                self.transition(ConnectivityStatus::Online, n);
            },
            n if n > 0 && n < min_peers => {
                if self.status != ConnectivityStatus::Initializing {
                    self.transition(ConnectivityStatus::Degraded, n);
                }
            },
            n if n == 0 => {
                if self.pool.count_failed() > 0 {
                    self.transition(ConnectivityStatus::Offline, n);
                }
            },
            _ => unreachable!("num_connected is unsigned and only negative pattern covered on this branch"),
        }
    }

    fn transition(&mut self, next_status: ConnectivityStatus, num_peers: usize) {
        use ConnectivityStatus::*;
        if self.status != next_status {
            debug!(
                target: LOG_TARGET,
                "Connectivity status transitioning from {} to {}", self.status, next_status
            );
        }

        match (self.status, next_status) {
            (Online, Online) => {},
            (_, Online) => {
                info!(
                    target: LOG_TARGET,
                    "Connectivity is ONLINE ({}/{} connections)", num_peers, self.config.min_desired_peers
                );
                self.publish_event(ConnectivityEvent::ConnectivityStateReady(num_peers));
            },
            (Degraded, Degraded) => {},
            (Online, Degraded) | (Offline, Degraded) => {
                warn!(
                    target: LOG_TARGET,
                    "Connectivity is DEGRADED ({}/{} connections)", num_peers, self.config.min_desired_peers
                );
                self.publish_event(ConnectivityEvent::ConnectivityStateDegraded(num_peers));
            },
            (Offline, Offline) => {},
            (_, Offline) => {
                warn!(
                    target: LOG_TARGET,
                    "Connectivity is OFFLINE (0/{} connections)", self.config.min_desired_peers
                );
                self.publish_event(ConnectivityEvent::ConnectivityStateOffline);
            },
            (status, next_status) => unreachable!("Unexpected status transition ({} to {})", status, next_status),
        }
        self.status = next_status;
    }

    fn publish_event(&mut self, event: ConnectivityEvent) {
        // A send operation can only fail if there are no subscribers, so it is safe to ignore the error
        let _ = self.event_tx.send(Arc::new(event));
    }

    async fn ban_peer(&mut self, node_id: &NodeId, duration: Duration) -> Result<(), ConnectivityError> {
        info!(target: LOG_TARGET, "Banning peer for {}", format_duration(duration));

        if let Some(pos) = self.eligible_peers.iter().position(|n| n == node_id) {
            let node_id = self.eligible_peers.remove(pos);
            warn!(target: LOG_TARGET, "Banned eligible peer '{}'", node_id);
        }

        self.peer_manager.ban_peer_by_node_id(node_id, duration).await?;

        self.publish_event(ConnectivityEvent::PeerBanned(node_id.clone()));

        if self.pool.contains(node_id) {
            self.connection_manager.disconnect_peer(node_id.clone()).await?;
            self.pool.set_status(node_id, ConnectionStatus::Disconnected);
        }
        Ok(())
    }
}
