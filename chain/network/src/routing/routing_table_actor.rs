use std::collections::{HashMap, HashSet};

use actix::dev::MessageResponse;
use actix::{Actor, Addr, Context, Handler, Message, System};
#[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
use tracing::error;
use tracing::{debug, trace, warn};

use crate::stats::metrics;
use near_performance_metrics_macros::perf;
use near_primitives::network::PeerId;

#[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
use crate::routing::ibf::{Ibf, IbfBox};
#[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
use crate::routing::ibf_peer_set::IbfPeerSet;
#[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
use crate::routing::ibf_set::IbfSet;
use crate::routing::routing::{Edge, EdgeType, Graph, SAVE_PEERS_MAX_TIME};
#[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
use crate::routing::routing::{SimpleEdge, ValidIBFLevel, MIN_IBF_LEVEL};
use crate::types::StopMsg;
#[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
use crate::types::{PartialSync, PeerMessage, RoutingState, RoutingSyncV2, RoutingVersion2};
#[cfg(feature = "delay_detector")]
use delay_detector::DelayDetector;
use near_primitives::utils::index_to_bytes;
use near_store::db::DBCol::{ColComponentEdges, ColLastComponentNonce, ColPeerComponent};
use near_store::{Store, StoreUpdate};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `Prune` enum is to specify how often should we prune edges.
#[derive(Debug, Eq, PartialEq)]
pub enum Prune {
    /// Prune once per hour - default.
    PruneOncePerHour,
    /// Prune right now - for testing purposes.
    PruneNow,
    /// Don't prune at all.
    Disable,
}

/// RoutingTableActor that maintains routing table information. We currently have only one
/// instance of this actor.
///
/// We store the following information
///   - list of all known edges
///   - helper data structure for exchanging routing table
///   - routing information (where a message should be send to reach given peer)
///  
/// We use store for following reasons:
///   - store removed edges to disk
///   - we currently don't store active edges to disk
pub struct RoutingTableActor {
    /// Data structure with all edges. It's guaranteed that `peer.0` < `peer.1`.
    pub edges_info: HashMap<(PeerId, PeerId), Edge>,
    /// Data structure used for exchanging routing tables.
    #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
    pub peer_ibf_set: IbfPeerSet,
    /// Current view of the network represented by undirected graph.
    /// Nodes are Peers and edges are active connections.
    pub raw_graph: Graph,
    /// Active PeerId that are part of the shortest path to each PeerId.
    pub peer_forwarding: Arc<HashMap<PeerId, Vec<PeerId>>>,
    /// Last time a peer was reachable through active edges.
    pub peer_last_time_reachable: HashMap<PeerId, Instant>,
    /// Everytime a group of peers becomes unreachable at the same time; We store edges belonging to
    /// them in components. We remove all of those edges from memory, and save them to database,
    /// If any of them become reachable again, we re-add whole component.
    ///
    /// To store components, we have following column in the DB.
    /// ColLastComponentNonce -> stores component_nonce: u64, which is the lowest nonce that
    ///                          hasn't been used yet. If new component gets created it will use
    ///                          this nonce.
    /// ColComponentEdges     -> Mapping from `component_nonce` to list of edges
    /// ColPeerComponent      -> Mapping from `peer_id` to last component nonce if there
    ///                          exists one it belongs to.
    store: Arc<Store>,
    /// First component nonce id that hasn't been used. Used for creating new components.
    pub next_available_component_nonce: u64,
    /// True if edges were changed and we need routing table recalculation.
    pub needs_routing_table_recalculation: bool,
}

impl RoutingTableActor {
    pub fn new(my_peer_id: PeerId, store: Arc<Store>) -> Self {
        let component_nonce = store
            .get_ser::<u64>(ColLastComponentNonce, &[])
            .unwrap_or(None)
            .map_or(0, |nonce| nonce + 1);
        Self {
            edges_info: Default::default(),
            #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
            peer_ibf_set: Default::default(),
            raw_graph: Graph::new(my_peer_id),
            peer_forwarding: Default::default(),
            peer_last_time_reachable: Default::default(),
            store,
            next_available_component_nonce: component_nonce,
            needs_routing_table_recalculation: false,
        }
    }

    pub fn remove_edges(&mut self, edges: &[Edge]) {
        for edge in edges.iter() {
            self.remove_edge(edge);
        }
    }

