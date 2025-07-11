use crate::error::ProgramResult;
use crate::error::RomeEvmError::Custom;
use crate::indexer::solana_client::{GetConfirmedSignaturesForAddress2ConfigClone, SolanaClient};
use crate::indexer::SolanaBlockStorage;
use solana_program::clock::Slot;
use solana_program::pubkey::Pubkey;
use solana_rpc_client_api::config::{RpcBlockConfig, RpcTransactionConfig};
use solana_sdk::commitment_config::{CommitmentConfig, CommitmentLevel};
use solana_sdk::signature::Signature;
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, EncodedTransactionWithStatusMeta,
    TransactionDetails, UiConfirmedBlock, UiTransactionEncoding,
};
use std::collections::{BTreeMap, HashSet};
use std::iter;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tokio::task::JoinHandle;

type ArcSolanaClient = Arc<dyn SolanaClient>;

pub struct SolanaBlockLoader {
    pub solana_block_storage: Arc<dyn SolanaBlockStorage>,
    pub client: ArcSolanaClient,
    pub commitment: CommitmentLevel,
    pub program_id: Pubkey,
    pub batch_size: Slot,
    pub block_retries: usize,
    pub tx_retries: usize,
    pub retry_int: Duration,
}

#[tracing::instrument(skip(client))]
async fn load_transaction(
    client: &ArcSolanaClient,
    program_id: Pubkey,
    signature: Signature,
    commitment: CommitmentLevel,
    tx_retries: usize,
    retry_delay: Duration,
) -> ProgramResult<Option<EncodedConfirmedTransactionWithStatusMeta>> {
    if let Some(tx) = client
        .get_transaction_with_config(
            &signature,
            RpcTransactionConfig {
                encoding: Some(UiTransactionEncoding::Base58),
                commitment: Some(CommitmentConfig { commitment }),
                max_supported_transaction_version: Some(0),
            },
            tx_retries,
            retry_delay,
        )
        .await?
    {
        if let Some(vt) = tx.transaction.transaction.decode() {
            let accounts = vt.message.static_account_keys();
            for instruction in vt.message.instructions() {
                if accounts[instruction.program_id_index as usize] == program_id {
                    return Ok(Some(tx));
                }
            }
        }
    }

    Ok(None)
}

async fn load_transactions(
    client: ArcSolanaClient,
    commitment: CommitmentLevel,
    program_id: Pubkey,
    signatures: Vec<Signature>,
    tx_retries: usize,
    retry_int: Duration,
) -> ProgramResult<Option<Vec<EncodedTransactionWithStatusMeta>>> {
    let futures = signatures.into_iter().map(|signature| {
        load_transaction(
            &client, program_id, signature, commitment, tx_retries, retry_int,
        )
    });

    let txs = futures_util::future::join_all(futures)
        .await
        .into_iter()
        .collect::<ProgramResult<Vec<Option<EncodedConfirmedTransactionWithStatusMeta>>>>()?
        .into_iter()
        .flatten()
        .map(|tx| tx.transaction)
        .collect::<Vec<_>>();

    if txs.is_empty() {
        Ok(None)
    } else {
        Ok(Some(txs))
    }
}

async fn load_block_transactions(
    client: ArcSolanaClient,
    program_id: Pubkey,
    mut block: UiConfirmedBlock,
    commitment: CommitmentLevel,
    tx_retries: usize,
    retry_int: Duration,
) -> ProgramResult<UiConfirmedBlock> {
    let before = block
        .signatures
        .as_ref()
        .map(|sigs| sigs.last())
        .and_then(|sig| sig.and_then(|sig| Signature::from_str(sig).ok()));

    let until = block
        .signatures
        .as_ref()
        .map(|sigs| sigs.first())
        .and_then(|sig| sig.and_then(|sig| Signature::from_str(sig).ok()));

    // not needed anymore
    block.signatures = None;

    if let (Some(before), Some(until)) = (before, until) {
        let tx_signatures = iter::once(before)
            .chain(
                client
                    .get_signatures_for_address_with_config(
                        &program_id,
                        GetConfirmedSignaturesForAddress2ConfigClone {
                            commitment: Some(CommitmentConfig { commitment }),
                            before: Some(before),
                            until: Some(until),
                            limit: None,
                        },
                    )
                    .await?
                    .into_iter()
                    .filter_map(|status_with_signature| {
                        Signature::from_str(&status_with_signature.signature).ok()
                    }),
            )
            .chain(iter::once(until))
            .rev()
            .collect();

        block.transactions = load_transactions(
            client,
            commitment,
            program_id,
            tx_signatures,
            tx_retries,
            retry_int,
        )
        .await?;
    }

    Ok(block)
}

