use crate::events::HotShotEvent;
use async_compatibility_layer::{
    art::async_timeout,
    async_primitives::subscribable_rwlock::{ReadView, SubscribableRwLock},
};
use async_lock::RwLock;
use bincode::config::Options;
use commit::{Commitment, Committable};
use hotshot_task::{
    event_stream::{ChannelStream, EventStream},
    global_registry::GlobalRegistry,
    task::{HotShotTaskCompleted, TS},
    task_impls::HSTWithEvent,
};
use hotshot_types::{
    block_impl::{VIDBlockPayload, VIDTransaction},
    certificate::QuorumCertificate,
    consensus::Consensus,
    data::{Leaf, VidDisperse, VidScheme, VidSchemeTrait},
    message::{Message, Proposal, SequencingMessage},
    traits::{
        consensus_api::ConsensusApi,
        election::{ConsensusExchange, QuorumExchangeType},
        node_implementation::{NodeImplementation, NodeType, QuorumEx},
        BlockPayload,
    },
};
use hotshot_utils::bincode::bincode_opts;
use snafu::Snafu;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Instant,
};
use tracing::{debug, error, instrument, warn};

/// A type alias for `HashMap<Commitment<T>, T>`
type CommitmentMap<T> = HashMap<Commitment<T>, T>;

#[derive(Snafu, Debug)]
/// Error type for consensus tasks
pub struct ConsensusTaskError {}

/// Tracks state of a Transaction task
pub struct TransactionTaskState<
    TYPES: NodeType,
    I: NodeImplementation<TYPES, Leaf = Leaf<TYPES>, ConsensusMessage = SequencingMessage<TYPES, I>>,
    A: ConsensusApi<TYPES, Leaf<TYPES>, I> + 'static,
> where
    QuorumEx<TYPES, I>: ConsensusExchange<
        TYPES,
        Message<TYPES, I>,
        Certificate = QuorumCertificate<TYPES, Commitment<I::Leaf>>,
    >,
{
    /// The state's api
    pub api: A,
    /// Global registry task for the state
    pub registry: GlobalRegistry,

    /// View number this view is executing in.
    pub cur_view: TYPES::Time,

    /// Reference to consensus. Leader will require a read lock on this.
    pub consensus: Arc<RwLock<Consensus<TYPES, Leaf<TYPES>>>>,

    /// A list of undecided transactions
    pub transactions: Arc<SubscribableRwLock<CommitmentMap<TYPES::Transaction>>>,

    /// A list of transactions we've seen decided, but didn't receive
    pub seen_transactions: HashSet<Commitment<TYPES::Transaction>>,

    /// the committee exchange
    pub quorum_exchange: Arc<QuorumEx<TYPES, I>>,

    /// Global events stream to publish events
    pub event_stream: ChannelStream<HotShotEvent<TYPES, I>>,

    /// This state's ID
    pub id: u64,
}

// We have two `TransactionTaskState` implementations with different bounds. The implementation
// here requires `TYPES: NodeType<Transaction = VIDTransaction, BlockPayload = VIDBlockPayload>`,
// whereas it's just `TYPES: NodeType` in the second implementation.
impl<
        TYPES: NodeType<Transaction = VIDTransaction, BlockPayload = VIDBlockPayload>,
        I: NodeImplementation<
            TYPES,
            Leaf = Leaf<TYPES>,
            ConsensusMessage = SequencingMessage<TYPES, I>,
        >,
        A: ConsensusApi<TYPES, Leaf<TYPES>, I> + 'static,
    > TransactionTaskState<TYPES, I, A>
