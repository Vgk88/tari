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

use crate::{DhtActorError, DhtConfig, DhtRequester};
use derive_error::Error;
use futures::StreamExt;
use log::*;
use std::{sync::Arc, time::Instant};
use tari_comms::{
    connectivity::{ConnectivityError, ConnectivityEvent, ConnectivityEventRx, ConnectivityRequester},
    peer_manager::{node_id::NodeDistance, NodeId, PeerManagerError, PeerQuery, PeerQuerySortBy},
    NodeIdentity,
    PeerConnection,
    PeerManager,
};
use tari_shutdown::ShutdownSignal;
use tokio::{task, task::JoinHandle};

const LOG_TARGET: &str = "comms::dht::connectivity";

#[derive(Debug, Error)]
pub enum DhtConnectivityError {
    ConnectivityError(ConnectivityError),
    PeerManagerError(PeerManagerError),
    /// Failed to send network Join message
    #[error(no_from)]
    SendJoinFailed(DhtActorError),
}

pub struct DhtConnectivity {
    config: DhtConfig,
    peer_manager: Arc<PeerManager>,
    node_identity: Arc<NodeIdentity>,
    connectivity: ConnectivityRequester,
    dht_requester: DhtRequester,
    /// List of neighbours managed by DhtConnectivity ordered by distance from this node
    neighbours: Vec<NodeId>,
    random_pool: Vec<NodeId>,
    stats: Stats,
    shutdown_signal: Option<ShutdownSignal>,
}

impl DhtConnectivity {
    pub fn new(
        config: DhtConfig,
        peer_manager: Arc<PeerManager>,
        node_identity: Arc<NodeIdentity>,
        connectivity: ConnectivityRequester,
        dht_requester: DhtRequester,
        shutdown_signal: ShutdownSignal,
    ) -> Self
    {
        Self {
            neighbours: Vec::with_capacity(config.num_neighbouring_nodes),
            random_pool: Vec::with_capacity(config.num_random_nodes),
            config,
            peer_manager,
            node_identity,
            connectivity,
            dht_requester,
            stats: Stats::new(),
            shutdown_signal: Some(shutdown_signal),
        }
    }

    /// Spawn a DhtConnectivity actor. This will immediately subscribe to the connection manager event stream to
    /// prevent unexpected missed events.
    pub fn spawn(self) -> JoinHandle<Result<(), DhtConnectivityError>> {
        let connectivity_events = self.connectivity.subscribe_event_stream();
        task::spawn(async move {
            match self.run(connectivity_events).await {
                Ok(_) => Ok(()),
                Err(err) => {
                    error!(target: LOG_TARGET, "DhtConnectivity exited with error: {:?}", err);
                    Err(err)
                },
            }
        })
    }

    pub async fn run(mut self, connectivity_events: ConnectivityEventRx) -> Result<(), DhtConnectivityError> {
        let mut connectivity_events = connectivity_events.fuse();
        let mut shutdown_signal = self
            .shutdown_signal
            .take()
            .expect("DhtConnectivity initialized without a shutdown_signal");

        self.initialize().await?;

        loop {
            futures::select! {
                event = connectivity_events.select_next_some() => {
                    if let Ok(event) = event {
                        if let Err(err) = self.handle_connectivity_event(&event).await {
                            error!(target: LOG_TARGET, "Error handling connectivity event: {:?}", err);
                        }
                    }
               },

               _ = shutdown_signal => {
                    info!(target: LOG_TARGET, "DhtConnectivity shutting down because the shutdown signal was received");
                    break;
               }
            }
        }

        Ok(())
    }

    async fn initialize(&mut self) -> Result<(), DhtConnectivityError> {
        self.neighbours = self
            .fetch_neighbouring_peers(self.config.num_neighbouring_nodes, &[])
            .await?;
        self.random_pool = self
            .fetch_random_peers(self.config.num_random_nodes, &self.neighbours)
            .await?;
        info!(
            target: LOG_TARGET,
            "Adding {} eligible peer(s) ({} neighbouring peer(s), {} random peer(s))",
            self.neighbours.len() + self.random_pool.len(),
            self.neighbours.len(),
            self.random_pool.len()
        );

        let managed = self.get_managed_peers();
        self.connectivity.add_managed_peers(managed).await?;
        Ok(())
    }