async fn load_block(
    client: ArcSolanaClient,
    commitment: CommitmentLevel,
    program_id: Pubkey,
    slot_number: Slot,
    block_retries: usize,
    tx_retries: usize,
    retry_int: Duration,
) -> ProgramResult<Option<Arc<UiConfirmedBlock>>> {
    Ok(
        if let Some(block) = client
            .get_block_with_config(
                slot_number,
                RpcBlockConfig {
                    encoding: Some(UiTransactionEncoding::Base58),
                    transaction_details: Some(TransactionDetails::Signatures),
                    rewards: None,
                    commitment: Some(CommitmentConfig { commitment }),
                    max_supported_transaction_version: Some(0),
                },
                block_retries,
                retry_int,
            )
            .await?
        {
            Some(Arc::new(
                load_block_transactions(
                    client, program_id, block, commitment, tx_retries, retry_int,
                )
                .await?,
            ))
        } else {
            None
        },
    )
}

impl SolanaBlockLoader {
    async fn load_blocks(
        &self,
        mut slots: HashSet<Slot>,
    ) -> ProgramResult<BTreeMap<Slot, Arc<UiConfirmedBlock>>> {
        if slots.is_empty() {
            tracing::warn!("No slots to load");
            return Ok(BTreeMap::new());
        }

        let mut results = BTreeMap::new();
        let futures = slots.iter().map(|slot_number| {
            load_block(
                self.client.clone(),
                self.commitment,
                self.program_id,
                *slot_number,
                self.block_retries,
                self.tx_retries,
                self.retry_int,
            )
        });

        slots = slots
            .iter()
            .zip(futures_util::future::join_all(futures).await.into_iter())
            .filter_map(|(slot, result)| match result {
                Err(err) => {
                    tracing::info!("Failed to load block on slot {:?}: {:?}", slot, err);
                    Some(*slot)
                }
                Ok(Some(block)) => {
                    results.insert(*slot, block);
                    None
                }
                _ => None,
            })
            .collect();

        if !slots.is_empty() {
            Err(Custom(format!(
                "Failed to load blocks on slots {:?}",
                slots
            )))
        } else {
            Ok(results)
        }
    }

    async fn preload_blocks(
        &self,
        from_slot: Slot,
        max_slot: Slot,
        finalized_slot: Slot,
    ) -> ProgramResult<Slot> {
        let to_slot = from_slot + std::cmp::min(max_slot - from_slot, self.batch_size);
        self.solana_block_storage
            .store_blocks(
                self.load_blocks((from_slot..to_slot).collect()).await?,
                finalized_slot,
            )
            .await?;
        Ok(to_slot)
    }

    async fn get_latest_slots(&self) -> ProgramResult<(Slot, Slot)> {
        if let (Ok(confirmed_slot), Ok(finalized_slot)) = (
            self.client
                .get_slot_with_commitment(CommitmentConfig {
                    commitment: CommitmentLevel::Confirmed,
                })
                .await,
            self.client
                .get_slot_with_commitment(CommitmentConfig {
                    commitment: CommitmentLevel::Finalized,
                })
                .await,
        ) {
            Ok((confirmed_slot, finalized_slot))
        } else {
            Err(Custom("Failed to get latest slots from Solana".to_string()))
        }
    }

    async fn preload_blocks_until_in_sync(
        &self,
        mut from_slot: Slot,
        interval: Duration,
        reorg_event_tx: &UnboundedSender<Slot>,
    ) -> ProgramResult<Slot> {
        loop {
            from_slot = match self.get_latest_slots().await {
                Ok((to_slot, finalized_slot)) if from_slot < to_slot => {
                    let mut current_slot = from_slot;
                    while current_slot < to_slot {
                        current_slot = self
                            .preload_blocks(current_slot, to_slot, finalized_slot)
                            .await?;
                    }
                    self.update_finalized_slot(finalized_slot, reorg_event_tx)
                        .await?;
                    to_slot
                }
                Ok((_, finalized_slot)) => {
                    self.update_finalized_slot(finalized_slot, reorg_event_tx)
                        .await?;
                    tokio::time::sleep(interval).await;
                    break Ok(from_slot);
                }
                Err(err) => {
                    tracing::warn!("Unable to get latest slot from Solana: {:?}", err);
                    tokio::time::sleep(interval).await;
                    from_slot
                }
            }
        }
    }