    pub fn remove_edge(&mut self, edge: &Edge) {
        #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
        self.peer_ibf_set.remove_edge(&edge.to_simple_edge());

        let key = (edge.peer0.clone(), edge.peer1.clone());
        if self.edges_info.remove(&key).is_some() {
            self.raw_graph.remove_edge(&edge.peer0, &edge.peer1);
            self.needs_routing_table_recalculation = true;
        }
    }

    /// `add_verified_edge` adds edges, for which we already that theirs signatures
    /// are valid (`signature0`, `signature`).
    fn add_verified_edge(&mut self, edge: Edge) -> bool {
        let key = edge.get_pair();
        if !self.is_edge_newer(&key, edge.nonce) {
            // We already have a newer information about this edge. Discard this information.
            false
        } else {
            self.needs_routing_table_recalculation = true;
            match edge.edge_type() {
                EdgeType::Added => {
                    self.raw_graph.add_edge(key.0.clone(), key.1.clone());
                }
                EdgeType::Removed => {
                    self.raw_graph.remove_edge(&key.0, &key.1);
                }
            }
            #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
            self.peer_ibf_set.add_edge(&edge.to_simple_edge());
            self.edges_info.insert(key, edge);
            true
        }
    }

    /// Add several edges to the current view of the network.
    /// These edges are assumed to have been verified at this point.
    /// Return list of edges added.
    ///
    /// Everytime we remove an edge we store all edges removed at given time to disk.
    /// If new edge comes comes that is adjacent to a peer that has been previously removed,
    /// we will try to re-add edges previously removed from disk.
    pub fn add_verified_edges_to_routing_table(&mut self, mut edges: Vec<Edge>) -> Vec<Edge> {
        let total = edges.len();
        edges.retain(|edge| {
            let key = edge.get_pair();

            self.fetch_edges_for_peer_from_disk(&key.0);
            self.fetch_edges_for_peer_from_disk(&key.1);

            self.add_verified_edge(edge.clone())
        });

        // Update metrics after edge update
        near_metrics::inc_counter_by(&metrics::EDGE_UPDATES, total as u64);
        near_metrics::set_gauge(&metrics::EDGE_ACTIVE, self.raw_graph.total_active_edges() as i64);

        edges
    }

    /// If peer_id is not in memory check if it is on disk in bring it back on memory.
    fn fetch_edges_for_peer_from_disk(&mut self, other_peer_id: &PeerId) {
        if other_peer_id == self.my_peer_id()
            || self.peer_last_time_reachable.contains_key(other_peer_id)
        {
            return;
        }

        let my_peer_id = self.my_peer_id().clone();

        // Get the "row" (a.k.a nonce) at which we've stored a given peer in the past (when we pruned it).
        if let Ok(component_nonce) = self.component_nonce_from_peer(other_peer_id.clone()) {
            let mut update = self.store.store_update();

            // Load all edges that were persisted in database in the cell - and add them to the current graph.
            if let Ok(edges) = self.get_and_remove_component_edges(component_nonce, &mut update) {
                for edge in edges {
                    for &peer_id in vec![&edge.peer0, &edge.peer1].iter() {
                        if peer_id == &my_peer_id
                            || self.peer_last_time_reachable.contains_key(peer_id)
                        {
                            continue;
                        }

                        // `edge = (peer_id, other_peer_id)` belongs to component that we loaded from database.
                        if let Ok(cur_nonce) = self.component_nonce_from_peer(peer_id.clone()) {
                            // If `peer_id` belongs to current component
                            if cur_nonce == component_nonce {
                                // Mark it as reachable and delete from database.
                                self.peer_last_time_reachable
                                    .insert(peer_id.clone(), Instant::now() - SAVE_PEERS_MAX_TIME);
                                update
                                    .delete(ColPeerComponent, Vec::from(peer_id.clone()).as_ref());
                            } else {
                                warn!("We expected `peer_id` to belong to component {}, but it belongs to {}",
                                       component_nonce, cur_nonce);
                            }
                        } else {
                            warn!("We expected `peer_id` to belong to a component {}, but it doesn't belong anywhere",
                                       component_nonce);
                        }
                    }
                    self.add_verified_edge(edge);
                }
            }

            if let Err(e) = update.commit() {
                warn!(target: "network", "Error removing network component from store. {:?}", e);
            }
        } else {
            self.peer_last_time_reachable.insert(other_peer_id.clone(), Instant::now());
        }
    }

