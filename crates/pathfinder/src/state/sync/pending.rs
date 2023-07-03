use pathfinder_common::{Chain, StateUpdate};
use pathfinder_storage::Storage;
use starknet_gateway_types::reply::Block;

use crate::state::sync::SyncEvent;

/// Poll's the Sequencer's pending block and emits [pending events](SyncEvent::Pending)
/// until the pending block is no longer connected to our current head.
///
/// This disconnect is detected whenever
/// - `pending.parent_hash != head`, or
/// - `pending` is a fully formed block and not [PendingBlock](starknet_gateway_types::reply::MaybePendingBlock::Pending), or
/// - the state update parent root does not match head.
///
/// A full block or full state update can be returned from this function if it is encountered during polling.
pub async fn poll_pending(
    tx_event: tokio::sync::mpsc::Sender<SyncEvent>,
    sequencer: &impl starknet_gateway_client::GatewayApi,
    head: (
        pathfinder_common::BlockHash,
        pathfinder_common::StateCommitment,
    ),
    poll_interval: std::time::Duration,
    chain: Chain,
    storage: Storage,
) -> anyhow::Result<(Option<Block>, Option<StateUpdate>)> {
    use anyhow::Context;
    use pathfinder_common::BlockId;
    use std::sync::Arc;

    loop {
        use starknet_gateway_types::reply::MaybePendingBlock;

        let block = match sequencer
            .block(BlockId::Pending)
            .await
            .context("Download pending block")?
        {
            MaybePendingBlock::Block(block) if block.block_hash == head.0 => {
                // Sequencer `pending` may return the latest full block for quite some time, so ignore it.
                tracing::trace!(hash=%block.block_hash, "Found current head from pending mode");
                tokio::time::sleep(poll_interval).await;
                continue;
            }
            MaybePendingBlock::Block(block) => {
                tracing::trace!(hash=%block.block_hash, "Found full block, exiting pending mode.");
                return Ok((Some(block), None));
            }
            MaybePendingBlock::Pending(pending) if pending.parent_hash != head.0 => {
                tracing::trace!(
                    pending=%pending.parent_hash, head=%head.0,
                    "Pending block's parent hash does not match head, exiting pending mode"
                );
                return Ok((None, None));
            }
            MaybePendingBlock::Pending(pending) => pending,
        };

        // Add a timeout to the pending state update query.
        //
        // This is work-around for the gateway constantly 503/502 on this query because
        // it cannot calculate the state root on the fly quickly enough.
        //
        // Without this timeout, we can potentially sit here infinitely retrying this query internally.
        let state_update = match tokio::time::timeout(
            std::time::Duration::from_secs(3 * 60),
            sequencer.state_update(BlockId::Pending),
        )
        .await
        {
            Ok(gateway_result) => gateway_result,
            Err(_timeout) => {
                tracing::debug!("Pending state update query timed out, exiting pending mode.");
                return Ok((None, None));
            }
        }
        .context("Downloading pending state update")?;

        if state_update.block_hash != pathfinder_common::BlockHash::ZERO {
            tracing::trace!("Found full state update, exiting pending mode.");
            return Ok((None, Some(state_update)));
        } else if state_update.parent_state_commitment != head.1 {
            tracing::trace!(pending=%state_update.parent_state_commitment, head=%head.1, "Pending state update's old root does not match head, exiting pending mode.");
            return Ok((None, None));
        }

        // Download, process and emit all missing classes.
        super::l2::download_new_classes(
            &state_update,
            sequencer,
            &tx_event,
            chain,
            &block.starknet_version,
            storage.clone(),
        )
        .await
        .context("Handling newly declared classes for pending block")?;

        // Emit new block.
        tx_event
            .send(SyncEvent::Pending(Arc::new(block), Arc::new(state_update)))
            .await
            .context("Event channel closed")?;

        tokio::time::sleep(poll_interval).await;
    }
}

#[cfg(test)]
mod tests {
    use crate::state::sync::SyncEvent;

    use super::poll_pending;
    use assert_matches::assert_matches;
    use pathfinder_common::{
        felt, felt_bytes, BlockHash, BlockNumber, BlockTimestamp, Chain, GasPrice,
        SequencerAddress, StarknetVersion, StateCommitment, StateUpdate,
    };
    use pathfinder_storage::Storage;
    use starknet_gateway_client::MockGatewayApi;
    use starknet_gateway_types::reply::{Block, MaybePendingBlock, PendingBlock, Status};

    lazy_static::lazy_static!(
        pub static ref PARENT_HASH: BlockHash =  BlockHash(felt!("0x1234"));
        pub static ref PARENT_ROOT: StateCommitment = StateCommitment(felt_bytes!(b"parent root"));

        pub static ref NEXT_BLOCK: Block = Block{
            block_hash: BlockHash(felt!("0xabcd")),
            block_number: BlockNumber::new_or_panic(1),
            gas_price: None,
            parent_block_hash: *PARENT_HASH,
            sequencer_address: None,
            state_commitment: *PARENT_ROOT,
            status: Status::AcceptedOnL2,
            timestamp: BlockTimestamp::new_or_panic(10),
            transaction_receipts: Vec::new(),
            transactions: Vec::new(),
            starknet_version: StarknetVersion::default(),
        };

        pub static ref PENDING_UPDATE: StateUpdate = {
            StateUpdate::default().with_parent_state_commitment(*PARENT_ROOT)
        };

        pub static ref PENDING_BLOCK: PendingBlock = PendingBlock {
            gas_price: GasPrice(11),
            parent_hash: NEXT_BLOCK.parent_block_hash,
            sequencer_address: SequencerAddress(felt_bytes!(b"seqeunecer address")),
            status: Status::Pending,
            timestamp: BlockTimestamp::new_or_panic(20),
            transaction_receipts: Vec::new(),
            transactions: Vec::new(),
            starknet_version: StarknetVersion::default(),
        };
    );

