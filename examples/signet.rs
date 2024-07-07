#![allow(unused)]

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;
use tokio::task;

use bdk_wallet::bitcoin::{
    constants::genesis_block, secp256k1::Secp256k1, Address, BlockHash, Network, ScriptBuf,
};
use bdk_wallet::chain::{
    keychain::KeychainTxOutIndex, local_chain::LocalChain, miniscript::Descriptor, FullTxOut,
    IndexedTxGraph,
};

use kyoto::chain::checkpoints::HeaderCheckpoint;
use kyoto::node::builder::NodeBuilder;

/* Sync bdk chain and txgraph structures */

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let secp = Secp256k1::new();

    let desc = "tr([7d94197e/86'/1'/0']tpubDCyQVJj8KzjiQsFjmb3KwECVXPvMwvAxxZGCP9XmWSopmjW3bCV3wD7TgxrUhiGSueDS1MU5X1Vb1YjYcp8jitXc5fXfdC1z68hDDEyKRNr/0/*)";
    let change_desc = "tr([7d94197e/86'/1'/0']tpubDCyQVJj8KzjiQsFjmb3KwECVXPvMwvAxxZGCP9XmWSopmjW3bCV3wD7TgxrUhiGSueDS1MU5X1Vb1YjYcp8jitXc5fXfdC1z68hDDEyKRNr/1/*)";
    let (descriptor, _) = Descriptor::parse_descriptor(&secp, &desc)?;
    let (change_descriptor, _) = Descriptor::parse_descriptor(&secp, &change_desc)?;

    let g = genesis_block(Network::Signet).block_hash();
    let (mut chain, _) = LocalChain::from_genesis_hash(g);

    let mut graph = IndexedTxGraph::new({
        let mut index = KeychainTxOutIndex::default();
        let _ = index.insert_descriptor(0usize, descriptor);
        let _ = index.insert_descriptor(1, change_descriptor);
        index
    });

    let mut spks_to_watch: HashSet<ScriptBuf> = HashSet::new();
    for keychain in 0usize..=1 {
        let (indexed_spks, _changeset) = graph.index.reveal_to_target(&keychain, 19).unwrap();
        let mut spks = indexed_spks.into_iter().map(|(_i, spk)| spk);
        spks_to_watch.extend(&mut spks);
    }

    let subscriber = tracing_subscriber::FmtSubscriber::new();
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let peers = vec![
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        IpAddr::V4(Ipv4Addr::new(170, 75, 163, 219)),
        IpAddr::V4(Ipv4Addr::new(23, 137, 57, 100)),
    ];

    let builder = NodeBuilder::new(Network::Signet);
    let (mut node, client) = builder
        .add_peers(peers.into_iter().map(|ip| (ip, None).into()).collect())
        .add_scripts(spks_to_watch)
        .anchor_checkpoint(HeaderCheckpoint::new(
            169_000,
            BlockHash::from_str(
                "000000ed6fe89c46140f55ff511c558bcbdb1239ba95474f38f619b3bb657d4a",
            )?,
        ))
        .num_required_peers(2)
        .build_node();

    // Start a sync `Request`
    let req = bdk_kyoto::Request::new(chain.tip(), &graph.index);
    let mut client = req.into_client(client);

    // Run the `Node`
    if !node.is_running() {
        task::spawn(async move { node.run().await });
    }

    // Sync and apply updates
    if let Some(update) = client.update().await {
        let bdk_kyoto::Update {
            cp,
            indexed_tx_graph,
        } = update;

        let _ = chain.apply_update(cp)?;
        let _ = graph.apply_changeset(indexed_tx_graph.initial_changeset());
    }

    // Shutdown
    client.shutdown().await?;

    let cp = chain.tip();
    let index = &graph.index;
    let outpoints = index.outpoints().clone();
    let unspent: Vec<FullTxOut<_>> = graph
        .graph()
        .filter_chain_unspents(&chain, cp.block_id(), outpoints)
        .map(|(_, txout)| txout)
        .collect();
    for utxo in unspent {
        let addr = Address::from_script(utxo.txout.script_pubkey.as_script(), Network::Signet)?;
        println!("Funded: {:?}", addr);
    }
    println!("Graph txs count: {}", graph.graph().full_txs().count());
    println!("Local tip: {} {}", cp.height(), cp.hash());
    println!(
        "Balance: {:#?}",
        graph.graph().balance(
            &chain,
            cp.block_id(),
            index.outpoints().iter().cloned(),
            |_, _| true
        )
    );
    println!(
        "Last revealed indices: {:#?}",
        index.last_revealed_indices()
    );

    Ok(())
}