    fn my_peer_id(&self) -> &PeerId {
        &self.raw_graph.my_peer_id()
    }

    /// Recalculate routing table and update list of reachable peers.
    /// If pruning is enabled we will remove unused edges and store them to disk.
    ///
    /// # Returns
    /// List of edges removed.
    pub fn recalculate_routing_table_and_maybe_prune_edges(
        &mut self,
        prune: Prune,
        prune_edges_not_reachable_for: Duration,
    ) -> Vec<Edge> {
        #[cfg(feature = "delay_detector")]
        let _d = DelayDetector::new("routing table update".into());
        let _routing_table_recalculation =
            near_metrics::start_timer(&metrics::ROUTING_TABLE_RECALCULATION_HISTOGRAM);

        trace!(target: "network", "Update routing table.");

        self.peer_forwarding = Arc::new(self.raw_graph.calculate_distance());

        let now = Instant::now();
        for peer in self.peer_forwarding.keys() {
            self.peer_last_time_reachable.insert(peer.clone(), now);
        }

        let edges_to_remove = if prune != Prune::Disable {
            self.prune_unreachable_edges_and_save_to_db(
                prune == Prune::PruneNow,
                prune_edges_not_reachable_for,
            )
        } else {
            Vec::new()
        };
        self.remove_edges(&edges_to_remove);

        near_metrics::inc_counter_by(&metrics::ROUTING_TABLE_RECALCULATIONS, 1);
        near_metrics::set_gauge(&metrics::PEER_REACHABLE, self.peer_forwarding.len() as i64);
        edges_to_remove
    }

    fn prune_unreachable_edges_and_save_to_db(
        &mut self,
        force_pruning: bool,
        prune_edges_not_reachable_for: Duration,
    ) -> Vec<Edge> {
        let now = Instant::now();
        let mut oldest_time = now;

        // We compute routing graph every one second; we mark every node that was reachable during that time.
        // All nodes not reachable for at last 1 hour(SAVE_PEERS_AFTER_TIME) will be moved to disk.
        let peers_to_remove = self
            .peer_last_time_reachable
            .iter()
            .filter_map(|(peer_id, last_time)| {
                oldest_time = std::cmp::min(oldest_time, *last_time);
                if now.duration_since(*last_time) >= prune_edges_not_reachable_for {
                    Some(peer_id.clone())
                } else {
                    None
                }
            })
            .collect::<HashSet<_>>();

        // Save nodes on disk and remove from memory only if elapsed time from oldest peer
        // is greater than `SAVE_PEERS_MAX_TIME`
        if !force_pruning && now.duration_since(oldest_time) < SAVE_PEERS_MAX_TIME {
            return Vec::new();
        }
        debug!(target: "network", "try_save_edges: We are going to remove {} peers", peers_to_remove.len());

        let current_component_nonce = self.next_available_component_nonce;
        self.next_available_component_nonce += 1;

        let mut update = self.store.store_update();
        // Stores next available nonce.
        let _ = update.set_ser(ColLastComponentNonce, &[], &self.next_available_component_nonce);

        // Sets mapping from `peer_id` to `component nonce` in DB. This is later used to find
        // component that the edge belonged to.
        for peer_id in peers_to_remove.iter() {
            let _ = update.set_ser(
                ColPeerComponent,
                Vec::from(peer_id.clone()).as_ref(),
                &current_component_nonce,
            );

            self.peer_last_time_reachable.remove(peer_id);
        }

        let component_nonce = index_to_bytes(current_component_nonce);
        let edges_to_remove = self
            .edges_info
            .iter()
            .filter_map(|(key, edge)| {
                if peers_to_remove.contains(&key.0) || peers_to_remove.contains(&key.1) {
                    Some(edge.clone())
                } else {
                    None
                }
            })
            .collect();

        let _ = update.set_ser(ColComponentEdges, component_nonce.as_ref(), &edges_to_remove);

        if let Err(e) = update.commit() {
            warn!(target: "network", "Error storing network component to store. {:?}", e);
        }
        edges_to_remove
    }

    /// Checks whenever given edge is newer than the one we already have.
    pub fn is_edge_newer(&self, key: &(PeerId, PeerId), nonce: u64) -> bool {
        self.edges_info.get(&key).map_or(0, |x| x.nonce) < nonce
    }

    pub fn get_edge(&self, peer0: PeerId, peer1: PeerId) -> Option<Edge> {
        let key = Edge::key(peer0, peer1);
        self.edges_info.get(&key).cloned()
    }