where
    QuorumEx<TYPES, I>: ConsensusExchange<
        TYPES,
        Message<TYPES, I>,
        Certificate = QuorumCertificate<TYPES, Commitment<I::Leaf>>,
    >,
{
    /// main task event handler
    #[instrument(skip_all, fields(id = self.id, view = *self.cur_view), name = "Transaction Handling Task", level = "error")]

    pub async fn handle_event(
        &mut self,
        event: HotShotEvent<TYPES, I>,
    ) -> Option<HotShotTaskCompleted> {
        match event {
            HotShotEvent::TransactionsRecv(transactions) => {
                let consensus = self.consensus.read().await;
                self.transactions
                    .modify(|txns| {
                        for transaction in transactions {
                            let size = bincode_opts().serialized_size(&transaction).unwrap_or(0);

                            // If we didn't already know about this transaction, update our mempool metrics.
                            if !self.seen_transactions.remove(&transaction.commit())
                                && txns.insert(transaction.commit(), transaction).is_none()
                            {
                                consensus.metrics.outstanding_transactions.update(1);
                                consensus
                                    .metrics
                                    .outstanding_transactions_memory_size
                                    .update(i64::try_from(size).unwrap_or_else(|e| {
                                        warn!("Conversion failed: {e}. Using the max value.");
                                        i64::MAX
                                    }));
                            }
                        }
                    })
                    .await;

                return None;
            }
            HotShotEvent::LeafDecided(leaf_chain) => {
                let mut included_txns = HashSet::new();
                let mut included_txn_size = 0;
                let mut included_txn_count = 0;
                for leaf in leaf_chain {
                    if let Some(payload) = leaf.block_payload {
                        for txn in payload.transaction_commitments() {
                            included_txns.insert(txn);
                        }
                    }
                }
                let consensus = self.consensus.read().await;
                let txns = self.transactions.cloned().await;

                let _ = included_txns.iter().map(|hash| {
                    if !txns.contains_key(hash) {
                        self.seen_transactions.insert(*hash);
                    }
                });
                drop(txns);
                self.transactions
                    .modify(|txns| {
                        *txns = txns
                            .drain()
                            .filter(|(txn_hash, txn)| {
                                if included_txns.contains(txn_hash) {
                                    included_txn_count += 1;
                                    included_txn_size +=
                                        bincode_opts().serialized_size(txn).unwrap_or_default();
                                    false
                                } else {
                                    true
                                }
                            })
                            .collect();
                    })
                    .await;

                consensus
                    .metrics
                    .outstanding_transactions
                    .update(-included_txn_count);
                consensus
                    .metrics
                    .outstanding_transactions_memory_size
                    .update(-(i64::try_from(included_txn_size).unwrap_or(i64::MAX)));
                return None;
            }
            HotShotEvent::ViewChange(view) => {
                if *self.cur_view >= *view {
                    return None;
                }

                if *view - *self.cur_view > 1 {
                    error!("View changed by more than 1 going to view {:?}", view);
                }
                self.cur_view = view;

                // If we are not the next leader (DA leader for this view) immediately exit
                if !self.quorum_exchange.is_leader(self.cur_view + 1) {
                    // panic!("We are not the DA leader for view {}", *self.cur_view + 1);
                    return None;
                }

                // ED Copy of parent_leaf() function from sequencing leader

                let consensus = self.consensus.read().await;
                let parent_view_number = &consensus.high_qc.view_number;

                let Some(parent_view) = consensus.state_map.get(parent_view_number) else {
                    error!(
                        "Couldn't find high QC parent in state map. Parent view {:?}",
                        parent_view_number
                    );
                    return None;
                };
                let Some(leaf) = parent_view.get_leaf_commitment() else {
                    error!(
                        ?parent_view_number,
                        ?parent_view,
                        "Parent of high QC points to a view without a proposal"
                    );
                    return None;
                };
                let Some(leaf) = consensus.saved_leaves.get(&leaf) else {
                    error!("Failed to find high QC parent.");
                    return None;
                };
                let parent_leaf = leaf.clone();

                drop(consensus);

                // TODO (Keyao) Determine whether to allow empty blocks.
                // <https://github.com/EspressoSystems/HotShot/issues/1822>
                let txns = self.wait_for_transactions(parent_leaf).await?;

                // TODO move all VID stuff to a new VID task
                // details here: https://github.com/EspressoSystems/HotShot/issues/1817#issuecomment-1747143528
                let num_storage_nodes = 8;
                debug!("Prepare VID shares for {} storage nodes", num_storage_nodes);

                // TODO Secure SRS for VID
                // https://github.com/EspressoSystems/HotShot/issues/1686
                let srs = hotshot_types::data::test_srs(num_storage_nodes);

                // TODO proper source for VID erasure code rate
                // https://github.com/EspressoSystems/HotShot/issues/1734
                let num_chunks = 8;

                let vid = VidScheme::new(num_chunks, num_storage_nodes, &srs).unwrap();

                // TODO Wasteful flattening of tx bytes to accommodate VID API
                // https://github.com/EspressoSystems/jellyfish/issues/375
                let mut txns_flatten = Vec::new();
                for txn in &txns {
                    txns_flatten.extend(txn.0.clone());
                }

                let vid_disperse = vid.disperse(&txns_flatten).unwrap();
                let block = VIDBlockPayload {
                    transactions: txns,
                    payload_commitment: vid_disperse.commit,
                };

                // TODO never clone a block
                // https://github.com/EspressoSystems/HotShot/issues/1858
                self.event_stream
                    .publish(HotShotEvent::BlockReady(block.clone(), view + 1))
                    .await;

                // TODO (Keyao) Determine and update where to publish VidDisperseSend.
                // <https://github.com/EspressoSystems/HotShot/issues/1817>
                debug!("publishing VID disperse for view {}", *view + 1);
                self.event_stream
                    .publish(HotShotEvent::VidDisperseSend(
                        Proposal {
                            data: VidDisperse {
                                view_number: view + 1,
                                payload_commitment: block.commit(),
                                shares: vid_disperse.shares,
                                common: vid_disperse.common,
                            },
                            // TODO (Keyao) This is also signed in DA task.
                            signature: self.quorum_exchange.sign_payload_commitment(block.commit()),
                        },
                        self.quorum_exchange.public_key().clone(),
                    ))
                    .await;
                return None;
            }
            HotShotEvent::Shutdown => {
                return Some(HotShotTaskCompleted::ShutDown);
            }
            _ => {}
        }
        None
    }
}

