use std::sync::Arc;

use futures::future::{self, FutureExt};
use parking_lot::{RwLock, RwLockUpgradableReadGuard};
use rethnet_eth::{
    block::{BlockAndCallers, DetailedBlock},
    receipt::BlockReceipt,
    remote::{self, BlockSpec, RpcClient, RpcClientError},
    transaction::SignedTransaction,
    B256, U256,
};
use tokio::runtime::Runtime;

use super::storage::SparseBlockchainStorage;

#[derive(Debug)]
pub struct RemoteBlockchain {
    client: RpcClient,
    runtime: Arc<Runtime>,
    cache: RwLock<SparseBlockchainStorage>,
}

impl RemoteBlockchain {
    /// Constructs a new instance with the provided RPC client.
    pub fn new(client: RpcClient, runtime: Arc<Runtime>) -> Self {
        Self {
            client,
            runtime,
            cache: RwLock::new(SparseBlockchainStorage::default()),
        }
    }

    /// Retrieves the block with the provided hash, if it exists.
    pub fn block_by_hash(&self, hash: &B256) -> Result<Option<Arc<DetailedBlock>>, RpcClientError> {
        let cache = self.cache.upgradable_read();

        if let Some(block) = cache.block_by_hash(hash).cloned() {
            return Ok(Some(block));
        }

        if let Some(block) = tokio::task::block_in_place(move || {
            self.runtime
                .block_on(self.client.get_block_by_hash_with_transaction_data(hash))
        })? {
            self.fetch_and_cache_block(cache, block).map(Option::Some)
        } else {
            Ok(None)
        }
    }

    /// Retrieves the block with the provided number, if it exists.
    pub fn block_by_number(&self, number: &U256) -> Result<Arc<DetailedBlock>, RpcClientError> {
        let cache = self.cache.upgradable_read();

        if let Some(block) = cache.block_by_number(number).cloned() {
            Ok(block)
        } else {
            let block = self.runtime.block_on(
                self.client
                    .get_block_by_number_with_transaction_data(BlockSpec::Number(*number)),
            )?;

            self.fetch_and_cache_block(cache, block)
        }
    }

    /// Retrieves the block that contains a transaction with the provided hash, if it exists.
    pub fn block_by_transaction_hash(
        &self,
        transaction_hash: &B256,
    ) -> Result<Option<Arc<DetailedBlock>>, RpcClientError> {
        if let Some(block) = self
            .cache
            .read()
            .block_by_transaction_hash(transaction_hash)
            .cloned()
        {
            Ok(Some(block))
        } else if let Some(transaction) = tokio::task::block_in_place(move || {
            self.runtime
                .block_on(self.client.get_transaction_by_hash(transaction_hash))
        })? {
            // TODO: is this true?
            self.block_by_hash(&transaction.block_hash.expect("Not a pending transaction"))
        } else {
            Ok(None)
        }
    }

    /// Retrieves the receipt of the transaction with the provided hash, if it exists.
    pub fn receipt_by_transaction_hash(
        &self,
        transaction_hash: &B256,
    ) -> Result<Option<Arc<BlockReceipt>>, RpcClientError> {
        let cache = self.cache.upgradable_read();

        if let Some(receipt) = cache.receipt_by_transaction_hash(transaction_hash) {
            Ok(Some(receipt.clone()))
        } else if let Some(receipt) = tokio::task::block_in_place(move || {
            self.runtime
                .block_on(self.client.get_transaction_receipt(transaction_hash))
        })? {
            Ok(Some({
                let mut cache = RwLockUpgradableReadGuard::upgrade(cache);
                // SAFETY: the receipt with this hash didn't exist yet, so it must be unique
                unsafe { cache.insert_receipt_unchecked(receipt) }.clone()
            }))
        } else {
            Ok(None)
        }
    }

    /// Retrieves the total difficulty at the block with the provided hash.
    pub fn total_difficulty_by_hash(&self, hash: &B256) -> Result<Option<U256>, RpcClientError> {
        let cache = self.cache.upgradable_read();

        if let Some(difficulty) = cache.total_difficulty_by_hash(hash).cloned() {
            Ok(Some(difficulty))
        } else if let Some(block) = tokio::task::block_in_place(move || {
            self.runtime
                .block_on(self.client.get_block_by_hash_with_transaction_data(hash))
        })? {
            let total_difficulty = block
                .total_difficulty
                .expect("Must be present as this is not a pending transaction");

            self.fetch_and_cache_block(cache, block)?;

            Ok(Some(total_difficulty))
        } else {
            Ok(None)
        }
    }

    /// Fetches detailed block information and caches the block.
    fn fetch_and_cache_block(
        &self,
        cache: RwLockUpgradableReadGuard<'_, SparseBlockchainStorage>,
        block: remote::eth::Block<remote::eth::Transaction>,
    ) -> Result<Arc<DetailedBlock>, RpcClientError> {
        let total_difficulty = block
            .total_difficulty
            .expect("Must be present as this is not a pending block");

        let BlockAndCallers {
            block,
            transaction_callers,
        } = block
            .try_into()
            .expect("Conversion must succeed, as we're not retrieving a pending block");

        let transaction_hashes: Vec<B256> = block
            .transactions
            .iter()
            .map(SignedTransaction::hash)
            .collect();

        let receipts = tokio::task::block_in_place(move || {
            self.runtime.block_on({
                future::try_join_all(transaction_hashes.iter().map(|hash| {
                    self.client
                        .get_transaction_receipt(hash)
                        .map(|result| result.map(|receipt| Arc::new(receipt.unwrap())))
                }))
            })
        })?;

        let block = DetailedBlock::new(block, transaction_callers, receipts);
        let block = {
            let mut remote_cache = RwLockUpgradableReadGuard::upgrade(cache);
            // SAFETY: the block with this number didn't exist yet, so it must be unique
            unsafe { remote_cache.insert_block_unchecked(block, total_difficulty) }.clone()
        };

        Ok(block)
    }
}