    pub fn start_loading(
        self,
        start_slot: Option<Slot>,
        interval_ms: u64,
        idx_started_tx: Option<oneshot::Sender<()>>,
        reorg_event_tx: UnboundedSender<Slot>,
    ) -> JoinHandle<ProgramResult<()>> {
        tokio::spawn(async move {
            let mut from_slot = self
                .solana_block_storage
                .get_last_slot()
                .await?
                .or(start_slot)
                .ok_or(Custom(
                    "start_slot is not set and there's no registered blocks in SolanaBlockStorage"
                        .to_string(),
                ))?;

            tracing::info!("SolanaBlockLoader starting from slot: {:?}", from_slot);
            let sleep_duration = Duration::from_millis(interval_ms);
            from_slot = self
                .preload_blocks_until_in_sync(from_slot, sleep_duration, &reorg_event_tx)
                .await?;

            tracing::info!("SolanaBlockLoader is in sync with Solana validator");
            if let Some(idx_started_tx) = idx_started_tx {
                idx_started_tx
                    .send(())
                    .expect("Failed to send SolanaBlockLoader started signal");
            }

            loop {
                from_slot = match self
                    .preload_blocks_until_in_sync(from_slot, sleep_duration, &reorg_event_tx)
                    .await
                {
                    Ok(res) => res,
                    Err(err) => {
                        tracing::warn!(
                            "Failed to preload blocks starting from slot {:?}: {:?}",
                            from_slot,
                            err
                        );
                        from_slot
                    }
                };
            }
        })
    }

    // Reads solana blocks from SolanaBlockStorage from the highest one down to the lowest
    // and checks consistency of corresponding block's data with actual data on-chain.
    // Reloads blocks which were missed in DB or changed on-chain,
    // Submits numbers of slots where reload happens over the reorg_event_tx channel.
    pub fn start_checking(
        self,
        start_slot: Slot,
        end_slot: Option<Slot>,
        reorg_event_tx: UnboundedSender<Slot>,
    ) -> JoinHandle<ProgramResult<()>> {
        tokio::spawn(async move {
            let mut end_slot = if let Some(value) = end_slot {
                value
            } else {
                match self.solana_block_storage.get_last_slot().await {
                    Ok(Some(value)) => value,
                    Ok(None) => {
                        tracing::info!("SolanaBlockStorage is empty");
                        return Ok(());
                    }
                    Err(err) => {
                        return Err(Custom(format!(
                            "Failed to get last slot from SolanaBlockStorage {:?}",
                            err
                        )))
                    }
                }
            };

            if end_slot < start_slot {
                tracing::warn!(
                    "start_slot {:?} is greater than the last slot in SolanaBlockStorage: {:?}",
                    start_slot,
                    end_slot
                );
                return Ok(());
            }

            if self
                .solana_block_storage
                .get_block(start_slot)
                .await?
                .is_none()
            {
                tracing::warn!("start_slot {:?} is not in SolanaBlockStorage", start_slot);
                return Ok(());
            }

            tracing::info!(
                "SolanaBlockLoader start checking from slot {:?} to slot {:?}...",
                start_slot,
                end_slot
            );

            while end_slot >= start_slot {
                end_slot = self
                    .check_batch(start_slot, end_slot, &reorg_event_tx)
                    .await?;
            }

            tracing::info!("Check completed");
            Ok(())
        })
    }