    #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
    pub fn convert_simple_edges_to_edges(&self, edges: Vec<SimpleEdge>) -> Vec<Edge> {
        edges.iter().filter_map(|k| self.edges_info.get(&k.key()).cloned()).collect()
    }

    /// Get edges stored in DB under `ColPeerComponent` column at `peer_id` key.
    fn component_nonce_from_peer(&mut self, peer_id: PeerId) -> Result<u64, ()> {
        match self.store.get_ser::<u64>(ColPeerComponent, Vec::from(peer_id).as_ref()) {
            Ok(Some(nonce)) => Ok(nonce),
            _ => Err(()),
        }
    }

    /// Get all edges that were stored at a given "row" (a.k.a. component_nonce) in the store (and also remove them).
    fn get_and_remove_component_edges(
        &mut self,
        component_nonce: u64,
        update: &mut StoreUpdate,
    ) -> Result<Vec<Edge>, ()> {
        let enc_nonce = index_to_bytes(component_nonce);

        let res = match self.store.get_ser::<Vec<Edge>>(ColComponentEdges, enc_nonce.as_ref()) {
            Ok(Some(edges)) => Ok(edges),
            _ => Err(()),
        };

        update.delete(ColComponentEdges, enc_nonce.as_ref());

        res
    }
}

impl Actor for RoutingTableActor {
    type Context = Context<Self>;

    fn started(&mut self, _ctx: &mut Self::Context) {}
}

#[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
impl RoutingTableActor {
    pub fn split_edges_for_peer(
        &self,
        peer_id: &PeerId,
        unknown_edges: &[u64],
    ) -> (Vec<SimpleEdge>, Vec<u64>) {
        self.peer_ibf_set.split_edges_for_peer(peer_id, unknown_edges)
    }
}

impl Handler<StopMsg> for RoutingTableActor {
    type Result = ();
    fn handle(&mut self, _: StopMsg, _ctx: &mut Self::Context) -> Self::Result {
        System::current().stop();
    }
}

// Messages for RoutingTableActor
#[derive(Debug)]
pub enum RoutingTableMessages {
    // Add verified edges to routing table actor and update stats.
    // Each edge contains signature of both peers.
    // We say that the edge is "verified" if and only if we checked that the `signature0` and
    // `signature1` is valid.
    AddVerifiedEdges {
        edges: Vec<Edge>,
    },
    // Remove edges for unit tests
    #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
    AdvRemoveEdges(Vec<Edge>),
    // Get RoutingTable for debugging purposes.
    RequestRoutingTable,
    // Add Peer and generate IbfSet.
    #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
    AddPeerIfMissing(PeerId, Option<u64>),
    // Remove Peer from IbfSet
    #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
    RemovePeer(PeerId),
    // Do new routing table exchange algorithm.
    #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
    ProcessIbfMessage {
        peer_id: PeerId,
        ibf_msg: RoutingVersion2,
    },
    // Start new routing table sync.
    #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
    StartRoutingTableSync {
        seed: u64,
    },
    // Request routing table update and maybe prune edges.
    RoutingTableUpdate {
        prune: Prune,
        prune_edges_not_reachable_for: Duration,
    },
}

impl Message for RoutingTableMessages {
    type Result = RoutingTableMessagesResponse;
}

#[derive(MessageResponse, Debug)]
pub enum RoutingTableMessagesResponse {
    #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
    AddPeerResponse {
        seed: u64,
    },
    Empty,
    #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
    ProcessIbfMessageResponse {
        ibf_msg: Option<RoutingVersion2>,
    },
    RequestRoutingTableResponse {
        edges_info: Vec<Edge>,
    },
    AddVerifiedEdgesResponse(Vec<Edge>),
    #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
    StartRoutingTableSyncResponse(PeerMessage),
    RoutingTableUpdateResponse {
        edges_to_remove: Vec<Edge>,
        /// Active PeerId that are part of the shortest path to each PeerId.
        peer_forwarding: Arc<HashMap<PeerId, Vec<PeerId>>>,
    },
}