    async fn handle_connectivity_event(&mut self, event: &ConnectivityEvent) -> Result<(), DhtConnectivityError> {
        use ConnectivityEvent::*;
        match event {
            PeerConnected(conn) => {
                self.handle_new_peer_connected(conn).await?;
            },
            PeerOffline(node_id) | PeerBanned(node_id) => {
                self.handle_peer_offline(node_id).await?;
            },
            ConnectivityStateDegraded(n) | ConnectivityStateOnline(n) => {
                if self.config.auto_join && self.can_send_join() {
                    info!(target: LOG_TARGET, "Joining the network automatically");
                    self.dht_requester
                        .send_join()
                        .await
                        .map_err(DhtConnectivityError::SendJoinFailed)?;
                    // If we only have 1 peer, we should be able to send another join soon
                    if *n >= 1 {
                        self.stats.mark_join_sent();
                    }
                }
            },
            _ => {},
        }

        Ok(())
    }

    async fn handle_new_peer_connected(&mut self, conn: &PeerConnection) -> Result<(), DhtConnectivityError> {
        if conn.peer_features().is_client() {
            debug!(
                target: LOG_TARGET,
                "Client node '{}' connected",
                conn.peer_node_id().short_str()
            );
            return Ok(());
        }

        if self.is_managed(conn.peer_node_id()) {
            debug!(
                target: LOG_TARGET,
                "Node {} connected that is already managed by DhtConnectivity",
                conn.peer_node_id()
            );
            return Ok(());
        }

        let current_dist = conn.peer_node_id().distance(self.node_identity.node_id());
        let neighbour_distance = self.get_neighbour_max_distance();
        if current_dist < neighbour_distance {
            info!(
                target: LOG_TARGET,
                "Peer '{}' connected that is closer than any current neighbour. Adding to neighbours.",
                conn.peer_node_id().short_str()
            );

            if let Some(node_id) = self.insert_neighbour(conn.peer_node_id().clone()) {
                // If we kicked a neighbour out of our neighbour pool but the random pool is not full.
                // Add the neighbour to the random pool, otherwise remove it
                if self.random_pool.len() < self.config.num_random_nodes {
                    info!(
                        target: LOG_TARGET,
                        "Moving peer '{}' from neighbouring pool to random pool", node_id
                    );
                    self.random_pool.push(node_id);
                } else {
                    info!(target: LOG_TARGET, "Removing peer '{}' from neighbouring pool", node_id);
                    self.connectivity.remove_peer(node_id).await?;
                }
            }
            self.connectivity
                .add_managed_peers(vec![conn.peer_node_id().clone()])
                .await?;

            return Ok(());
        }

        Ok(())
    }

    async fn handle_peer_offline(&mut self, node_id: &NodeId) -> Result<(), DhtConnectivityError> {
        if !self.is_managed(node_id) {
            return Ok(());
        }

        if self.random_pool.contains(node_id) {
            info!(
                target: LOG_TARGET,
                "Peer '{}' in random pool disconnected. Adding a new random peer if possible", node_id
            );
            let exclude = self.get_managed_peers();
            match self.fetch_random_peers(1, &exclude).await?.pop() {
                Some(node_id) => {
                    self.random_pool.push(node_id);
                },
                None => {
                    warn!(
                        target: LOG_TARGET,
                        "Unable to fetch new random peer to replace disconnected peer '{}' because not enough peers \
                         are known. Random pool size is {}.",
                        node_id,
                        self.random_pool.len()
                    );
                },
            }
        }

        if self.neighbours.contains(node_id) {
            info!(
                target: LOG_TARGET,
                "Peer '{}' in neighbour pool disconnected. Adding a new peer if possible", node_id
            );
            let exclude = self.get_managed_peers();
            match self.fetch_neighbouring_peers(1, &exclude).await?.pop() {
                Some(node_id) => {
                    self.random_pool.push(node_id);
                },
                None => {
                    warn!(
                        target: LOG_TARGET,
                        "Unable to fetch new neighbouring peer to replace disconnected peer '{}'. Neighbour pool size \
                         is {}.",
                        node_id,
                        self.neighbours.len()
                    );
                },
            }
        }

        self.connectivity.remove_peer(node_id.clone()).await?;

        Ok(())
    }

    fn insert_neighbour(&mut self, node_id: NodeId) -> Option<NodeId> {
        let dist = node_id.distance(self.node_identity.node_id());
        let pos = self.neighbours.iter().position(|node_id| {
            let d = node_id.distance(self.node_identity.node_id());
            d > dist
        });

        let mut removed_peer = None;
        if self.neighbours.len() + 1 > self.config.num_neighbouring_nodes {
            removed_peer = self.neighbours.pop();
        }

        match pos {
            Some(idx) => {
                self.neighbours.insert(idx, node_id);
            },
            None => {
                self.neighbours.push(node_id);
            },
        }

        removed_peer
    }

