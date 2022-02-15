//! Client actor orchestrates Client and facilitates network connection.

use crate::near_client::client::Client;
use crate::near_client::info::{get_validator_epoch_stats, InfoHelper, ValidatorInfoHelper};
use crate::near_client::sync::{StateSync, StateSyncResult};
use crate::near_client::StatusResponse;
use actix::dev::SendError;
use actix::dev::ToEnvelope;
use actix::{Actor, Addr, Arbiter, AsyncContext, Context, Handler, Message};
use borsh::BorshSerialize;
use chrono::DateTime;
use log::{debug, error, info, trace, warn};
use near_chain::chain::{
    do_apply_chunks, ApplyStatePartsRequest, ApplyStatePartsResponse, BlockCatchUpRequest,
    BlockCatchUpResponse, StateSplitRequest, StateSplitResponse,
};
use near_chain::test_utils::format_hash;
use near_chain::types::{AcceptedBlock, ValidatorInfoIdentifier};
use near_chain::{
    byzantine_assert, near_chain_primitives, Block, BlockHeader, ChainGenesis, ChainStoreAccess,
    Provenance, RuntimeAdapter,
};
use near_chain_configs::ClientConfig;
use near_client_primitives::types::{
    Error, GetNetworkInfo, NetworkInfoResponse, ShardSyncDownload, ShardSyncStatus, Status,
    StatusError, StatusSyncInfo, SyncStatus,
};
use near_network::types::{
    NetworkClientMessages, NetworkClientResponses, NetworkInfo, NetworkRequests,
    PeerManagerAdapter, PeerManagerMessageRequest,
};
use near_network_primitives::types::ReasonForBan;
use near_performance_metrics;
use near_performance_metrics_macros::{perf, perf_with_debug};
use near_primitives::block_header::ApprovalType;
use near_primitives::epoch_manager::RngSeed;
use near_primitives::hash::CryptoHash;
use near_primitives::network::{AnnounceAccount, PeerId};
use near_primitives::syncing::StatePartKey;
use near_primitives::time::{Clock, Utc};
use near_primitives::types::BlockHeight;
use near_primitives::unwrap_or_return;
use near_primitives::utils::{from_timestamp, MaybeValidated};
use near_primitives::validator_signer::ValidatorSigner;
use near_primitives::version::PROTOCOL_VERSION;
use near_primitives::views::ValidatorInfo;
use near_store::db::DBCol::ColStateParts;
use near_telemetry::TelemetryActor;
use rand::seq::SliceRandom;
use rand::{thread_rng};
use std::collections::HashMap;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

/// Multiplier on `max_block_time` to wait until deciding that chain stalled.
const STATUS_WAIT_TIME_MULTIPLIER: u64 = 10;
/// Drop blocks whose height are beyond head + horizon if it is not in the current epoch.
const BLOCK_HORIZON: u64 = 500;
/// `max_block_production_time` times this multiplier is how long we wait before rebroadcasting
/// the current `head`
const HEAD_STALL_MULTIPLIER: u32 = 4;

pub struct ClientActor {
    client: Client,
    network_adapter: Arc<dyn PeerManagerAdapter>,
    network_info: NetworkInfo,
    /// Identity that represents this Client at the network level.
    /// It is used as part of the messages that identify this client.
    node_id: PeerId,
    /// Last time we announced our accounts as validators.
    last_validator_announce_time: Option<Instant>,
    /// Info helper.
    info_helper: InfoHelper,

    /// Last time handle_block_production method was called
    block_production_next_attempt: DateTime<Utc>,
    block_production_started: bool,
    doomslug_timer_next_attempt: DateTime<Utc>,
    chunk_request_retry_next_attempt: DateTime<Utc>,
    sync_started: bool,
    state_parts_task_scheduler: Box<dyn Fn(ApplyStatePartsRequest)>,
    state_split_scheduler: Box<dyn Fn(StateSplitRequest)>,
    state_parts_client_arbiter: Arbiter,

    #[cfg(feature = "sandbox")]
    fastforward_delta: Option<near_primitives::types::BlockHeightDelta>,
}

/// Blocks the program until given genesis time arrives.
fn wait_until_genesis(genesis_time: &DateTime<Utc>) {
    loop {
        // Get chrono::Duration::num_seconds() by deducting genesis_time from now.
        let duration = genesis_time.signed_duration_since(Clock::utc());
        let chrono_seconds = duration.num_seconds();
        // Check if number of seconds in chrono::Duration larger than zero.
        if chrono_seconds <= 0 {
            break;
        }
        info!(target: "near", "Waiting until genesis: {}d {}h {}m {}s", duration.num_days(),
              (duration.num_hours() % 24),
              (duration.num_minutes() % 60),
              (duration.num_seconds() % 60));
        let wait =
            std::cmp::min(Duration::from_secs(10), Duration::from_secs(chrono_seconds as u64));
        thread::sleep(wait);
    }
}

impl ClientActor {
    pub fn new(
        config: ClientConfig,
        chain_genesis: ChainGenesis,
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        node_id: PeerId,
        network_adapter: Arc<dyn PeerManagerAdapter>,
        validator_signer: Option<Arc<dyn ValidatorSigner>>,
        telemetry_actor: Addr<TelemetryActor>,
        enable_doomslug: bool,
        rng_seed: RngSeed,
        ctx: &Context<ClientActor>,
    ) -> Result<Self, Error> {
        let state_parts_arbiter = Arbiter::new();
        let self_addr = ctx.address();
        let sync_jobs_actor_addr = SyncJobsActor::start_in_arbiter(
            &state_parts_arbiter.handle(),
            move |ctx: &mut Context<SyncJobsActor>| -> SyncJobsActor {
                ctx.set_mailbox_capacity(SyncJobsActor::MAILBOX_CAPACITY);
                SyncJobsActor { client_addr: self_addr }
            },
        );
        wait_until_genesis(&chain_genesis.time);
        if let Some(vs) = &validator_signer {
            info!(target: "client", "Starting validator node: {}", vs.validator_id());
        }
        let info_helper = InfoHelper::new(telemetry_actor, &config, validator_signer.clone());
        let client = Client::new(
            config,
            chain_genesis,
            runtime_adapter,
            network_adapter.clone(),
            validator_signer,
            enable_doomslug,
            rng_seed,
        )?;

        let now = Utc::now();
        Ok(ClientActor {
            client,
            network_adapter,
            node_id,
            network_info: NetworkInfo {
                connected_peers: vec![],
                num_connected_peers: 0,
                peer_max_count: 0,
                highest_height_peers: vec![],
                received_bytes_per_sec: 0,
                sent_bytes_per_sec: 0,
                known_producers: vec![],
                peer_counter: 0,
            },
            last_validator_announce_time: None,
            info_helper,
            block_production_next_attempt: now,
            block_production_started: false,
            doomslug_timer_next_attempt: now,
            chunk_request_retry_next_attempt: now,
            sync_started: false,
            state_parts_task_scheduler: create_sync_job_scheduler::<ApplyStatePartsRequest>(
                sync_jobs_actor_addr.clone(),
            ),
            state_split_scheduler: create_sync_job_scheduler::<StateSplitRequest>(
                sync_jobs_actor_addr,
            ),
            state_parts_client_arbiter: state_parts_arbiter,

            #[cfg(feature = "sandbox")]
            fastforward_delta: None,
        })
    }
}