    async fn check_batch(
        &self,
        start_slot: Slot,
        end_slot: Slot,
        reorg_event_tx: &UnboundedSender<Slot>,
    ) -> ProgramResult<Slot> {
        let mut num_checked = 0;
        let mut current_slot = end_slot;
        let mut blocks = BTreeMap::new();

        while num_checked < self.batch_size && current_slot >= start_slot {
            let block =
                if let Some(block) = self.solana_block_storage.get_block(current_slot).await? {
                    (*block).clone()
                } else if let Some(block) = load_block(
                    self.client.clone(),
                    self.commitment,
                    self.program_id,
                    current_slot,
                    self.block_retries,
                    self.tx_retries,
                    self.retry_int,
                )
                .await?
                {
                    tracing::info!(
                        "Block on slot {:?} missed in DB and reloaded.",
                        current_slot
                    );
                    (*block).clone()
                } else {
                    return Err(Custom(format!(
                        "Failed to load block on slot {:?}",
                        current_slot
                    )));
                };

            let new_current_slot = block.parent_slot as Slot;
            blocks.insert(current_slot, block);
            current_slot = new_current_slot;
            num_checked += 1;
        }

        self.recheck_finalized_blocks(blocks, reorg_event_tx)
            .await?;

        Ok(current_slot)
    }

    // - Updates the latest finalized slot on the Solana network.
    //
    // - Reloads any Solana blocks whose block hashes have changed since their initial loading.
    //
    // - Clears associated Ethereum block parameters for all Ethereum blocks that were derived
    //   from the reloaded Solana blocks.
    //
    // - Forces the rollup process to regenerate Ethereum blocks from the updated Solana blocks.
    async fn update_finalized_slot(
        &self,
        finalized_slot: Slot,
        reorg_event_tx: &UnboundedSender<Slot>,
    ) -> ProgramResult<()> {
        self.recheck_finalized_blocks(
            self.solana_block_storage
                .set_finalized_slot(finalized_slot)
                .await?,
            reorg_event_tx,
        )
        .await
    }

    async fn recheck_finalized_blocks(
        &self,
        blocks: BTreeMap<Slot, UiConfirmedBlock>,
        reorg_event_tx: &UnboundedSender<Slot>,
    ) -> ProgramResult<()> {
        let futures = blocks
            .iter()
            .map(|(slot_number, block)| self.recheck_finalized_block(*slot_number, block));

        let mut updated_blocks = BTreeMap::new();
        for (res, slot_number) in futures_util::future::join_all(futures)
            .await
            .into_iter()
            .zip(blocks.keys())
        {
            match res {
                Ok(Some(block)) => {
                    tracing::info!(
                        "Block on slot {:?} has been changed on-chain and refreshed in DB",
                        slot_number
                    );
                    updated_blocks.insert(*slot_number, block);
                }
                Err(err) => {
                    tracing::warn!("Failed to recheck block: {:?}", err);
                    return Err(err);
                }
                _ => {}
            }
        }

        let reset_from_slot = updated_blocks
            .first_key_value()
            .map(|(slot_number, _)| *slot_number);
        self.solana_block_storage
            .update_finalized_blocks(updated_blocks)
            .await?;

        if let Some(slot_number) = reset_from_slot {
            if let Err(err) = reorg_event_tx.send(slot_number) {
                tracing::warn!(
                    "Failed to send reorg event for slot {:?}: {:?}",
                    slot_number,
                    err
                );
            };
        }

        Ok(())
    }

    // Reloads block header on a given slot.
    // Compares its blockhash with a value got on initial loading.
    // If blockhash changed, reloads all EVM transactions and returns new version of a block
    async fn recheck_finalized_block(
        &self,
        slot_number: Slot,
        block: &UiConfirmedBlock,
    ) -> ProgramResult<Option<Arc<UiConfirmedBlock>>> {
        let commitment = CommitmentLevel::Finalized;

        match self
            .client
            .get_block_with_config(
                slot_number,
                RpcBlockConfig {
                    encoding: Some(UiTransactionEncoding::Base58),
                    transaction_details: None,
                    rewards: None,
                    commitment: Some(CommitmentConfig { commitment }),
                    max_supported_transaction_version: Some(0),
                },
                self.block_retries,
                self.retry_int,
            )
            .await
        {
            Ok(Some(finalized_block)) => {
                if finalized_block.blockhash != block.blockhash {
                    load_block(
                        self.client.clone(),
                        commitment,
                        self.program_id,
                        slot_number,
                        self.block_retries,
                        self.tx_retries,
                        self.retry_int,
                    )
                    .await
                } else {
                    Ok(None)
                }
            }
            Ok(None) => Ok(None),
            Err(err) => Err(Custom(format!(
                "Failed to recheck block {:?}: {:?}",
                slot_number, err
            ))),
        }
    }
}
