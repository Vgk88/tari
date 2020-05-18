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

use super::error::ConnectivityError;
use crate::{connectivity::connection_pool::PeerConnectionState, peer_manager::NodeId, PeerConnection};
use futures::{
    channel::{mpsc, oneshot},
    SinkExt,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::broadcast;

#[derive(Debug)]
pub enum ConnectivityEvent {
    PeerDisconnected(NodeId),
    EligiblePeerDisconnected(NodeId),
    PeerConnected(PeerConnection),
    PeerConnectFailed(NodeId),
    EligiblePeerConnectFailed(NodeId),
    PeerBanned(NodeId),

    ConnectivityStateInitialized,
    ConnectivityStateReady(usize),
    ConnectivityStateDegraded(usize),
    ConnectivityStateOffline,
}

#[derive(Debug)]
pub enum ConnectivityRequest {
    AddEligiblePeers(Vec<NodeId>),
    SelectConnections(
        ConnectivitySelection,
        oneshot::Sender<Result<Vec<PeerConnection>, ConnectivityError>>,
    ),
    GetConnection(Box<NodeId>, oneshot::Sender<Option<PeerConnection>>),
    GetAll(oneshot::Sender<Vec<PeerConnectionState>>),
    BanPeer(Box<NodeId>, Duration),
}

#[derive(Debug, Clone)]
pub enum ConnectivitySelection {
    RandomNodes(usize, Vec<NodeId>),
    ClosestTo(Box<NodeId>, usize),
}

impl ConnectivitySelection {
    pub fn random_nodes(n: usize, exclude: Vec<NodeId>) -> Self {
        ConnectivitySelection::RandomNodes(n, exclude)
    }

    pub fn closest_to(node_id: NodeId, n: usize) -> Self {
        ConnectivitySelection::ClosestTo(Box::new(node_id), n)
    }
}

#[derive(Clone)]
pub struct ConnectivityRequester {
    sender: mpsc::Sender<ConnectivityRequest>,
    event_tx: broadcast::Sender<Arc<ConnectivityEvent>>,
}

impl ConnectivityRequester {
    pub fn new(sender: mpsc::Sender<ConnectivityRequest>, event_tx: broadcast::Sender<Arc<ConnectivityEvent>>) -> Self {
        Self { sender, event_tx }
    }

    pub fn subscribe_event_stream(&self) -> broadcast::Receiver<Arc<ConnectivityEvent>> {
        self.event_tx.subscribe()
    }

    pub async fn add_eligible_peers(&mut self, peers: Vec<NodeId>) -> Result<(), ConnectivityError> {
        self.sender
            .send(ConnectivityRequest::AddEligiblePeers(peers))
            .await
            .map_err(|_| ConnectivityError::ActorDisconnected)?;
        Ok(())
    }

    pub async fn select_connections(
        &mut self,
        selection: ConnectivitySelection,
    ) -> Result<Vec<PeerConnection>, ConnectivityError>
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender
            .send(ConnectivityRequest::SelectConnections(selection, reply_tx))
            .await
            .map_err(|_| ConnectivityError::ActorDisconnected)?;
        reply_rx.await.map_err(|_| ConnectivityError::ActorResponseCancelled)?
    }

    pub async fn get_connection(&mut self, node_id: NodeId) -> Result<Option<PeerConnection>, ConnectivityError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender
            .send(ConnectivityRequest::GetConnection(Box::new(node_id), reply_tx))
            .await
            .map_err(|_| ConnectivityError::ActorDisconnected)?;
        reply_rx.await.map_err(|_| ConnectivityError::ActorResponseCancelled)
    }

    pub async fn get_all_connection_states(&mut self) -> Result<Vec<PeerConnectionState>, ConnectivityError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.sender
            .send(ConnectivityRequest::GetAll(reply_tx))
            .await
            .map_err(|_| ConnectivityError::ActorDisconnected)?;
        reply_rx.await.map_err(|_| ConnectivityError::ActorResponseCancelled)
    }

    pub async fn ban_peer(&mut self, node_id: NodeId, duration: Duration) -> Result<(), ConnectivityError> {
        self.sender
            .send(ConnectivityRequest::BanPeer(Box::new(node_id), duration))
            .await
            .map_err(|_| ConnectivityError::ActorDisconnected)?;
        Ok(())
    }
}
