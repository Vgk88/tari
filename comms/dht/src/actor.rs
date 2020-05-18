// Copyright 2019, The Tari Project
//
// Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
// following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
// disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
// following disclaimer in the documentation and/or other materials provided with the distribution.
//
// 3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
// products derived from this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
// INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
// SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
// WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE
// USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

//! Actor for DHT functionality.
//!
//! The DhtActor is responsible for sending a join request on startup
//! and furnishing [DhtRequest]s.
//!
//! [DhtRequest]: ./enum.DhtRequest.html

use crate::{
    broadcast_strategy::BroadcastStrategy,
    discovery::DhtDiscoveryError,
    outbound::{OutboundMessageRequester, SendMessageParams},
    proto::{dht::JoinMessage, envelope::DhtMessageType},
    storage::{DbConnection, DhtDatabase, DhtMetadataKey, StorageError},
    DhtConfig,
};
use chrono::{DateTime, Utc};
use derive_error::Error;
use futures::{
    channel::{mpsc, mpsc::SendError, oneshot},
    future,
    future::BoxFuture,
    stream::{Fuse, FuturesUnordered},
    SinkExt,
    StreamExt,
};
use log::*;
use std::{fmt, fmt::Display, sync::Arc};
use tari_comms::{
    connection_manager::ConnectionManagerError,
    connectivity::{ConnectivityError, ConnectivityRequester, ConnectivitySelection},
    peer_manager::{NodeId, NodeIdentity, PeerFeatures, PeerManager, PeerManagerError, PeerQuery, PeerQuerySortBy},
};
use tari_shutdown::ShutdownSignal;
use tari_utilities::message_format::{MessageFormat, MessageFormatError};
use tokio::task;
use ttl_cache::TtlCache;

const LOG_TARGET: &str = "comms::dht::actor";

#[derive(Debug, Error)]
pub enum DhtActorError {
    /// MPSC channel is disconnected
    ChannelDisconnected,
    /// MPSC sender was unable to send because the channel buffer is full
    SendBufferFull,
    /// Reply sender canceled the request
    ReplyCanceled,
    PeerManagerError(PeerManagerError),
    #[error(msg_embedded, no_from, non_std)]
    SendFailed(String),
    DiscoveryError(DhtDiscoveryError),
    BlockingJoinError(tokio::task::JoinError),
    StorageError(StorageError),
    #[error(no_from)]
    StoredValueFailedToDeserialize(MessageFormatError),
    #[error(no_from)]
    FailedToSerializeValue(MessageFormatError),
    ConnectionManagerError(ConnectionManagerError),
    ConnectivityError(ConnectivityError),
    /// Connectivity event stream closed
    ConnectivityEventStreamClosed,
}

impl From<SendError> for DhtActorError {
    fn from(err: SendError) -> Self {
        if err.is_disconnected() {
            DhtActorError::ChannelDisconnected
        } else if err.is_full() {
            DhtActorError::SendBufferFull
        } else {
            unreachable!();
        }
    }
}

#[derive(Debug)]
pub enum DhtRequest {
    /// Send a Join request to the network
    SendJoin,
    /// Inserts a message signature to the msg hash cache. This operation replies with a boolean
    /// which is true if the signature already exists in the cache, otherwise false
    MsgHashCacheInsert(Vec<u8>, oneshot::Sender<bool>),
    /// Fetch selected peers according to the broadcast strategy
    SelectPeers(BroadcastStrategy, oneshot::Sender<Vec<NodeId>>),
    GetMetadata(DhtMetadataKey, oneshot::Sender<Result<Option<Vec<u8>>, DhtActorError>>),
    SetMetadata(DhtMetadataKey, Vec<u8>),
}

impl Display for DhtRequest {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use DhtRequest::*;
        match self {
            SendJoin => f.write_str("SendJoin"),
            MsgHashCacheInsert(_, _) => f.write_str("MsgHashCacheInsert"),
            SelectPeers(s, _) => f.write_str(&format!("SelectPeers (Strategy={})", s)),
            GetMetadata(key, _) => f.write_str(&format!("GetSetting (key={})", key)),
            SetMetadata(key, value) => f.write_str(&format!("SetSetting (key={}, value={} bytes)", key, value.len())),
        }
    }
}

