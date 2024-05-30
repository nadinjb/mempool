use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::collections::HashMap;

use async_trait::async_trait;
use starknet_api::core::ContractAddress;
use starknet_api::transaction::TransactionHash;
use starknet_mempool_infra::component_definitions::ComponentRequestHandler;
use starknet_mempool_infra::component_server::ComponentServer;
use starknet_mempool_types::errors::MempoolError;
use starknet_mempool_types::mempool_types::{
    Account, AccountState, MempoolInput, MempoolRequest, MempoolRequestAndResponseSender,
    MempoolResponse, MempoolResult, ThinTransaction,
};
use tokio::sync::mpsc::Receiver;

use crate::priority_queue::{PrioritizedTransaction, TransactionPriorityQueue};
use crate::staging_area::StagingArea;
use crate::transaction_store::TransactionStore;

#[cfg(test)]
#[path = "mempool_test.rs"]
pub mod mempool_test;

pub struct Mempool {
    // TODO: add docstring explaining visibility and coupling of the fields.
    txs_queue: TransactionPriorityQueue,
    // All transactions currently held in the mempool.
    tx_store: TransactionStore,
    staging: StagingArea,
    state: HashMap<ContractAddress, AccountState>,
}

impl Mempool {
    pub fn new(inputs: impl IntoIterator<Item = MempoolInput>) -> Self {
        let mut mempool = Mempool {
            txs_queue: TransactionPriorityQueue::default(),
            tx_store: TransactionStore::default(),
            state: HashMap::default(),
            staging: StagingArea::default(),
        };

        for MempoolInput { tx, account: Account { sender_address, state } } in inputs.into_iter() {
            // Attempts to insert a key-value pair into the mempool's state. Returns `None`
            // if the key was not present, otherwise returns the old value while updating
            // the new value.
            let existing_account_state = mempool.state.insert(sender_address, state);
            assert!(
                existing_account_state.is_none(),
                "Sender address: {:?} already exists in the mempool. Can't add {:?} to the \
                 mempool.",
                sender_address,
                tx
            );

            // Insert the transaction into the tx_store.
            let res = mempool.tx_store.push(tx.clone());
            assert!(res.is_ok(), "Transaction: {:?} already exists in the mempool.", tx.tx_hash);

            mempool.txs_queue.push(tx.clone().into());
        }

        mempool
    }

    pub fn empty() -> Self {
        Mempool::new([])
    }

    /// Retrieves up to `n_txs` transactions with the highest priority from the mempool.
    /// Transactions are guaranteed to be unique across calls until `commit_block` is invoked.
    // TODO: the last part about commit_block is incorrect if we delete txs in get_txs and then push
    // back. TODO: Consider renaming to `pop_txs` to be more consistent with the standard
    // library.
    pub fn get_txs(&mut self, n_txs: usize) -> MempoolResult<Vec<ThinTransaction>> {
        let pq_txs = self.txs_queue.pop_last_chunk(n_txs);

        let mut txs: Vec<ThinTransaction> = Vec::default();
        for pq_tx in &pq_txs {
            let tx = self.tx_store.get(&pq_tx.tx_hash)?;
            self.state.remove(&tx.sender_address);
            self.staging.insert(tx.tx_hash)?;
            txs.push(tx.clone());
        }

        Ok(txs)
    }

    // TODO(Ayelet): implement a method that returns the next eligible transaction for the given
    // sender address to be added to priority queue.
    #[allow(dead_code)]
    fn get_next_eligible_tx(
        &self,
        _sender_address: ContractAddress,
    ) -> Option<PrioritizedTransaction> {
        todo!()
    }

    /// Adds a new transaction to the mempool.
    /// TODO: support fee escalation and transactions with future nonces.
    /// TODO: change input type to `MempoolInput`.
    pub fn add_tx(&mut self, tx: ThinTransaction, account: Account) -> MempoolResult<()> {
        match self.state.entry(account.sender_address) {
            Occupied(_) => Err(MempoolError::DuplicateTransaction { tx_hash: tx.tx_hash }),
            Vacant(entry) => {
                entry.insert(account.state);
                // TODO(Mohammad): use `handle_tx`.
                self.txs_queue.push(tx.clone().into());
                self.tx_store.push(tx)?;

                Ok(())
            }
        }
    }

    /// Update the mempool's internal state according to the committed block's transactions.
    /// This method also updates internal state (resolves nonce gaps, updates account balances).
    // TODO: the part about resolving nonce gaps is incorrect if we delete txs in get_txs and then
    // push back.
    pub fn commit_block(
        &mut self,
        txs_in_block: &[TransactionHash],
        state_changes: HashMap<ContractAddress, AccountState>,
    ) -> MempoolResult<()> {
        let mut counter = 0;
        for tx_hash in txs_in_block {
            if self.staging.contains(tx_hash) {
                counter += 1;
                self.tx_store.remove(tx_hash)?;
            }
        }
        // It pops the first `counter` hashes from staging area.
        // As the transactions keep in the same order after the Mempool, the transactions included
        // in the block should be the first ones in the staging area.
        self.staging.remove(counter);

        for (contract_address, account_state) in state_changes {
            self.state.insert(contract_address, account_state);
        }

        // Re-insert transaction to PQ.
        for tx_hash in self.staging.iter() {
            let tx = self.tx_store.get(tx_hash)?.clone();
            self.txs_queue.push(tx.into());
        }

        // Cleanin the `StagingArea`.
        self.staging = StagingArea::default();

        Ok(())
    }
}

/// Wraps the mempool to enable inbound async communication from other components.
pub struct MempoolCommunicationWrapper {
    mempool: Mempool,
}

impl MempoolCommunicationWrapper {
    pub fn new(mempool: Mempool) -> Self {
        MempoolCommunicationWrapper { mempool }
    }

    fn add_tx(&mut self, mempool_input: MempoolInput) -> MempoolResult<()> {
        self.mempool.add_tx(mempool_input.tx, mempool_input.account)
    }

    fn get_txs(&mut self, n_txs: usize) -> MempoolResult<Vec<ThinTransaction>> {
        self.mempool.get_txs(n_txs)
    }
}

#[async_trait]
impl ComponentRequestHandler<MempoolRequest, MempoolResponse> for MempoolCommunicationWrapper {
    async fn handle_request(&mut self, request: MempoolRequest) -> MempoolResponse {
        match request {
            MempoolRequest::AddTransaction(mempool_input) => {
                MempoolResponse::AddTransaction(self.add_tx(mempool_input))
            }
            MempoolRequest::GetTransactions(n_txs) => {
                MempoolResponse::GetTransactions(self.get_txs(n_txs))
            }
        }
    }
}

type MempoolCommunicationServer =
    ComponentServer<MempoolCommunicationWrapper, MempoolRequest, MempoolResponse>;

pub fn create_mempool_server(
    mempool: Mempool,
    rx_mempool: Receiver<MempoolRequestAndResponseSender>,
) -> MempoolCommunicationServer {
    let mempool_communication_wrapper = MempoolCommunicationWrapper::new(mempool);
    ComponentServer::new(mempool_communication_wrapper, rx_mempool)
}
