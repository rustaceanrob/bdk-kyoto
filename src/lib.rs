#![doc = include_str!("../README.md")]
//! ## Examples
//!
//! If you have an existing project that leverages `bdk_wallet`, building the compact block filter
//! _node_ and _client_ is simple. You may construct and configure a node to integrate with your
//! wallet by using the [`BuilderExt`](crate::builder) and [`Builder`](crate::builder).
//!
//! ```no_run
//! # const RECEIVE: &str = "tr([7d94197e/86'/1'/0']tpubDCyQVJj8KzjiQsFjmb3KwECVXPvMwvAxxZGCP9XmWSopmjW3bCV3wD7TgxrUhiGSueDS1MU5X1Vb1YjYcp8jitXc5fXfdC1z68hDDEyKRNr/0/*)";
//! # const CHANGE: &str = "tr([7d94197e/86'/1'/0']tpubDCyQVJj8KzjiQsFjmb3KwECVXPvMwvAxxZGCP9XmWSopmjW3bCV3wD7TgxrUhiGSueDS1MU5X1Vb1YjYcp8jitXc5fXfdC1z68hDDEyKRNr/1/*)";
//! use bdk_wallet::Wallet;
//! use bdk_wallet::bitcoin::Network;
//! use bdk_kyoto::builder::{Builder, BuilderExt};
//! use bdk_kyoto::{LightClient, ScanType};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let mut wallet = Wallet::create(RECEIVE, CHANGE)
//!         .network(Network::Signet)
//!         .create_wallet_no_persist()?;
//!
//!     let LightClient {
//!         requester,
//!         info_subscriber: _,
//!         warning_subscriber: _,
//!         mut update_subscriber,
//!         node
//!     } = Builder::new(Network::Signet).build_with_wallet(&wallet, ScanType::Sync)?;
//!
//!     tokio::task::spawn(async move { node.run().await });
//!
//!     loop {
//!         let update = update_subscriber.update().await?;
//!         wallet.apply_update(update)?;
//!         return Ok(());
//!     }
//! }
//! ```

#![warn(missing_docs)]
use bdk_wallet::chain::BlockId;
use bdk_wallet::chain::CheckPoint;
pub use bdk_wallet::Update;

use bdk_wallet::chain::{keychain_txout::KeychainTxOutIndex, IndexedTxGraph};
use bdk_wallet::chain::{ConfirmationBlockTime, TxUpdate};
use bdk_wallet::KeychainKind;

pub extern crate bip157;

pub use bip157::builder::NodeDefault;
#[doc(inline)]
pub use bip157::{
    BlockHash, ClientError, FeeRate, HeaderCheckpoint, Info, RejectPayload, RejectReason,
    Requester, TrustedPeer, TxBroadcast, TxBroadcastPolicy, Txid, Warning,
};
use bip157::{Event, SyncUpdate};

#[doc(inline)]
pub use bip157::Receiver;
#[doc(inline)]
pub use bip157::UnboundedReceiver;

#[doc(inline)]
pub use builder::BuilderExt;
pub mod builder;

#[derive(Debug)]
/// A node and associated structs to send and receive events to and from the node.
pub struct LightClient {
    /// Send events to a running node (i.e. broadcast a transaction).
    pub requester: Requester,
    /// Receive informational messages as the node runs.
    pub info_subscriber: Receiver<Info>,
    /// Receive warnings from the node as it runs.
    pub warning_subscriber: UnboundedReceiver<Warning>,
    /// Receive wallet updates from a node.
    pub update_subscriber: UpdateSubscriber,
    /// The underlying node that must be run to fetch blocks from peers.
    pub node: NodeDefault,
}

/// Interpret events from a node that is running to apply
/// updates to an underlying wallet.
#[derive(Debug)]
pub struct UpdateSubscriber {
    // channel receiver
    receiver: UnboundedReceiver<Event>,
    // changes to local chain
    cp: CheckPoint,
    // receive graph
    graph: IndexedTxGraph<ConfirmationBlockTime, KeychainTxOutIndex<KeychainKind>>,
}

impl UpdateSubscriber {
    fn new(
        receiver: UnboundedReceiver<Event>,
        cp: CheckPoint,
        graph: IndexedTxGraph<ConfirmationBlockTime, KeychainTxOutIndex<KeychainKind>>,
    ) -> Self {
        Self {
            receiver,
            cp,
            graph,
        }
    }
    /// Return the most recent update from the node once it has synced to the network's tip.
    /// This may take a significant portion of time during wallet recoveries or dormant wallets.
    /// Note that you may call this method in a loop as long as the node is running.
    ///
    /// **Warning**
    ///
    /// This method is _not_ cancel safe. You cannot use it within a `tokio::select` arm.
    pub async fn update(&mut self) -> Result<Update, UpdateError> {
        let mut cp = self.cp.clone();
        while let Some(message) = self.receiver.recv().await {
            match message {
                Event::IndexedFilter(filter) => {
                    let _block_hash = filter.block_hash();
                }
                Event::BlocksDisconnected {
                    accepted,
                    disconnected: _,
                } => {
                    for header in accepted {
                        cp = cp.insert(BlockId {
                            height: header.height,
                            hash: header.block_hash(),
                        });
                    }
                }
                Event::FiltersSynced(SyncUpdate {
                    tip: _,
                    recent_history,
                }) => {
                    for (height, header) in recent_history {
                        cp = cp.insert(BlockId {
                            height,
                            hash: header.block_hash(),
                        });
                    }
                    self.cp = cp;
                    return Ok(self.get_scan_response());
                }
                _ => (),
            }
        }
        Err(UpdateError::NodeStopped)
    }

    // When the client is believed to have synced to the chain tip of most work,
    // we can return a wallet update.
    fn get_scan_response(&mut self) -> Update {
        let tx_update = TxUpdate::from(self.graph.graph().clone());
        let graph = core::mem::take(&mut self.graph);
        let last_active_indices = graph.index.last_used_indices();
        self.graph = IndexedTxGraph::new(graph.index);
        Update {
            tx_update,
            last_active_indices,
            chain: Some(self.cp.clone()),
        }
    }
}

/// Errors encountered when attempting to construct a wallet update.
#[derive(Debug, Clone, Copy)]
pub enum UpdateError {
    /// The node has stopped running.
    NodeStopped,
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateError::NodeStopped => write!(f, "the node halted execution."),
        }
    }
}

impl std::error::Error for UpdateError {}

/// How to scan compact block filters on start up.
#[derive(Debug, Clone, Copy, Default)]
pub enum ScanType {
    /// Sync the wallet from the last known wallet checkpoint to the rest of the network.
    #[default]
    Sync,
    /// Recover an old wallet by scanning after the specified height.
    Recovery {
        /// The height in the block chain to begin searching for transactions.
        checkpoint: HeaderCheckpoint,
    },
}