#[derive(Clone)]
pub struct DhtRequester {
    sender: mpsc::Sender<DhtRequest>,
}

impl DhtRequester {
    pub fn new(sender: mpsc::Sender<DhtRequest>) -> Self {
        Self { sender }
    }

    pub async fn send_join(&mut self) -> Result<(), DhtActorError> {
        self.sender.send(DhtRequest::SendJoin).await.map_err(Into::into)
    }

    pub async fn select_peers(&mut self, broadcast_strategy: BroadcastStrategy) -> Result<Vec<NodeId>, DhtActorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender
            .send(DhtRequest::SelectPeers(broadcast_strategy, reply_tx))
            .await?;
        reply_rx.await.map_err(|_| DhtActorError::ReplyCanceled)
    }

    pub async fn insert_message_hash(&mut self, signature: Vec<u8>) -> Result<bool, DhtActorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender
            .send(DhtRequest::MsgHashCacheInsert(signature, reply_tx))
            .await?;

        reply_rx.await.map_err(|_| DhtActorError::ReplyCanceled)
    }

    pub async fn get_metadata<T: MessageFormat>(&mut self, key: DhtMetadataKey) -> Result<Option<T>, DhtActorError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender.send(DhtRequest::GetMetadata(key, reply_tx)).await?;
        match reply_rx.await.map_err(|_| DhtActorError::ReplyCanceled)?? {
            Some(bytes) => T::from_binary(&bytes)
                .map(Some)
                .map_err(DhtActorError::StoredValueFailedToDeserialize),
            None => Ok(None),
        }
    }

    pub async fn set_metadata<T: MessageFormat>(&mut self, key: DhtMetadataKey, value: T) -> Result<(), DhtActorError> {
        let bytes = value.to_binary().map_err(DhtActorError::FailedToSerializeValue)?;
        self.sender.send(DhtRequest::SetMetadata(key, bytes)).await?;
        Ok(())
    }
}

pub struct DhtActor {
    node_identity: Arc<NodeIdentity>,
    peer_manager: Arc<PeerManager>,
    database: DhtDatabase,
    outbound_requester: OutboundMessageRequester,
    connectivity: ConnectivityRequester,
    config: DhtConfig,
    shutdown_signal: Option<ShutdownSignal>,
    request_rx: Fuse<mpsc::Receiver<DhtRequest>>,
    msg_hash_cache: TtlCache<Vec<u8>, ()>,
}

impl DhtActor {
    pub fn new(
        config: DhtConfig,
        conn: DbConnection,
        node_identity: Arc<NodeIdentity>,
        peer_manager: Arc<PeerManager>,
        connectivity: ConnectivityRequester,
        outbound_requester: OutboundMessageRequester,
        request_rx: mpsc::Receiver<DhtRequest>,
        shutdown_signal: ShutdownSignal,
    ) -> Self
    {
        Self {
            msg_hash_cache: TtlCache::new(config.msg_hash_cache_capacity),
            config,
            database: DhtDatabase::new(conn),
            outbound_requester,
            peer_manager,
            connectivity,
            node_identity,
            shutdown_signal: Some(shutdown_signal),
            request_rx: request_rx.fuse(),
        }
    }

    pub fn spawn(self) {
        task::spawn(async move {
            if let Err(err) = self.run().await {
                error!(target: LOG_TARGET, "DhtActor failed to start with error: {:?}", err);
            }
        });
    }

    async fn run(mut self) -> Result<(), DhtActorError> {
        let offline_ts = self
            .database
            .get_metadata_value::<DateTime<Utc>>(DhtMetadataKey::OfflineTimestamp)
            .await
            .ok()
            .flatten();
        info!(
            target: LOG_TARGET,
            "DhtActor started. {}",
            offline_ts
                .map(|dt| format!("Dht has been offline since '{}'", dt))
                .unwrap_or_else(String::new)
        );

        let mut pending_jobs = FuturesUnordered::new();

        let mut shutdown_signal = self
            .shutdown_signal
            .take()
            .expect("DhtActor initialized without shutdown_signal");

        loop {
            futures::select! {
                request = self.request_rx.select_next_some() => {
                    debug!(target: LOG_TARGET, "DhtActor received message: {}", request);
                    pending_jobs.push(self.request_handler(request));
                },

                result = pending_jobs.select_next_some() => {
                    if let Err(err) = result {
                        error!(target: LOG_TARGET, "Error when handling DHT request message. {}", err);
                    }
                },

                _ = shutdown_signal => {
                    info!(target: LOG_TARGET, "DhtActor is shutting down because it received a shutdown signal.");
                    self.mark_shutdown_time().await;
                    break Ok(());
                },
            }
        }
    }