// We have two `TransactionTaskState` implementations with different bounds. The implementation
// above requires `TYPES: NodeType<Transaction = VIDTransaction, BlockPayload = VIDBlockPayload>`,
// whereas here it's just `TYPES: NodeType`.
impl<
        TYPES: NodeType,
        I: NodeImplementation<
            TYPES,
            Leaf = Leaf<TYPES>,
            ConsensusMessage = SequencingMessage<TYPES, I>,
        >,
        A: ConsensusApi<TYPES, Leaf<TYPES>, I> + 'static,
    > TransactionTaskState<TYPES, I, A>
where
    QuorumEx<TYPES, I>: ConsensusExchange<
        TYPES,
        Message<TYPES, I>,
        Certificate = QuorumCertificate<TYPES, Commitment<I::Leaf>>,
    >,
{
    #[instrument(skip_all, fields(id = self.id, view = *self.cur_view), name = "Transaction Handling Task", level = "error")]
    async fn wait_for_transactions(
        &self,
        _parent_leaf: Leaf<TYPES>,
    ) -> Option<Vec<TYPES::Transaction>> {
        let task_start_time = Instant::now();

        // TODO (Keyao) Investigate the use of transaction hash
        // <https://github.com/EspressoSystems/HotShot/issues/1811>
        // let parent_leaf = self.parent_leaf().await?;
        // let previous_used_txns = match parent_leaf.tarnsaction_commitments {
        //     Some(txns) => txns,
        //     None => HashSet::new(),
        // };

        let receiver = self.transactions.subscribe().await;

        loop {
            let all_txns = self.transactions.cloned().await;
            debug!("Size of transactions: {}", all_txns.len());
            // TODO (Keyao) Investigate the use of transaction hash
            // <https://github.com/EspressoSystems/HotShot/issues/1811>
            // let unclaimed_txns: Vec<_> = all_txns
            //     .iter()
            //     .filter(|(txn_hash, _txn)| !previous_used_txns.contains(txn_hash))
            //     .collect();
            let unclaimed_txns = all_txns;

            let time_past = task_start_time.elapsed();
            if unclaimed_txns.len() < self.api.min_transactions()
                && (time_past < self.api.propose_max_round_time())
            {
                let duration = self.api.propose_max_round_time() - time_past;
                let result = async_timeout(duration, receiver.recv()).await;
                match result {
                    Err(_) => {
                        // Fall through below to updating new block
                        error!(
                            "propose_max_round_time passed, sending transactions we have so far"
                        );
                    }
                    Ok(Err(e)) => {
                        // Something unprecedented is wrong, and `transactions` has been dropped
                        error!("Channel receiver error for SubscribableRwLock {:?}", e);
                        return None;
                    }
                    Ok(Ok(_)) => continue,
                }
            }
            break;
        }
        let all_txns = self.transactions.cloned().await;
        // TODO (Keyao) Investigate the use of transaction hash
        // <https://github.com/EspressoSystems/HotShot/issues/1811>
        let txns: Vec<TYPES::Transaction> = all_txns.values().cloned().collect();
        // let txns: Vec<TYPES::Transaction> = all_txns
        //     .iter()
        //     .filter_map(|(txn_hash, txn)| {
        //         if previous_used_txns.contains(txn_hash) {
        //             None
        //         } else {
        //             Some(txn.clone())
        //         }
        //     })
        //     .collect();
        Some(txns)
    }

    /// Event filter for the transaction task
    pub fn filter(event: &HotShotEvent<TYPES, I>) -> bool {
        matches!(
            event,
            HotShotEvent::TransactionsRecv(_)
                | HotShotEvent::LeafDecided(_)
                | HotShotEvent::Shutdown
                | HotShotEvent::ViewChange(_)
        )
    }
}

/// task state implementation for Transactions Task
impl<
        TYPES: NodeType,
        I: NodeImplementation<
            TYPES,
            Leaf = Leaf<TYPES>,
            ConsensusMessage = SequencingMessage<TYPES, I>,
        >,
        A: ConsensusApi<TYPES, Leaf<TYPES>, I> + 'static,
    > TS for TransactionTaskState<TYPES, I, A>
where
    QuorumEx<TYPES, I>: ConsensusExchange<
        TYPES,
        Message<TYPES, I>,
        Certificate = QuorumCertificate<TYPES, Commitment<I::Leaf>>,
    >,
{
}

/// Type alias for DA Task Types
pub type TransactionsTaskTypes<TYPES, I, A> = HSTWithEvent<
    ConsensusTaskError,
    HotShotEvent<TYPES, I>,
    ChannelStream<HotShotEvent<TYPES, I>>,
    TransactionTaskState<TYPES, I, A>,
>;
