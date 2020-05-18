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

use super::connection_pool::ConnectionPool;
use crate::{peer_manager::NodeId, PeerConnection};
use rand::{rngs::OsRng, seq::SliceRandom};

#[derive(Debug, Clone)]
pub enum ConnectivitySelection {
    RandomNodes(usize, Vec<NodeId>),
    ClosestTo(Box<NodeId>, usize, Vec<NodeId>),
}

pub fn select_closest<'a>(pool: &'a ConnectionPool, node_id: &NodeId, exclude: &[NodeId]) -> Vec<&'a PeerConnection> {
    let mut nodes = pool.filter_connections(|conn| {
        conn.is_connected() && conn.peer_features().is_node() && !exclude.contains(conn.peer_node_id())
    });

    nodes.sort_by(|a, b| {
        let dist_a = a.peer_node_id().distance(node_id);
        let dist_b = b.peer_node_id().distance(node_id);
        dist_a.cmp(&dist_b)
    });

    nodes
}

pub fn select_random_nodes<'a>(pool: &'a ConnectionPool, n: usize, excluding: &[NodeId]) -> Vec<&'a PeerConnection> {
    let connections = pool.filter_connections(|conn| {
        conn.is_connected() && conn.peer_features().is_node() && !excluding.contains(conn.peer_node_id())
    });
    connections.choose_multiple(&mut OsRng, n).map(|c| *c).collect()
}