    async fn mark_shutdown_time(&self) {
        if let Err(err) = self
            .database
            .set_metadata_value(DhtMetadataKey::OfflineTimestamp, Utc::now())
            .await
        {
            error!(target: LOG_TARGET, "Failed to mark offline time: {:?}", err);
        }
    }

    fn request_handler(&mut self, request: DhtRequest) -> BoxFuture<'static, Result<(), DhtActorError>> {
        use DhtRequest::*;
        match request {
            SendJoin => {
                let node_identity = Arc::clone(&self.node_identity);
                let outbound_requester = self.outbound_requester.clone();
                Box::pin(Self::broadcast_join(node_identity, outbound_requester))
            },
            MsgHashCacheInsert(hash, reply_tx) => {
                // No locks needed here. Downside is this isn't really async, however this should be
                // fine as it is very quick
                let already_exists = self
                    .msg_hash_cache
                    .insert(hash, (), self.config.msg_hash_cache_ttl)
                    .is_some();
                let result = reply_tx.send(already_exists).map_err(|_| DhtActorError::ReplyCanceled);
                Box::pin(future::ready(result))
            },
            SelectPeers(broadcast_strategy, reply_tx) => {
                let peer_manager = Arc::clone(&self.peer_manager);
                let node_identity = Arc::clone(&self.node_identity);
                let connectivity = self.connectivity.clone();
                let config = self.config.clone();
                Box::pin(async move {
                    match Self::select_peers(config, node_identity, peer_manager, connectivity, broadcast_strategy)
                        .await
                    {
                        Ok(peers) => reply_tx.send(peers).map_err(|_| DhtActorError::ReplyCanceled),
                        Err(err) => {
                            error!(target: LOG_TARGET, "Peer selection failed: {:?}", err);
                            reply_tx.send(Vec::new()).map_err(|_| DhtActorError::ReplyCanceled)
                        },
                    }
                })
            },
            GetMetadata(key, reply_tx) => {
                let db = self.database.clone();
                Box::pin(async move {
                    let _ = reply_tx.send(db.get_metadata_value_bytes(key).await.map_err(Into::into));
                    Ok(())
                })
            },
            SetMetadata(key, value) => {
                let db = self.database.clone();
                Box::pin(async move {
                    match db.set_metadata_value_bytes(key, value).await {
                        Ok(_) => {
                            info!(target: LOG_TARGET, "Dht setting '{}' set", key);
                        },
                        Err(err) => {
                            error!(target: LOG_TARGET, "set_setting failed because {:?}", err);
                        },
                    }
                    Ok(())
                })
            },
        }
    }

    async fn broadcast_join(
        node_identity: Arc<NodeIdentity>,
        mut outbound_requester: OutboundMessageRequester,
    ) -> Result<(), DhtActorError>
    {
        let message = JoinMessage::from(&node_identity);

        debug!(target: LOG_TARGET, "Sending Join message to  closest peers",);

        outbound_requester
            .send_message_no_header(
                SendMessageParams::new()
                    .broadcast(vec![])
                    .with_dht_message_type(DhtMessageType::Join)
                    .force_origin()
                    .finish(),
                message,
            )
            .await
            .map_err(|err| DhtActorError::SendFailed(format!("Failed to send join message: {}", err)))?;

        Ok(())
    }

    async fn select_peers(
        config: DhtConfig,
        node_identity: Arc<NodeIdentity>,
        peer_manager: Arc<PeerManager>,
        mut connectivity: ConnectivityRequester,
        broadcast_strategy: BroadcastStrategy,
    ) -> Result<Vec<NodeId>, DhtActorError>
    {
        use BroadcastStrategy::*;
        match broadcast_strategy {
            DirectNodeId(node_id) => {
                // Send to a particular peer matching the given node ID
                peer_manager
                    .direct_identity_node_id(&node_id)
                    .await
                    .map(|peer| peer.map(|p| vec![p.node_id]).unwrap_or_default())
                    .map_err(Into::into)
            },
            DirectPublicKey(public_key) => {
                // Send to a particular peer matching the given node ID
                peer_manager
                    .direct_identity_public_key(&public_key)
                    .await
                    .map(|peer| peer.map(|p| vec![p.node_id]).unwrap_or_default())
                    .map_err(Into::into)
            },
            Flood => {
                // Send to all known peers
                // TODO: This should never be needed, remove
                let peers = peer_manager.flood_peers().await?;
                Ok(peers.into_iter().map(|p| p.node_id).collect())
            },
            Closest(closest_request) => {
                Self::select_closest_peers_for_propagation(
                    &config,
                    &peer_manager,
                    &closest_request.node_id,
                    closest_request.n,
                    &closest_request.excluded_peers,
                    closest_request.peer_features,
                )
                .await
            },
            Random(n, excluded) => {
                // Send to a random set of peers of size n that are Communication Nodes
                Ok(peer_manager
                    .random_peers(n, &excluded)
                    .await?
                    .into_iter()
                    .map(|p| p.node_id)
                    .collect())
            },
            Broadcast(exclude) => {
                let connections = connectivity
                    .select_connections(ConnectivitySelection::random_nodes(
                        config.num_neighbouring_nodes,
                        exclude.clone(),
                    ))
                    .await?;

                let candidates = connections
                    .iter()
                    .map(|c| c.peer_node_id())
                    .cloned()
                    .collect::<Vec<_>>();

                if candidates.is_empty() {
                    warn!(
                        target: LOG_TARGET,
                        "Broadcast requested before any peer connections have been established"
                    );
                }
                info!(
                    target: LOG_TARGET,
                    "{} candidate(s) selected for broadcast",
                    candidates.len()
                );

                Ok(candidates)
            },
            Propagate(destination, exclude) => {
                let dest_node_id = destination
                    .node_id()
                    .map(Clone::clone)
                    .or_else(|| destination.public_key().and_then(|pk| NodeId::from_key(pk).ok()));

                let connections = match dest_node_id {
                    Some(node_id) => {
                        let dest_connection = connectivity.get_connection(node_id.clone()).await?;
                        // If the peer was added to the exclude list, we don't want to send directly to the peer.
                        // This handles an edge case for the the join message which has a destination to the peer that
                        // sent it.
                        let dest_connection = dest_connection.filter(|c| !exclude.contains(c.peer_node_id()));
                        match dest_connection {
                            Some(conn) => {
                                // We're connected to the destination, so send the message directly
                                vec![conn]
                            },
                            None => {
                                // Select connections closer to the destination
                                let mut connections = connectivity
                                    .select_connections(ConnectivitySelection::closest_to(
                                        node_identity.node_id().clone(),
                                        config.num_neighbouring_nodes,
                                        exclude.clone(),
                                    ))
                                    .await?;
                                // Exclude candidates that are further away from the destination than this node
                                // unless this node has not selected a big enough sample i.e. this node is not well
                                // connected
                                if connections.len() >= config.num_propagation_nodes() {
                                    let dist_from_dest = node_identity.node_id().distance(&node_id);
                                    let before_len = connections.len();
                                    connections = connections
                                        .into_iter()
                                        .filter(|conn| conn.peer_node_id().distance(&node_id) < dist_from_dest)
                                        .collect();

                                    debug!(
                                        target: LOG_TARGET,
                                        "Filtered out {} node(s) that are further away than this node.",
                                        before_len - connections.len()
                                    );
                                }

                                connections
                            },
                        }
                    },
                    None => {
                        connectivity
                            .select_connections(ConnectivitySelection::random_nodes(
                                config.num_neighbouring_nodes,
                                exclude.clone(),
                            ))
                            .await?
                    },
                };

                if connections.is_empty() {
                    warn!(
                        target: LOG_TARGET,
                        "Propagation requested before any peer connections have been established"
                    );
                }

                let candidates = connections
                    .iter()
                    .take(config.num_propagation_nodes())
                    .map(|c| c.peer_node_id())
                    .cloned()
                    .collect::<Vec<_>>();

                info!(
                    target: LOG_TARGET,
                    "{} candidate(s) selected for propagation to {}",
                    candidates.len(),
                    destination
                );

                Ok(candidates)
            },
        }
    }

    /// Selects at least `n` MESSAGE_PROPAGATION peers (assuming that many are known) that are closest to `node_id` as
    /// well as other peers which do not advertise the MESSAGE_PROPAGATION flag (unless excluded by some other means
    /// e.g. `excluded` list, filter_predicate etc. The filter_predicate is called on each peer excluding them from
    /// the final results if that returns false.
    ///
    /// This ensures that peers are selected which are able to propagate the message further while still allowing
    /// clients to propagate to non-propagation nodes if required (e.g. Discovery messages)
    async fn select_closest_peers_for_propagation(
        config: &DhtConfig,
        peer_manager: &PeerManager,
        node_id: &NodeId,
        n: usize,
        excluded_peers: &[NodeId],
        features: PeerFeatures,
    ) -> Result<Vec<NodeId>, DhtActorError>
    {
        // TODO: This query is expensive. We can probably cache a list of neighbouring peers which are online
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

                if !peer.features.contains(features) {
                    trace!(
                        target: LOG_TARGET,
                        "[{}] is does not have the required features {:?}",
                        peer.node_id,
                        features
                    );
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

                let is_excluded = excluded_peers.contains(&peer.node_id);
                if is_excluded {
                    trace!(target: LOG_TARGET, "[{}] is explicitly excluded", peer.node_id);
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
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        broadcast_strategy::BroadcastClosestRequest,
        test_utils::{make_client_identity, make_node_identity, make_peer_manager},
    };
    use chrono::{DateTime, Utc};
    use tari_comms::{
        peer_manager::PeerFeatures,
        test_utils::mocks::{create_connectivity_manager_mock, create_peer_connection_mock_pair},
    };
    use tari_shutdown::Shutdown;
    use tari_test_utils::random;

    async fn db_connection() -> DbConnection {
        let conn = DbConnection::connect_memory(random::string(8)).await.unwrap();
        conn.migrate().await.unwrap();
        conn
    }

    #[tokio_macros::test_basic]
    async fn send_join_request() {
        let node_identity = make_node_identity();
        let peer_manager = make_peer_manager();
        let (out_tx, mut out_rx) = mpsc::channel(1);
        let (connectivity_manager, mock) = create_connectivity_manager_mock();
        mock.spawn();
        let (actor_tx, actor_rx) = mpsc::channel(1);
        let mut requester = DhtRequester::new(actor_tx);
        let outbound_requester = OutboundMessageRequester::new(out_tx);
        let shutdown = Shutdown::new();
        let actor = DhtActor::new(
            Default::default(),
            db_connection().await,
            node_identity,
            peer_manager,
            connectivity_manager,
            outbound_requester,
            actor_rx,
            shutdown.to_signal(),
        );

        actor.spawn();

        requester.send_join().await.unwrap();
        let (params, _) = unwrap_oms_send_msg!(out_rx.next().await.unwrap());
        assert_eq!(params.dht_message_type, DhtMessageType::Join);
    }

    #[tokio_macros::test_basic]
    async fn insert_message_signature() {
        let node_identity = make_node_identity();
        let peer_manager = make_peer_manager();
        let (connectivity_manager, mock) = create_connectivity_manager_mock();
        mock.spawn();
        let (out_tx, _) = mpsc::channel(1);
        let (actor_tx, actor_rx) = mpsc::channel(1);
        let mut requester = DhtRequester::new(actor_tx);
        let outbound_requester = OutboundMessageRequester::new(out_tx);
        let shutdown = Shutdown::new();
        let actor = DhtActor::new(
            Default::default(),
            db_connection().await,
            node_identity,
            peer_manager,
            connectivity_manager,
            outbound_requester,
            actor_rx,
            shutdown.to_signal(),
        );

        actor.spawn();

        let signature = vec![1u8, 2, 3];
        let is_dup = requester.insert_message_hash(signature.clone()).await.unwrap();
        assert_eq!(is_dup, false);
        let is_dup = requester.insert_message_hash(signature).await.unwrap();
        assert_eq!(is_dup, true);
        let is_dup = requester.insert_message_hash(Vec::new()).await.unwrap();
        assert_eq!(is_dup, false);
    }

    #[tokio_macros::test_basic]
    async fn select_peers() {
        let node_identity = make_node_identity();
        let peer_manager = make_peer_manager();

        let client_node_identity = make_client_identity();
        peer_manager.add_peer(client_node_identity.to_peer()).await.unwrap();

        let (connectivity_manager, mock) = create_connectivity_manager_mock();
        let connectivity_manager_mock_state = mock.get_shared_state();
        mock.spawn();

        let (conn_in, _, conn_out, _) =
            create_peer_connection_mock_pair(1, client_node_identity.to_peer(), node_identity.to_peer()).await;
        connectivity_manager_mock_state
            .add_active_connection(node_identity.node_id().clone(), conn_in)
            .await;

        peer_manager.add_peer(make_node_identity().to_peer()).await.unwrap();

        let (out_tx, _) = mpsc::channel(1);
        let (actor_tx, actor_rx) = mpsc::channel(1);
        let mut requester = DhtRequester::new(actor_tx);
        let outbound_requester = OutboundMessageRequester::new(out_tx);
        let shutdown = Shutdown::new();
        let actor = DhtActor::new(
            Default::default(),
            db_connection().await,
            Arc::clone(&node_identity),
            peer_manager,
            connectivity_manager,
            outbound_requester,
            actor_rx,
            shutdown.to_signal(),
        );

        actor.spawn();

        let peers = requester
            .select_peers(BroadcastStrategy::Broadcast(Vec::new()))
            .await
            .unwrap();

        assert_eq!(peers.len(), 0);

        connectivity_manager_mock_state
            .set_selected_connections(vec![conn_out.clone()])
            .await;
        let peers = requester
            .select_peers(BroadcastStrategy::Broadcast(Vec::new()))
            .await
            .unwrap();
        assert_eq!(peers.len(), 1);

        let send_request = Box::new(BroadcastClosestRequest {
            n: 10,
            node_id: node_identity.node_id().clone(),
            peer_features: PeerFeatures::DHT_STORE_FORWARD,
            excluded_peers: vec![],
        });
        let peers = requester
            .select_peers(BroadcastStrategy::Closest(send_request))
            .await
            .unwrap();
        assert_eq!(peers.len(), 1);

        let peers = requester
            .select_peers(BroadcastStrategy::DirectNodeId(Box::new(
                client_node_identity.node_id().clone(),
            )))
            .await
            .unwrap();

        assert_eq!(peers.len(), 1);
    }

    #[tokio_macros::test_basic]
    async fn get_and_set_metadata() {
        let node_identity = make_node_identity();
        let peer_manager = make_peer_manager();
        let (out_tx, _out_rx) = mpsc::channel(1);
        let (actor_tx, actor_rx) = mpsc::channel(1);
        let (connectivity_manager, mock) = create_connectivity_manager_mock();
        mock.spawn();
        let mut requester = DhtRequester::new(actor_tx);
        let outbound_requester = OutboundMessageRequester::new(out_tx);
        let mut shutdown = Shutdown::new();
        let actor = DhtActor::new(
            Default::default(),
            db_connection().await,
            node_identity,
            peer_manager,
            connectivity_manager,
            outbound_requester,
            actor_rx,
            shutdown.to_signal(),
        );

        actor.spawn();

        assert!(requester
            .get_metadata::<DateTime<Utc>>(DhtMetadataKey::OfflineTimestamp)
            .await
            .unwrap()
            .is_none());
        let ts = Utc::now();
        requester
            .set_metadata(DhtMetadataKey::OfflineTimestamp, ts)
            .await
            .unwrap();

        let got_ts = requester
            .get_metadata::<DateTime<Utc>>(DhtMetadataKey::OfflineTimestamp)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got_ts, ts);

        // Check upsert
        let ts = Utc::now().checked_add_signed(chrono::Duration::seconds(123)).unwrap();
        requester
            .set_metadata(DhtMetadataKey::OfflineTimestamp, ts)
            .await
            .unwrap();

        let got_ts = requester
            .get_metadata::<DateTime<Utc>>(DhtMetadataKey::OfflineTimestamp)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got_ts, ts);

        shutdown.trigger().unwrap();
    }
}