fn create_sync_job_scheduler<M>(address: Addr<SyncJobsActor>) -> Box<dyn Fn(M)>
where
    M: Message + Send + 'static,
    M::Result: Send,
    SyncJobsActor: Handler<M>,
    Context<SyncJobsActor>: ToEnvelope<SyncJobsActor, M>,
{
    Box::new(move |msg: M| {
        if let Err(err) = address.try_send(msg) {
            match err {
                SendError::Full(request) => {
                    address.do_send(request);
                }
                SendError::Closed(_) => {
                    error!("Can't send message to SyncJobsActor, mailbox is closed");
                }
            }
        }
    })
}

impl Actor for ClientActor {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // Start syncing job.
        self.start_sync(ctx);

        // Start block production tracking if have block producer info.
        if self.client.validator_signer.is_some() {
            self.block_production_started = true;
        }

        // Start periodic logging of current state of the client.
        self.log_summary(ctx);
    }
}

impl Handler<NetworkClientMessages> for ClientActor {
    type Result = NetworkClientResponses;

    #[perf_with_debug]
    fn handle(&mut self, msg: NetworkClientMessages, ctx: &mut Context<Self>) -> Self::Result {
        #[cfg(feature = "delay_detector")]
        let _d = delay_detector::DelayDetector::new(
            format!("NetworkClientMessage {}", msg.as_ref()).into(),
        );
        self.check_triggers(ctx);

        match msg {
            NetworkClientMessages::Transaction { transaction, is_forwarded, check_only } => {
                self.client.process_tx(transaction, is_forwarded, check_only)
            }
            NetworkClientMessages::Block(block, peer_id, was_requested) => {
                let blocks_at_height = self
                    .client
                    .chain
                    .mut_store()
                    .get_all_block_hashes_by_height(block.header().height());
                if was_requested || !blocks_at_height.is_ok() {
                    if let SyncStatus::StateSync(sync_hash, _) = &mut self.client.sync_status {
                        if let Ok(header) = self.client.chain.get_block_header(sync_hash) {
                            if block.hash() == header.prev_hash() {
                                if let Err(e) = self.client.chain.save_block(block.into()) {
                                    error!(target: "client", "Failed to save a block during state sync: {}", e);
                                }
                            } else if block.hash() == sync_hash {
                                // This is the immediate block after a state sync
                                // We can afford to delay requesting missing chunks for this one block
                                if let Err(e) = self.client.chain.save_orphan(block.into(), false) {
                                    error!(target: "client", "Received an invalid block during state sync: {}", e);
                                }
                            }
                            return NetworkClientResponses::NoResponse;
                        }
                    }
                    self.receive_block(block, peer_id, was_requested);
                    NetworkClientResponses::NoResponse
                } else {
                    match self
                        .client
                        .runtime_adapter
                        .get_epoch_id_from_prev_block(block.header().prev_hash())
                    {
                        Ok(epoch_id) => {
                            if let Some(hashes) = blocks_at_height.unwrap().get(&epoch_id) {
                                if !hashes.contains(block.header().hash()) {
                                    warn!(target: "client", "Rejecting unrequested block {}, height {}", block.header().hash(), block.header().height());
                                }
                            }
                        }
                        _ => {}
                    }
                    NetworkClientResponses::NoResponse
                }
            }
            NetworkClientMessages::BlockHeaders(headers, peer_id) => {
                if self.receive_headers(headers, peer_id) {
                    NetworkClientResponses::NoResponse
                } else {
                    warn!(target: "client", "Banning node for sending invalid block headers");
                    NetworkClientResponses::Ban { ban_reason: ReasonForBan::BadBlockHeader }
                }
            }
            NetworkClientMessages::BlockApproval(approval, peer_id) => {
                debug!(target: "client", "Receive approval {:?} from peer {:?}", approval, peer_id);
                self.client.collect_block_approval(&approval, ApprovalType::PeerApproval(peer_id));
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::StateResponse(state_response_info) => {
                let shard_id = state_response_info.shard_id();
                let hash = state_response_info.sync_hash();
                let state_response = state_response_info.take_state_response();

                trace!(target: "sync", "Received state response shard_id: {} sync_hash: {:?} part(id/size): {:?}",
                    shard_id,
                    hash,
                    state_response.part().as_ref().map(|(part_id, data)| (part_id, data.len()))
                );
                // Get the download that matches the shard_id and hash
                let download = {
                    let mut download: Option<&mut ShardSyncDownload> = None;

                    // ... It could be that the state was requested by the state sync
                    if let SyncStatus::StateSync(sync_hash, shards_to_download) =
                        &mut self.client.sync_status
                    {
                        if hash == *sync_hash {
                            if let Some(part_id) = state_response.part_id() {
                                self.client
                                    .state_sync
                                    .received_requested_part(part_id, shard_id, hash);
                            }

                            if let Some(shard_download) = shards_to_download.get_mut(&shard_id) {
                                assert!(
                                    download.is_none(),
                                    "Internal downloads set has duplicates"
                                );
                                download = Some(shard_download);
                            } else {
                                // This may happen because of sending too many StateRequests to different peers.
                                // For example, we received StateResponse after StateSync completion.
                            }
                        }
                    }

                    // ... Or one of the catchups
                    if let Some((_, shards_to_download, _)) =
                        self.client.catchup_state_syncs.get_mut(&hash)
                    {
                        if let Some(part_id) = state_response.part_id() {
                            self.client.state_sync.received_requested_part(part_id, shard_id, hash);
                        }

                        if let Some(shard_download) = shards_to_download.get_mut(&shard_id) {
                            assert!(download.is_none(), "Internal downloads set has duplicates");
                            download = Some(shard_download);
                        } else {
                            // This may happen because of sending too many StateRequests to different peers.
                            // For example, we received StateResponse after StateSync completion.
                        }
                    }
                    // We should not be requesting the same state twice.
                    download
                };

                if let Some(shard_sync_download) = download {
                    match shard_sync_download.status {
                        ShardSyncStatus::StateDownloadHeader => {
                            if let Some(header) = state_response.take_header() {
                                if !shard_sync_download.downloads[0].done {
                                    match self.client.chain.set_state_header(shard_id, hash, header)
                                    {
                                        Ok(()) => {
                                            shard_sync_download.downloads[0].done = true;
                                        }
                                        Err(err) => {
                                            error!(target: "sync", "State sync set_state_header error, shard = {}, hash = {}: {:?}", shard_id, hash, err);
                                            shard_sync_download.downloads[0].error = true;
                                        }
                                    }
                                }
                            } else {
                                // No header found.
                                // It may happen because requested node couldn't build state response.
                                if !shard_sync_download.downloads[0].done {
                                    info!(target: "sync", "state_response doesn't have header, should be re-requested, shard = {}, hash = {}", shard_id, hash);
                                    shard_sync_download.downloads[0].error = true;
                                }
                            }
                        }
                        ShardSyncStatus::StateDownloadParts => {
                            if let Some(part) = state_response.take_part() {
                                let num_parts = shard_sync_download.downloads.len() as u64;
                                let (part_id, data) = part;
                                if part_id >= num_parts {
                                    error!(target: "sync", "State sync received incorrect part_id # {:?} for hash {:?}, potential malicious peer", part_id, hash);
                                    return NetworkClientResponses::NoResponse;
                                }
                                if !shard_sync_download.downloads[part_id as usize].done {
                                    match self
                                        .client
                                        .chain
                                        .set_state_part(shard_id, hash, part_id, num_parts, &data)
                                    {
                                        Ok(()) => {
                                            shard_sync_download.downloads[part_id as usize].done =
                                                true;
                                        }
                                        Err(err) => {
                                            error!(target: "sync", "State sync set_state_part error, shard = {}, part = {}, hash = {}: {:?}", shard_id, part_id, hash, err);
                                            shard_sync_download.downloads[part_id as usize].error =
                                                true;
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                } else {
                    error!(target: "sync", "State sync received hash {} that we're not expecting, potential malicious peer", hash);
                }

                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::EpochSyncResponse(_peer_id, _response) => {
                // TODO #3488
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::EpochSyncFinalizationResponse(_peer_id, _response) => {
                // TODO #3488
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::PartialEncodedChunkRequest(part_request_msg, route_back) => {
                let _ = self.client.shards_mgr.process_partial_encoded_chunk_request(
                    part_request_msg,
                    route_back,
                    self.client.chain.mut_store(),
                );
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::PartialEncodedChunkResponse(response) => {
                if let Ok(accepted_blocks) =
                    self.client.process_partial_encoded_chunk_response(response)
                {
                    self.process_accepted_blocks(accepted_blocks);
                }
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::PartialEncodedChunk(partial_encoded_chunk) => {
                if let Ok(accepted_blocks) = self
                    .client
                    .process_partial_encoded_chunk(MaybeValidated::from(partial_encoded_chunk))
                {
                    self.process_accepted_blocks(accepted_blocks);
                }
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::PartialEncodedChunkForward(forward) => {
                match self.client.process_partial_encoded_chunk_forward(forward) {
                    Ok(accepted_blocks) => self.process_accepted_blocks(accepted_blocks),
                    // Unknown chunk is normal if we get parts before the header
                    Err(Error::Chunk(near_chunks::Error::UnknownChunk)) => (),
                    Err(err) => {
                        error!(target: "client", "Error processing forwarded chunk: {}", err)
                    }
                }
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::Challenge(challenge) => {
                match self.client.process_challenge(challenge) {
                    Ok(_) => {}
                    Err(err) => {
                        error!(target: "client", "Error processing challenge: {}", err);
                    }
                }
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::NetworkInfo(network_info) => {
                self.network_info = network_info;
                NetworkClientResponses::NoResponse
            }
        }
    }
}

impl Handler<Status> for ClientActor {
    type Result = Result<StatusResponse, StatusError>;

    #[perf]
    fn handle(&mut self, msg: Status, ctx: &mut Context<Self>) -> Self::Result {
        #[cfg(feature = "delay_detector")]
        let _d = delay_detector::DelayDetector::new("client status".to_string().into());
        self.check_triggers(ctx);

        let head = self.client.chain.head()?;
        let head_header = self.client.chain.get_block_header(&head.last_block_hash)?;
        let latest_block_time = head_header.raw_timestamp();
        let latest_state_root = (*head_header.prev_state_root()).into();
        if msg.is_health_check {
            let now = Utc::now();
            let block_timestamp = from_timestamp(latest_block_time);
            if now > block_timestamp {
                let elapsed = (now - block_timestamp).to_std().unwrap();
                if elapsed
                    > Duration::from_millis(
                        self.client.config.max_block_production_delay.as_millis() as u64
                            * STATUS_WAIT_TIME_MULTIPLIER,
                    )
                {
                    return Err(StatusError::NoNewBlocks { elapsed });
                }
            }

            if self.client.sync_status.is_syncing() {
                return Err(StatusError::NodeIsSyncing);
            }
        }
        let validators = self
            .client
            .runtime_adapter
            .get_epoch_block_producers_ordered(&head.epoch_id, &head.last_block_hash)?
            .into_iter()
            .map(|(validator_stake, is_slashed)| ValidatorInfo {
                account_id: validator_stake.take_account_id(),
                is_slashed,
            })
            .collect();

        let protocol_version =
            self.client.runtime_adapter.get_epoch_protocol_version(&head.epoch_id)?;

        let validator_account_id =
            self.client.validator_signer.as_ref().map(|vs| vs.validator_id()).cloned();

        let mut earliest_block_hash = None;
        let mut earliest_block_height = None;
        let mut earliest_block_time = None;
        if let Some(earliest_block_hash_value) = self.client.chain.get_earliest_block_hash()? {
            earliest_block_hash = Some(earliest_block_hash_value);
            if let Ok(earliest_block) =
                self.client.chain.get_block_header(&earliest_block_hash_value)
            {
                earliest_block_height = Some(earliest_block.height());
                earliest_block_time = Some(earliest_block.timestamp());
            }
        }
        Ok(StatusResponse {
            version: self.client.config.version.clone(),
            protocol_version,
            latest_protocol_version: PROTOCOL_VERSION,
            chain_id: self.client.config.chain_id.clone(),
            rpc_addr: self.client.config.rpc_addr.clone(),
            validators,
            sync_info: StatusSyncInfo {
                latest_block_hash: head.last_block_hash.into(),
                latest_block_height: head.height,
                latest_state_root,
                latest_block_time: from_timestamp(latest_block_time),
                syncing: self.client.sync_status.is_syncing(),
                earliest_block_hash,
                earliest_block_height,
                earliest_block_time,
            },
            validator_account_id,
        })
    }
}

impl Handler<GetNetworkInfo> for ClientActor {
    type Result = Result<NetworkInfoResponse, String>;

    #[perf]
    fn handle(&mut self, _msg: GetNetworkInfo, ctx: &mut Context<Self>) -> Self::Result {
        #[cfg(feature = "delay_detector")]
        let _d = delay_detector::DelayDetector::new("client get network info".into());
        self.check_triggers(ctx);

        Ok(NetworkInfoResponse {
            connected_peers: (self.network_info.connected_peers.iter())
                .map(|fpi| fpi.peer_info.clone())
                .collect(),
            num_connected_peers: self.network_info.num_connected_peers,
            peer_max_count: self.network_info.peer_max_count,
            sent_bytes_per_sec: self.network_info.sent_bytes_per_sec,
            received_bytes_per_sec: self.network_info.received_bytes_per_sec,
            known_producers: self.network_info.known_producers.clone(),
        })
    }
}

impl ClientActor {
    /// Check if client Account Id should be sent and send it.
    /// Account Id is sent when is not current a validator but are becoming a validator soon.
    fn check_send_announce_account(&mut self, prev_block_hash: CryptoHash) {
        // If no peers, there is no one to announce to.
        if self.network_info.num_connected_peers == 0 {
            debug!(target: "client", "No peers: skip account announce");
            return;
        }

        // First check that we currently have an AccountId
        let validator_signer = match self.client.validator_signer.as_ref() {
            None => return,
            Some(signer) => signer,
        };

        let now = Clock::instant();
        // Check that we haven't announced it too recently
        if let Some(last_validator_announce_time) = self.last_validator_announce_time {
            // Don't make announcement if have passed less than half of the time in which other peers
            // should remove our Account Id from their Routing Tables.
            if 2 * (now - last_validator_announce_time) < self.client.config.ttl_account_id_router {
                return;
            }
        }

        debug!(target: "client", "Check announce account for {}, last announce time {:?}", validator_signer.validator_id(), self.last_validator_announce_time);

        // Announce AccountId if client is becoming a validator soon.
        let next_epoch_id = unwrap_or_return!(self
            .client
            .runtime_adapter
            .get_next_epoch_id_from_prev_block(&prev_block_hash));

        // Check client is part of the futures validators
        if self.client.is_validator(&next_epoch_id, &prev_block_hash) {
            debug!(target: "client", "Sending announce account for {}", validator_signer.validator_id());
            self.last_validator_announce_time = Some(now);

            let signature = validator_signer.sign_account_announce(
                validator_signer.validator_id(),
                &self.node_id,
                &next_epoch_id,
            );
            self.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
                NetworkRequests::AnnounceAccount(AnnounceAccount {
                    account_id: validator_signer.validator_id().clone(),
                    peer_id: self.node_id.clone(),
                    epoch_id: next_epoch_id,
                    signature,
                }),
            ));
        }
    }

    /// Retrieves latest height, and checks if must produce next block.
    /// Otherwise wait for block arrival or suggest to skip after timeout.
    fn handle_block_production(&mut self) -> Result<(), Error> {
        // If syncing, don't try to produce blocks.
        if self.client.sync_status.is_syncing() {
            return Ok(());
        }

        let _ = self.client.check_and_update_doomslug_tip();

        let head = self.client.chain.head()?;
        let latest_known = self.client.chain.mut_store().get_latest_known()?;

        #[cfg(feature = "sandbox")]
        let latest_known = if let Some(delta_height) = self.fastforward_delta.take() {
            let new_latest_known = near_chain::types::LatestKnown {
                height: latest_known.height + delta_height,
                seen: near_primitives::utils::to_timestamp(Clock::utc()),
            };

            self.client.chain.mut_store().save_latest_known(new_latest_known.clone())?;
            self.client.sandbox_update_tip(new_latest_known.height)?;
            new_latest_known
        } else {
            latest_known
        };

        assert!(
            head.height <= latest_known.height,
            "Latest known height is invalid {} vs {}",
            head.height,
            latest_known.height
        );

        let epoch_id =
            self.client.runtime_adapter.get_epoch_id_from_prev_block(&head.last_block_hash)?;

        for height in
            latest_known.height + 1..=self.client.doomslug.get_largest_height_crossing_threshold()
        {
            let next_block_producer_account =
                self.client.runtime_adapter.get_block_producer(&epoch_id, height)?;

            if self.client.validator_signer.as_ref().map(|bp| bp.validator_id())
                == Some(&next_block_producer_account)
            {
                let num_chunks = self.client.shards_mgr.num_chunks_for_block(&head.last_block_hash);
                let have_all_chunks = head.height == 0
                    || num_chunks == self.client.runtime_adapter.num_shards(&epoch_id).unwrap();

                if self.client.doomslug.ready_to_produce_block(
                    Clock::instant(),
                    height,
                    have_all_chunks,
                ) {
                    if let Err(err) = self.produce_block(height) {
                        // If there is an error, report it and let it retry on the next loop step.
                        error!(target: "client", "Block production failed: {}", err);
                    }
                }
            }
        }

        Ok(())
    }

    fn check_triggers(&mut self, ctx: &mut Context<ClientActor>) -> Duration {
        // There is a bug in Actix library. While there are messages in mailbox, Actix
        // will prioritize processing messages until mailbox is empty. Execution of any other task
        // scheduled with run_later will be delayed.

        #[cfg(feature = "delay_detector")]
        let _d = delay_detector::DelayDetector::new("client triggers".into());

        let mut delay = Duration::from_secs(1);
        let now = Utc::now();

        if self.sync_started {
            self.doomslug_timer_next_attempt = self.run_timer(
                self.client.config.doosmslug_step_period,
                self.doomslug_timer_next_attempt,
                ctx,
                |act, ctx| act.try_doomslug_timer(ctx),
            );
            delay = core::cmp::min(
                delay,
                self.doomslug_timer_next_attempt
                    .signed_duration_since(now)
                    .to_std()
                    .unwrap_or(delay),
            )
        }
        if self.block_production_started {
            self.block_production_next_attempt = self.run_timer(
                self.client.config.block_production_tracking_delay,
                self.block_production_next_attempt,
                ctx,
                |act, _ctx| act.try_handle_block_production(),
            );

            let _ = self.client.check_head_progress_stalled(
                self.client.config.max_block_production_delay * HEAD_STALL_MULTIPLIER,
            );

            delay = core::cmp::min(
                delay,
                self.block_production_next_attempt
                    .signed_duration_since(now)
                    .to_std()
                    .unwrap_or(delay),
            )
        }
        self.chunk_request_retry_next_attempt = self.run_timer(
            self.client.config.chunk_request_retry_period,
            self.chunk_request_retry_next_attempt,
            ctx,
            |act, _ctx| {
                if let Ok(header_head) = act.client.chain.header_head() {
                    act.client.shards_mgr.resend_chunk_requests(&header_head)
                }
            },
        );
        core::cmp::min(
            delay,
            self.chunk_request_retry_next_attempt
                .signed_duration_since(now)
                .to_std()
                .unwrap_or(delay),
        )
    }

    fn try_handle_block_production(&mut self) {
        match self.handle_block_production() {
            Ok(()) => {}
            Err(err) => {
                error!(target: "client", "Handle block production failed: {:?}", err);
            }
        }
    }

    fn try_doomslug_timer(&mut self, _: &mut Context<ClientActor>) {
        let _ = self.client.check_and_update_doomslug_tip();
        let approvals = self.client.doomslug.process_timer(Clock::instant());

        // Important to save the largest approval target height before sending approvals, so
        // that if the node crashes in the meantime, we cannot get slashed on recovery
        let mut chain_store_update = self.client.chain.mut_store().store_update();
        chain_store_update
            .save_largest_target_height(self.client.doomslug.get_largest_target_height());

        match chain_store_update.commit() {
            Ok(_) => {
                let head = unwrap_or_return!(self.client.chain.head());
                if self.client.is_validator(&head.epoch_id, &head.last_block_hash)
                    || self.client.is_validator(&head.next_epoch_id, &head.last_block_hash)
                {
                    for approval in approvals {
                        if let Err(e) =
                            self.client.send_approval(&self.client.doomslug.get_tip().0, approval)
                        {
                            error!("Error while sending an approval {:?}", e);
                        }
                    }
                }
            }
            Err(e) => error!("Error while committing largest skipped height {:?}", e),
        };
    }

    /// Produce block if we are block producer for given `next_height` height.
    /// Can return error, should be called with `produce_block` to handle errors and reschedule.
    fn produce_block(&mut self, next_height: BlockHeight) -> Result<(), Error> {
        match self.client.produce_block(next_height) {
            Ok(Some(block)) => {
                let peer_id = self.node_id.clone();
                // We’ve produced the block so that counts as validated block.
                let block = MaybeValidated::from_validated(block);
                let res = self.process_block(block, Provenance::PRODUCED, &peer_id);
                match &res {
                    Ok(_) => Ok(()),
                    Err(e) => match e.kind() {
                        near_chain::ErrorKind::ChunksMissing(_) => {
                            // missing chunks were already handled in Client::process_block, we don't need to
                            // do anything here
                            Ok(())
                        }
                        _ => {
                            error!(target: "client", "Failed to process freshly produced block: {:?}", res);
                            byzantine_assert!(false);
                            res.map_err(|err| err.into())
                        }
                    },
                }
            }
            Ok(None) => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// Process all blocks that were accepted by calling other relevant services.
    fn process_accepted_blocks(&mut self, accepted_blocks: Vec<AcceptedBlock>) {
        for accepted_block in accepted_blocks {
            self.client.on_block_accepted(
                accepted_block.hash,
                accepted_block.status,
                accepted_block.provenance,
            );
            let block = self.client.chain.get_block(&accepted_block.hash).unwrap();
            let chunks_in_block = block.header().chunk_mask().iter().filter(|&&m| m).count();
            let gas_used = Block::compute_gas_used(block.chunks().iter(), block.header().height());

            let last_final_hash = *block.header().last_final_block();

            self.info_helper.block_processed(gas_used, chunks_in_block as u64);
            self.check_send_announce_account(last_final_hash);
        }
    }

    /// Process block and execute callbacks.
    fn process_block(
        &mut self,
        block: MaybeValidated<Block>,
        provenance: Provenance,
        peer_id: &PeerId,
    ) -> Result<(), near_chain::Error> {
        // If we produced the block, send it out before we apply the block.
        // If we didn't produce the block and didn't request it, do basic validation
        // before sending it out.
        if provenance == Provenance::PRODUCED {
            self.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
                NetworkRequests::Block { block: block.as_ref().into_inner().clone() },
            ));
            // If we produced it, we don’t need to validate it.  Mark the block
            // as valid.
            block.mark_as_valid();
        } else {
            let chain = &mut self.client.chain;
            let res = chain.process_block_header(block.header(), &mut |_| {});
            let res = res.and_then(|_| chain.validate_block(&block));
            match res {
                Ok(_) => {
                    let head = self.client.chain.head()?;
                    // do not broadcast blocks that are too far back.
                    if (head.height < block.header().height()
                        || &head.epoch_id == block.header().epoch_id())
                        && provenance == Provenance::NONE
                        && !self.client.sync_status.is_syncing()
                    {
                        self.client.rebroadcast_block(block.as_ref().into_inner());
                    }
                }
                Err(e) if e.is_bad_data() => {
                    self.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
                        NetworkRequests::BanPeer {
                            peer_id: peer_id.clone(),
                            ban_reason: ReasonForBan::BadBlockHeader,
                        },
                    ));
                    return Err(e);
                }
                Err(_) => {
                    // We are ignoring all other errors and proceeding with the
                    // block.  If it is an orphan (i.e. we haven’t processed its
                    // previous block) than we will get MissingBlock errors.  In
                    // those cases we shouldn’t reject the block instead passing
                    // it along.  Eventually, it’ll get saved as an orphan.
                }
            }
        }
        let (accepted_blocks, result) = self.client.process_block(block, provenance);
        self.process_accepted_blocks(accepted_blocks);
        result.map(|_| ())
    }

    /// Processes received block. Ban peer if the block header is invalid or the block is ill-formed.
    fn receive_block(&mut self, block: Block, peer_id: PeerId, was_requested: bool) {
        let hash = *block.hash();
        debug!(target: "client", "{:?} Received block {} <- {} at {} from {}, requested: {}", self.client.validator_signer.as_ref().map(|vs| vs.validator_id()), hash, block.header().prev_hash(), block.header().height(), peer_id, was_requested);
        let head = unwrap_or_return!(self.client.chain.head());
        let is_syncing = self.client.sync_status.is_syncing();
        if block.header().height() >= head.height + BLOCK_HORIZON && is_syncing && !was_requested {
            debug!(target: "client", "dropping block {} that is too far ahead. Block height {} current head height {}", block.hash(), block.header().height(), head.height);
            return;
        }
        let tail = unwrap_or_return!(self.client.chain.tail());
        if block.header().height() < tail {
            debug!(target: "client", "dropping block {} that is too far behind. Block height {} current tail height {}", block.hash(), block.header().height(), tail);
            return;
        }
        let prev_hash = *block.header().prev_hash();
        let provenance =
            if was_requested { near_chain::Provenance::SYNC } else { near_chain::Provenance::NONE };
        match self.process_block(block.into(), provenance, &peer_id) {
            Ok(_) => {}
            Err(ref err) if err.is_bad_data() => {
                warn!(target: "client", "receive bad block: {}", err);
            }
            Err(ref err) if err.is_error() => {
                if let near_chain::ErrorKind::DBNotFoundErr(msg) = err.kind() {
                    debug_assert!(!msg.starts_with("BLOCK HEIGHT"), "{:?}", err);
                }
                if self.client.sync_status.is_syncing() {
                    // While syncing, we may receive blocks that are older or from next epochs.
                    // This leads to Old Block or EpochOutOfBounds errors.
                    debug!(target: "client", "Error on receival of block: {}", err);
                } else {
                    error!(target: "client", "Error on receival of block: {}", err);
                }
            }
            Err(e) => match e.kind() {
                near_chain::ErrorKind::Orphan => {
                    if !self.client.chain.is_orphan(&prev_hash) {
                        self.request_block_by_hash(prev_hash, peer_id)
                    }
                }
                // missing chunks are already handled in self.client.process_block()
                // we don't need to do anything here
                near_chain::ErrorKind::ChunksMissing(_) => {}
                _ => {
                    debug!(target: "client", "Process block: block {} refused by chain: {:?}", hash, e.kind());
                }
            },
        }
    }

    fn receive_headers(&mut self, headers: Vec<BlockHeader>, peer_id: PeerId) -> bool {
        info!(target: "client", "Received {} block headers from {}", headers.len(), peer_id);
        if headers.len() == 0 {
            return true;
        }
        match self.client.sync_block_headers(headers) {
            Ok(_) => true,
            Err(err) => {
                if err.is_bad_data() {
                    error!(target: "client", "Error processing sync blocks: {}", err);
                    false
                } else {
                    debug!(target: "client", "Block headers refused by chain: {}", err);
                    true
                }
            }
        }
    }

    fn request_block_by_hash(&mut self, hash: CryptoHash, peer_id: PeerId) {
        match self.client.chain.block_exists(&hash) {
            Ok(false) => {
                self.network_adapter.do_send(PeerManagerMessageRequest::NetworkRequests(
                    NetworkRequests::BlockRequest { hash, peer_id },
                ));
            }
            Ok(true) => {
                debug!(target: "client", "send_block_request_to_peer: block {} already known", hash)
            }
            Err(e) => {
                error!(target: "client", "send_block_request_to_peer: failed to check block exists: {:?}", e)
            }
        }
    }

    /// Check whether need to (continue) sync.
    /// Also return higher height with known peers at that height.
    fn syncing_info(&self) -> Result<(bool, u64), near_chain::Error> {
        let head = self.client.chain.head()?;
        let mut is_syncing = self.client.sync_status.is_syncing();

        let full_peer_info = if let Some(full_peer_info) =
            self.network_info.highest_height_peers.choose(&mut thread_rng())
        {
            full_peer_info
        } else {
            if !self.client.config.skip_sync_wait {
                warn!(target: "client", "Sync: no peers available, disabling sync");
            }
            return Ok((false, 0));
        };

        if is_syncing {
            if full_peer_info.chain_info.height <= head.height {
                info!(target: "client", "Sync: synced at {} [{}], {}, highest height peer: {}",
                      head.height, format_hash(head.last_block_hash),
                      full_peer_info.peer_info.id, full_peer_info.chain_info.height
                );
                is_syncing = false;
            }
        } else {
            if full_peer_info.chain_info.height
                > head.height + self.client.config.sync_height_threshold
            {
                info!(
                    target: "client",
                    "Sync: height: {}, peer id/height: {}/{}, enabling sync",
                    head.height,
                    full_peer_info.peer_info.id,
                    full_peer_info.chain_info.height,
                );
                is_syncing = true;
            }
        }
        Ok((is_syncing, full_peer_info.chain_info.height))
    }

    fn needs_syncing(&self, needs_syncing: bool) -> bool {
        #[cfg(feature = "test_features")]
        {
            if self.adv.read().unwrap().adv_disable_header_sync {
                return false;
            }
        }

        needs_syncing
    }

    /// Starts syncing and then switches to either syncing or regular mode.
    fn start_sync(&mut self, ctx: &mut Context<ClientActor>) {
        // Wait for connections reach at least minimum peers unless skipping sync.
        if self.network_info.num_connected_peers < self.client.config.min_num_peers
            && !self.client.config.skip_sync_wait
        {
            near_performance_metrics::actix::run_later(
                ctx,
                self.client.config.sync_step_period,
                move |act, ctx| {
                    act.start_sync(ctx);
                },
            );
            return;
        }
        self.sync_started = true;

        // Start main sync loop.
        self.sync(ctx);
    }

    /// Select the block hash we are using to sync state. It will sync with the state before applying the
    /// content of such block.
    ///
    /// The selected block will always be the first block on a new epoch:
    /// https://github.com/nearprotocol/nearcore/issues/2021#issuecomment-583039862
    ///
    /// To prevent syncing from a fork, we move `state_fetch_horizon` steps backwards and use that epoch.
    /// Usually `state_fetch_horizon` is much less than the expected number of produced blocks on an epoch,
    /// so this is only relevant on epoch boundaries.
    fn find_sync_hash(&mut self) -> Result<CryptoHash, near_chain::Error> {
        let header_head = self.client.chain.header_head()?;
        let mut sync_hash = header_head.prev_block_hash;
        for _ in 0..self.client.config.state_fetch_horizon {
            sync_hash = *self.client.chain.get_block_header(&sync_hash)?.prev_hash();
        }
        let mut epoch_start_sync_hash =
            StateSync::get_epoch_start_sync_hash(&mut self.client.chain, &sync_hash)?;

        if &epoch_start_sync_hash == self.client.chain.genesis().hash() {
            // If we are within `state_fetch_horizon` blocks of the second epoch, the sync hash will
            // be the first block of the first epoch (or, the genesis block). Due to implementation
            // details of the state sync, we can't state sync to the genesis block, so redo the
            // search without going back `state_fetch_horizon` blocks.
            epoch_start_sync_hash = StateSync::get_epoch_start_sync_hash(
                &mut self.client.chain,
                &header_head.last_block_hash,
            )?;
            assert_ne!(&epoch_start_sync_hash, self.client.chain.genesis().hash());
        }
        Ok(epoch_start_sync_hash)
    }

    fn run_timer<F>(
        &mut self,
        duration: Duration,
        next_attempt: DateTime<Utc>,
        ctx: &mut Context<ClientActor>,
        f: F,
    ) -> DateTime<Utc>
    where
        F: FnOnce(&mut Self, &mut <Self as Actor>::Context) + 'static,
    {
        let now = Utc::now();
        if now < next_attempt {
            return next_attempt;
        }

        f(self, ctx);

        return now.checked_add_signed(chrono::Duration::from_std(duration).unwrap()).unwrap();
    }

    /// Main syncing job responsible for syncing client with other peers.
    /// Runs itself iff it was not ran as reaction for message with results of
    /// finishing state part job
    fn sync(&mut self, ctx: &mut Context<ClientActor>) {
        #[cfg(feature = "delay_detector")]
        let _d = delay_detector::DelayDetector::new("client sync".into());
        // Macro to schedule to call this function later if error occurred.
        macro_rules! unwrap_or_run_later (($obj: expr) => (match $obj {
            Ok(v) => v,
            Err(err) => {
                error!(target: "sync", "Sync: Unexpected error: {}", err);

                near_performance_metrics::actix::run_later(
                    ctx,
                    self.client.config.sync_step_period, move |act, ctx| {
                        act.sync(ctx);
                    }
                );
                return;
            }
        }));

        let mut wait_period = self.client.config.sync_step_period;

        let currently_syncing = self.client.sync_status.is_syncing();
        let (needs_syncing, highest_height) = unwrap_or_run_later!(self.syncing_info());

        if !self.needs_syncing(needs_syncing) {
            if currently_syncing {
                debug!(
                    target: "client",
                    "{:?} transitions to no sync",
                    self.client.validator_signer.as_ref().map(|vs| vs.validator_id()),
                );
                self.client.sync_status = SyncStatus::NoSync;

                // Initial transition out of "syncing" state.
                // Announce this client's account id if their epoch is coming up.
                let head = unwrap_or_run_later!(self.client.chain.head());
                self.check_send_announce_account(head.prev_block_hash);
            }
            wait_period = self.client.config.sync_check_period;
        } else {
            // Run each step of syncing separately.
            unwrap_or_run_later!(self.client.header_sync.run(
                &mut self.client.sync_status,
                &mut self.client.chain,
                highest_height,
                &self.network_info.highest_height_peers
            ));
            // Only body / state sync if header height is close to the latest.
            let header_head = unwrap_or_run_later!(self.client.chain.header_head());

            // Sync state if already running sync state or if block sync is too far.
            let sync_state = match self.client.sync_status {
                SyncStatus::StateSync(_, _) => true,
                _ if header_head.height
                    >= highest_height
                        .saturating_sub(self.client.config.block_header_fetch_horizon) =>
                {
                    unwrap_or_run_later!(self.client.block_sync.run(
                        &mut self.client.sync_status,
                        &mut self.client.chain,
                        highest_height,
                        &self.network_info.highest_height_peers
                    ))
                }
                _ => false,
            };
            if sync_state {
                let (sync_hash, mut new_shard_sync, just_enter_state_sync) =
                    match &self.client.sync_status {
                        SyncStatus::StateSync(sync_hash, shard_sync) => {
                            (*sync_hash, shard_sync.clone(), false)
                        }
                        _ => {
                            let sync_hash = unwrap_or_run_later!(self.find_sync_hash());
                            (sync_hash, HashMap::default(), true)
                        }
                    };

                let me = self.client.validator_signer.as_ref().map(|x| x.validator_id().clone());
                let block_header =
                    unwrap_or_run_later!(self.client.chain.get_block_header(&sync_hash));
                let prev_hash = *block_header.prev_hash();
                let epoch_id = self.client.chain.get_block_header(&sync_hash).unwrap().epoch_id();
                let shards_to_sync = (0..self.client.runtime_adapter.num_shards(epoch_id).unwrap())
                    .filter(|x| {
                        self.client.shards_mgr.cares_about_shard_this_or_next_epoch(
                            me.as_ref(),
                            &prev_hash,
                            *x,
                            true,
                        )
                    })
                    .collect();

                if !self.client.config.archive && just_enter_state_sync {
                    unwrap_or_run_later!(self.client.chain.reset_data_pre_state_sync(sync_hash));
                }

                match unwrap_or_run_later!(self.client.state_sync.run(
                    &me,
                    sync_hash,
                    &mut new_shard_sync,
                    &mut self.client.chain,
                    &self.client.runtime_adapter,
                    &self.network_info.highest_height_peers,
                    shards_to_sync,
                    &self.state_parts_task_scheduler,
                    &self.state_split_scheduler,
                )) {
                    StateSyncResult::Unchanged => (),
                    StateSyncResult::Changed(fetch_block) => {
                        self.client.sync_status = SyncStatus::StateSync(sync_hash, new_shard_sync);
                        if fetch_block {
                            if let Some(peer_info) =
                                self.network_info.highest_height_peers.choose(&mut thread_rng())
                            {
                                let id = peer_info.peer_info.id.clone();

                                if let Ok(header) = self.client.chain.get_block_header(&sync_hash) {
                                    for hash in
                                        vec![*header.prev_hash(), *header.hash()].into_iter()
                                    {
                                        self.request_block_by_hash(hash, id.clone());
                                    }
                                }
                            }
                        }
                    }
                    StateSyncResult::Completed => {
                        info!(target: "sync", "State sync: all shards are done");

                        let mut accepted_blocks = vec![];
                        let mut orphans_missing_chunks = vec![];
                        let mut blocks_missing_chunks = vec![];
                        let mut challenges = vec![];

                        unwrap_or_run_later!(self.client.chain.reset_heads_post_state_sync(
                            &me,
                            sync_hash,
                            &mut |accepted_block| {
                                accepted_blocks.push(accepted_block);
                            },
                            &mut |missing_chunks| { blocks_missing_chunks.push(missing_chunks) },
                            &mut |orphan_missing_chunks| {
                                orphans_missing_chunks.push(orphan_missing_chunks);
                            },
                            &mut |challenge| challenges.push(challenge)
                        ));

                        self.client.send_challenges(challenges);

                        self.process_accepted_blocks(accepted_blocks);

                        self.client
                            .request_missing_chunks(blocks_missing_chunks, orphans_missing_chunks);

                        self.client.sync_status =
                            SyncStatus::BodySync { current_height: 0, highest_height: 0 };
                    }
                }
            }
        }

        near_performance_metrics::actix::run_later(ctx, wait_period, move |act, ctx| {
            act.sync(ctx);
        });
    }

    /// Periodically log summary.
    fn log_summary(&self, ctx: &mut Context<Self>) {
        near_performance_metrics::actix::run_later(
            ctx,
            self.client.config.log_summary_period,
            move |act, ctx| {
                #[cfg(feature = "delay_detector")]
                let _d = delay_detector::DelayDetector::new("client log summary".into());
                let is_syncing = act.client.sync_status.is_syncing();
                let head = unwrap_or_return!(act.client.chain.head(), act.log_summary(ctx));
                let validator_info = if !is_syncing {
                    let validators = unwrap_or_return!(
                        act.client.runtime_adapter.get_epoch_block_producers_ordered(
                            &head.epoch_id,
                            &head.last_block_hash
                        ),
                        act.log_summary(ctx)
                    );
                    let num_validators = validators.len();
                    let account_id = act.client.validator_signer.as_ref().map(|x| x.validator_id());
                    let is_validator = if let Some(account_id) = account_id {
                        match act.client.runtime_adapter.get_validator_by_account_id(
                            &head.epoch_id,
                            &head.last_block_hash,
                            account_id,
                        ) {
                            Ok((_, is_slashed)) => !is_slashed,
                            Err(_) => false,
                        }
                    } else {
                        false
                    };
                    Some(ValidatorInfoHelper { is_validator, num_validators })
                } else {
                    None
                };

                let epoch_identifier = ValidatorInfoIdentifier::BlockHash(head.last_block_hash);
                let validator_epoch_stats = act
                    .client
                    .runtime_adapter
                    .get_validator_info(epoch_identifier)
                    .map(get_validator_epoch_stats)
                    .unwrap_or_default();
                act.info_helper.info(
                    act.client.chain.store().get_genesis_height(),
                    &head,
                    &act.client.sync_status,
                    &act.node_id,
                    &act.network_info,
                    validator_info,
                    validator_epoch_stats,
                    act.client
                        .runtime_adapter
                        .get_epoch_height_from_prev_block(&head.prev_block_hash)
                        .unwrap_or(0),
                    act.client
                        .runtime_adapter
                        .get_protocol_upgrade_block_height(head.last_block_hash)
                        .unwrap_or(None)
                        .unwrap_or(0),
                );

                act.log_summary(ctx);
            },
        );
    }
}

impl Drop for ClientActor {
    fn drop(&mut self) {
        self.state_parts_client_arbiter.stop();
    }
}

struct SyncJobsActor {
    client_addr: Addr<ClientActor>,
}

impl SyncJobsActor {
    const MAILBOX_CAPACITY: usize = 100;

    fn apply_parts(
        &mut self,
        msg: &ApplyStatePartsRequest,
    ) -> Result<(), near_chain_primitives::error::Error> {
        let store = msg.runtime.get_store();

        for part_id in 0..msg.num_parts {
            let key = StatePartKey(msg.sync_hash, msg.shard_id, part_id).try_to_vec()?;
            let part = store.get(ColStateParts, &key)?.unwrap();

            msg.runtime.apply_state_part(
                msg.shard_id,
                &msg.state_root,
                part_id,
                msg.num_parts,
                &part,
                &msg.epoch_id,
            )?;
        }

        Ok(())
    }
}

impl Actor for SyncJobsActor {
    type Context = Context<Self>;
}

impl Handler<ApplyStatePartsRequest> for SyncJobsActor {
    type Result = ();

    fn handle(&mut self, msg: ApplyStatePartsRequest, _: &mut Self::Context) -> Self::Result {
        let result = self.apply_parts(&msg);

        self.client_addr.do_send(ApplyStatePartsResponse {
            apply_result: result,
            shard_id: msg.shard_id,
            sync_hash: msg.sync_hash,
        });
    }
}

impl Handler<ApplyStatePartsResponse> for ClientActor {
    type Result = ();

    fn handle(&mut self, msg: ApplyStatePartsResponse, _: &mut Self::Context) -> Self::Result {
        if let Some((sync, _, _)) = self.client.catchup_state_syncs.get_mut(&msg.sync_hash) {
            // We are doing catchup
            sync.set_apply_result(msg.shard_id, msg.apply_result);
        } else {
            self.client.state_sync.set_apply_result(msg.shard_id, msg.apply_result);
        }
    }
}

impl Handler<BlockCatchUpRequest> for SyncJobsActor {
    type Result = ();

    fn handle(&mut self, msg: BlockCatchUpRequest, _: &mut Self::Context) -> Self::Result {
        let results = do_apply_chunks(msg.work);

        self.client_addr.do_send(BlockCatchUpResponse {
            sync_hash: msg.sync_hash,
            block_hash: msg.block_hash,
            results,
        });
    }
}

impl Handler<BlockCatchUpResponse> for ClientActor {
    type Result = ();

    fn handle(&mut self, msg: BlockCatchUpResponse, _: &mut Self::Context) -> Self::Result {
        if let Some((_, _, blocks_catch_up_state)) =
            self.client.catchup_state_syncs.get_mut(&msg.sync_hash)
        {
            let saved_store_update = blocks_catch_up_state
                .scheduled_blocks
                .remove(&msg.block_hash)
                .expect("block caught up, but is not in processing");
            blocks_catch_up_state
                .processed_blocks
                .insert(msg.block_hash, (saved_store_update, msg.results));
        } else {
            panic!("block catch up processing result from unknown sync hash");
        }
    }
}

impl Handler<StateSplitRequest> for SyncJobsActor {
    type Result = ();

    fn handle(&mut self, msg: StateSplitRequest, _: &mut Self::Context) -> Self::Result {
        let results = msg.runtime.build_state_for_split_shards(
            msg.shard_uid,
            &msg.state_root,
            &msg.next_epoch_shard_layout,
        );

        self.client_addr.do_send(StateSplitResponse {
            sync_hash: msg.sync_hash,
            shard_id: msg.shard_id,
            new_state_roots: results,
        });
    }
}

impl Handler<StateSplitResponse> for ClientActor {
    type Result = ();

    fn handle(&mut self, msg: StateSplitResponse, _: &mut Self::Context) -> Self::Result {
        if let Some((sync, _, _)) = self.client.catchup_state_syncs.get_mut(&msg.sync_hash) {
            // We are doing catchup
            sync.set_split_result(msg.shard_id, msg.new_state_roots);
        } else {
            self.client.state_sync.set_split_result(msg.shard_id, msg.new_state_roots);
        }
    }
}