    fn is_managed(&self, node_id: &NodeId) -> bool {
        self.neighbours.contains(node_id) || self.random_pool.contains(node_id)
    }

    fn get_managed_peers(&self) -> Vec<NodeId> {
        self.neighbours.iter().chain(self.random_pool.iter()).cloned().collect()
    }

    fn get_neighbour_max_distance(&self) -> NodeDistance {
        assert!(
            self.config.num_neighbouring_nodes > 0,
            "DhtConfig::num_neighbouring_nodes must be greater than zero"
        );

        if self.neighbours.len() < self.config.num_neighbouring_nodes {
            return NodeDistance::max_distance();
        }

        self.neighbours
            .last()
            .map(|node_id| node_id.distance(self.node_identity.node_id()))
            .expect("already checked")
    }

    async fn fetch_neighbouring_peers(
        &self,
        n: usize,
        excluded: &[NodeId],
    ) -> Result<Vec<NodeId>, DhtConnectivityError>
    {
        let config = self.config.clone();
        let peer_manager = &self.peer_manager;
        let node_id = self.node_identity.node_id();
        // Fetch to all n nearest neighbour Communication Nodes
        // which are eligible for connection.
        // Currently that means:
        // - The peer isn't banned,
        // - it has the required features
        // - it didn't recently fail to connect, and
        // - it is not in the exclusion list in closest_request
        let mut connect_ineligable_count = 0;
        let mut banned_count = 0;
        let mut excluded_count = 0;
        let mut filtered_out_node_count = 0;
        let query = PeerQuery::new()
            .select_where(|peer| {
                if peer.is_banned() {
                    trace!(target: LOG_TARGET, "[{}] is banned", peer.node_id);
                    banned_count += 1;
                    return false;
                }

                if peer.features.is_client() {
                    filtered_out_node_count += 1;
                    return false;
                }

                let is_connect_eligible = {
                    !peer.is_offline() &&
                        // Check this peer was recently connectable
                        (peer.connection_stats.failed_attempts() <= config.broadcast_cooldown_max_attempts ||
                            peer.connection_stats
                                .time_since_last_failure()
                                .map(|failed_since| failed_since >= config.broadcast_cooldown_period)
                                .unwrap_or(true))
                };

                if !is_connect_eligible {
                    trace!(
                        target: LOG_TARGET,
                        "[{}] suffered too many connection attempt failures or is offline",
                        peer.node_id
                    );
                    connect_ineligable_count += 1;
                    return false;
                }

                let is_excluded = excluded.contains(&peer.node_id);
                if is_excluded {
                    excluded_count += 1;
                    return false;
                }

                true
            })
            .sort_by(PeerQuerySortBy::DistanceFrom(&node_id))
            .limit(n);

        let peers = peer_manager.perform_query(query).await?;
        let total_excluded = banned_count + connect_ineligable_count + excluded_count + filtered_out_node_count;
        if total_excluded > 0 {
            debug!(
                target: LOG_TARGET,
                "\n====================================\n Closest Peer Selection\n\n {num_peers} peer(s) selected\n \
                 {total} peer(s) were not selected \n\n {banned} banned\n {filtered_out} not communication node\n \
                 {not_connectable} are not connectable\n {excluded} explicitly excluded \
                 \n====================================\n",
                num_peers = peers.len(),
                total = total_excluded,
                banned = banned_count,
                filtered_out = filtered_out_node_count,
                not_connectable = connect_ineligable_count,
                excluded = excluded_count
            );
        }

        Ok(peers.into_iter().map(|p| p.node_id).collect())
    }

    async fn fetch_random_peers(&self, n: usize, excluded: &[NodeId]) -> Result<Vec<NodeId>, DhtConnectivityError> {
        let peers = self.peer_manager.random_peers(n, excluded).await?;
        Ok(peers.into_iter().map(|p| p.node_id).collect())
    }

    fn can_send_join(&self) -> bool {
        let cooldown = self.config.join_cooldown_interval;
        self.stats
            .join_last_sent_at()
            .map(|at| at.elapsed() > cooldown)
            .unwrap_or(true)
    }
}

/// Basic connectivity stats
#[derive(Debug, Default)]
struct Stats {
    join_last_sent_at: Option<Instant>,
}

impl Stats {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn join_last_sent_at(&self) -> Option<Instant> {
        self.join_last_sent_at
    }

    pub fn mark_join_sent(&mut self) {
        self.join_last_sent_at = Some(Instant::now());
    }
}