    /// Arbitrary timeout for receiving emits on the tokio channel. Otherwise failing tests will
    /// need to timeout naturally which may be forever.
    const TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    #[tokio::test]
    async fn exits_on_full_block() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut sequencer = MockGatewayApi::new();

        // Give a pending state update and full block.
        sequencer
            .expect_block()
            .returning(move |_| Ok(MaybePendingBlock::Block(NEXT_BLOCK.clone())));
        sequencer
            .expect_state_update()
            .returning(move |_| Ok(PENDING_UPDATE.clone()));

        let jh = tokio::spawn(async move {
            poll_pending(
                tx,
                &sequencer,
                (*PARENT_HASH, *PARENT_ROOT),
                std::time::Duration::ZERO,
                Chain::Testnet,
                Storage::in_memory().unwrap(),
            )
            .await
        });

        let result = tokio::time::timeout(TEST_TIMEOUT, rx.recv())
            .await
            .expect("Channel should be dropped");
        assert_matches!(result, None);

        let (full_block, _) = jh.await.unwrap().unwrap();
        assert_eq!(full_block.unwrap(), *NEXT_BLOCK);
    }

    #[tokio::test]
    async fn exits_on_full_state_diff() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut sequencer = MockGatewayApi::new();

        // Construct some full diff
        let full_diff = PENDING_UPDATE
            .clone()
            .with_block_hash(NEXT_BLOCK.block_hash)
            .with_state_commitment(StateCommitment(felt!("0x12")));
        let full_diff_copy = full_diff.clone();

        sequencer
            .expect_block()
            .returning(move |_| Ok(MaybePendingBlock::Pending(PENDING_BLOCK.clone())));
        sequencer
            .expect_state_update()
            .returning(move |_| Ok(full_diff_copy.clone()));

        let jh = tokio::spawn(async move {
            poll_pending(
                tx,
                &sequencer,
                (*PARENT_HASH, *PARENT_ROOT),
                std::time::Duration::ZERO,
                Chain::Testnet,
                Storage::in_memory().unwrap(),
            )
            .await
        });

        let result = tokio::time::timeout(TEST_TIMEOUT, rx.recv())
            .await
            .expect("Channel should be dropped");
        assert_matches!(result, None);

        let (_, full_state_update) = jh.await.unwrap().unwrap();
        assert_eq!(full_state_update.unwrap(), full_diff);
    }

    #[tokio::test]
    async fn exits_on_block_discontinuity() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut sequencer = MockGatewayApi::new();

        let mut pending_block = PENDING_BLOCK.clone();
        pending_block.parent_hash = BlockHash(felt!("0xFFFFFF"));
        sequencer
            .expect_block()
            .returning(move |_| Ok(MaybePendingBlock::Pending(pending_block.clone())));
        sequencer
            .expect_state_update()
            .returning(move |_| Ok(PENDING_UPDATE.clone()));

        let jh = tokio::spawn(async move {
            poll_pending(
                tx,
                &sequencer,
                (*PARENT_HASH, *PARENT_ROOT),
                std::time::Duration::ZERO,
                Chain::Testnet,
                Storage::in_memory().unwrap(),
            )
            .await
        });

        let result = tokio::time::timeout(TEST_TIMEOUT, rx.recv())
            .await
            .expect("Channel should be dropped");
        assert_matches!(result, None);
        jh.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn exits_on_state_diff_discontinuity() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut sequencer = MockGatewayApi::new();

        sequencer
            .expect_block()
            .returning(move |_| Ok(MaybePendingBlock::Pending(PENDING_BLOCK.clone())));

        let disconnected_diff = PENDING_UPDATE
            .clone()
            .with_parent_state_commitment(StateCommitment(felt_bytes!(b"different old root")));
        sequencer
            .expect_state_update()
            .returning(move |_| Ok(disconnected_diff.clone()));

        let jh = tokio::spawn(async move {
            poll_pending(
                tx,
                &sequencer,
                (*PARENT_HASH, *PARENT_ROOT),
                std::time::Duration::ZERO,
                Chain::Testnet,
                Storage::in_memory().unwrap(),
            )
            .await
        });

        let result = tokio::time::timeout(TEST_TIMEOUT, rx.recv())
            .await
            .expect("Channel should be dropped");
        assert_matches!(result, None);
        jh.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn success() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let mut sequencer = MockGatewayApi::new();

        sequencer
            .expect_block()
            .returning(move |_| Ok(MaybePendingBlock::Pending(PENDING_BLOCK.clone())));
        sequencer
            .expect_state_update()
            .returning(move |_| Ok(PENDING_UPDATE.clone()));

        let _jh = tokio::spawn(async move {
            poll_pending(
                tx,
                &sequencer,
                (*PARENT_HASH, *PARENT_ROOT),
                std::time::Duration::ZERO,
                Chain::Testnet,
                Storage::in_memory().unwrap(),
            )
            .await
        });

        let result = tokio::time::timeout(TEST_TIMEOUT, rx.recv())
            .await
            .expect("Event should be emitted")
            .unwrap();

        assert_matches!(result, SyncEvent::Pending(block, diff) if *block == *PENDING_BLOCK && *diff == *PENDING_UPDATE);
    }
}