#[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
impl RoutingTableActor {
    pub fn exchange_routing_tables_using_ibf(
        &self,
        peer_id: &PeerId,
        ibf_set: &IbfSet<SimpleEdge>,
        ibf_level: ValidIBFLevel,
        ibf_vec: &[IbfBox],
        seed: u64,
    ) -> (Vec<SimpleEdge>, Vec<u64>, u64) {
        let ibf = ibf_set.get_ibf(ibf_level);

        let mut new_ibf = Ibf::from_vec(ibf_vec.clone(), seed ^ (ibf_level.0 as u64));

        if !new_ibf.merge(&ibf.data, seed ^ (ibf_level.0 as u64)) {
            error!(target: "network", "exchange routing tables failed with peer {}", peer_id);
            return (Default::default(), Default::default(), 0);
        }

        let (edge_hashes, unknown_edges_count) = new_ibf.try_recover();
        let (known, unknown_edges) = self.split_edges_for_peer(&peer_id, &edge_hashes);

        (known, unknown_edges, unknown_edges_count)
    }
}

impl Handler<RoutingTableMessages> for RoutingTableActor {
    type Result = RoutingTableMessagesResponse;

    #[perf]
    fn handle(&mut self, msg: RoutingTableMessages, _ctx: &mut Self::Context) -> Self::Result {
        match msg {
            RoutingTableMessages::AddVerifiedEdges { edges } => {
                RoutingTableMessagesResponse::AddVerifiedEdgesResponse(
                    self.add_verified_edges_to_routing_table(edges),
                )
            }
            RoutingTableMessages::RoutingTableUpdate { prune, prune_edges_not_reachable_for } => {
                let edges_removed = if self.needs_routing_table_recalculation {
                    self.recalculate_routing_table_and_maybe_prune_edges(
                        prune,
                        prune_edges_not_reachable_for,
                    )
                } else {
                    Vec::new()
                };
                self.needs_routing_table_recalculation = false;
                RoutingTableMessagesResponse::RoutingTableUpdateResponse {
                    // PeerManager maintains list of local edges. We will notify `PeerManager`
                    // to remove those edges.
                    edges_to_remove: edges_removed
                        .iter()
                        .filter(|p| p.contains_peer(&self.my_peer_id()))
                        .cloned()
                        .collect(),
                    peer_forwarding: self.peer_forwarding.clone(),
                }
            }
            #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
            RoutingTableMessages::StartRoutingTableSync { seed } => {
                RoutingTableMessagesResponse::StartRoutingTableSyncResponse(
                    PeerMessage::RoutingTableSyncV2(RoutingSyncV2::Version2(RoutingVersion2 {
                        known_edges: self.edges_info.len() as u64,
                        seed,
                        edges: Default::default(),
                        routing_state: RoutingState::InitializeIbf,
                    })),
                )
            }
            #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
            RoutingTableMessages::AdvRemoveEdges(edges) => {
                for edge in edges.iter() {
                    self.remove_edge(edge);
                }
                RoutingTableMessagesResponse::Empty
            }
            RoutingTableMessages::RequestRoutingTable => {
                RoutingTableMessagesResponse::RequestRoutingTableResponse {
                    edges_info: self.edges_info.iter().map(|(_k, v)| v.clone()).collect(),
                }
            }
            #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
            RoutingTableMessages::AddPeerIfMissing(peer_id, ibf_set) => {
                let seed =
                    self.peer_ibf_set.add_peer(peer_id.clone(), ibf_set, &mut self.edges_info);
                RoutingTableMessagesResponse::AddPeerResponse { seed }
            }
            #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
            RoutingTableMessages::RemovePeer(peer_id) => {
                self.peer_ibf_set.remove_peer(&peer_id);
                RoutingTableMessagesResponse::Empty
            }
            #[cfg(feature = "protocol_feature_routing_exchange_algorithm")]
            RoutingTableMessages::ProcessIbfMessage { peer_id, ibf_msg } => {
                match ibf_msg.routing_state {
                    RoutingState::PartialSync(partial_sync) => {
                        if let Some(ibf_set) = self.peer_ibf_set.get(&peer_id) {
                            let seed = ibf_msg.seed;
                            let (edges_for_peer, unknown_edge_hashes, unknown_edges_count) = self
                                .exchange_routing_tables_using_ibf(
                                    &peer_id,
                                    ibf_set,
                                    partial_sync.ibf_level,
                                    &partial_sync.ibf,
                                    ibf_msg.seed,
                                );

                            let edges_for_peer = edges_for_peer
                                .iter()
                                .filter_map(|x| self.edges_info.get(&x.key()).cloned())
                                .collect();
                            // Prepare message
                            let ibf_msg = if unknown_edges_count == 0
                                && unknown_edge_hashes.len() > 0
                            {
                                RoutingVersion2 {
                                    known_edges: self.edges_info.len() as u64,
                                    seed,
                                    edges: edges_for_peer,
                                    routing_state: RoutingState::RequestMissingEdges(
                                        unknown_edge_hashes,
                                    ),
                                }
                            } else if unknown_edges_count == 0 && unknown_edge_hashes.len() == 0 {
                                RoutingVersion2 {
                                    known_edges: self.edges_info.len() as u64,
                                    seed,
                                    edges: edges_for_peer,
                                    routing_state: RoutingState::Done,
                                }
                            } else {
                                if let Some(new_ibf_level) = partial_sync.ibf_level.inc() {
                                    let ibf_vec = ibf_set.get_ibf_vec(new_ibf_level);
                                    RoutingVersion2 {
                                        known_edges: self.edges_info.len() as u64,
                                        seed,
                                        edges: edges_for_peer,
                                        routing_state: RoutingState::PartialSync(PartialSync {
                                            ibf_level: new_ibf_level,
                                            ibf: ibf_vec,
                                        }),
                                    }
                                } else {
                                    RoutingVersion2 {
                                        known_edges: self.edges_info.len() as u64,
                                        seed,
                                        edges: self
                                            .edges_info
                                            .iter()
                                            .map(|x| x.1.clone())
                                            .collect(),
                                        routing_state: RoutingState::RequestAllEdges,
                                    }
                                }
                            };
                            RoutingTableMessagesResponse::ProcessIbfMessageResponse {
                                ibf_msg: Some(ibf_msg),
                            }
                        } else {
                            error!(target: "network", "Peer not found {}", peer_id);
                            RoutingTableMessagesResponse::Empty
                        }
                    }
                    RoutingState::InitializeIbf => {
                        self.peer_ibf_set.add_peer(
                            peer_id.clone(),
                            Some(ibf_msg.seed),
                            &mut self.edges_info,
                        );
                        if let Some(ibf_set) = self.peer_ibf_set.get(&peer_id) {
                            let seed = ibf_set.get_seed();
                            let ibf_vec = ibf_set.get_ibf_vec(MIN_IBF_LEVEL);
                            RoutingTableMessagesResponse::ProcessIbfMessageResponse {
                                ibf_msg: Some(RoutingVersion2 {
                                    known_edges: self.edges_info.len() as u64,
                                    seed,
                                    edges: Default::default(),
                                    routing_state: RoutingState::PartialSync(PartialSync {
                                        ibf_level: MIN_IBF_LEVEL,
                                        ibf: ibf_vec,
                                    }),
                                }),
                            }
                        } else {
                            error!(target: "network", "Peer not found {}", peer_id);
                            RoutingTableMessagesResponse::Empty
                        }
                    }
                    RoutingState::RequestMissingEdges(requested_edges) => {
                        let seed = ibf_msg.seed;
                        let (edges_for_peer, _) =
                            self.split_edges_for_peer(&peer_id, &requested_edges);

                        let edges_for_peer = edges_for_peer
                            .iter()
                            .filter_map(|x| self.edges_info.get(&x.key()).cloned())
                            .collect();

                        let ibf_msg = RoutingVersion2 {
                            known_edges: self.edges_info.len() as u64,
                            seed,
                            edges: edges_for_peer,
                            routing_state: RoutingState::Done,
                        };
                        RoutingTableMessagesResponse::ProcessIbfMessageResponse {
                            ibf_msg: Some(ibf_msg),
                        }
                    }
                    RoutingState::RequestAllEdges => {
                        RoutingTableMessagesResponse::ProcessIbfMessageResponse {
                            ibf_msg: Some(RoutingVersion2 {
                                known_edges: self.edges_info.len() as u64,
                                seed: ibf_msg.seed,
                                edges: self.edges_info.iter().map(|x| x.1.clone()).collect(),
                                routing_state: RoutingState::Done,
                            }),
                        }
                    }
                    RoutingState::Done => {
                        RoutingTableMessagesResponse::ProcessIbfMessageResponse { ibf_msg: None }
                    }
                }
            }
        }
    }
}

pub fn start_routing_table_actor(peer_id: PeerId, store: Arc<Store>) -> Addr<RoutingTableActor> {
    RoutingTableActor::new(peer_id, store).start()
}
