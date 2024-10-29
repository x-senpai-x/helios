use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use common::types::{Block, BlockTag, Transactions};
use ethers::types::{Address, Transaction, H256, U256};
use tokio::{
    select,
    sync::{mpsc::Receiver, watch, RwLock},
};

#[derive(Clone)]
pub struct State {
    inner: Arc<RwLock<Inner>>,//read write lock 
}
//Multiple tasks or threads might need to read the state simultaneously (e.g., querying blocks or transactions).
// A read-write lock allows multiple readers to access the shared data concurrently, as long as no writers are active. 

impl State {
    pub fn new(
        mut block_recv: Receiver<Block>, //recieves new blocks
        mut finalized_block_recv: watch::Receiver<Option<Block>>, //watch changes in finalized blocks 
        history_length: u64,
    ) -> Self {
        let inner = Arc::new(RwLock::new(Inner::new(history_length)));
        let inner_ref = inner.clone();

        #[cfg(not(target_arch = "wasm32"))]
        let run = tokio::spawn;
        #[cfg(target_arch = "wasm32")]
        let run = wasm_bindgen_futures::spawn_local; //spawns task in wasm format for web assembly

        run(async move {
            loop {
                select! { //'select! ' await multiple asynchronous operations simultaneously and execute code based on which operation completes first.
                    block = block_recv.recv() => { //waits for new block to arrive 
                        if let Some(block) = block {
                            inner_ref.write().await.push_block(block);//pushes the block to the inner state and the state is updated
                        }
                    },
                    _ = finalized_block_recv.changed() => {
                        let block = finalized_block_recv.borrow_and_update().clone();//waits for some change in finalized blcok and updates the state
                        if let Some(block) = block {
                            inner_ref.write().await.push_finalized_block(block);
                        }

                    }
                }
            }
        });

        Self { inner }
    }

    pub async fn push_block(&self, block: Block) {
        self.inner.write().await.push_block(block);//acquires write lock and pushes the block to the inner state
    }

    // full block fetch

    pub async fn get_block(&self, tag: BlockTag) -> Option<Block> {
        //BlockTag is enum coneaining latest , finalized,specific  number 
        match tag {

            BlockTag::Latest => self //if latest block is requested then the last block is returned
                .inner
                .read()//acquires read lock
                .await //waits unitl lock is obtained until then control is returned to executor
                .blocks
                .last_key_value()//last key value pair is returned from blocks map 
                .map(|entry| entry.1)//extracts the block from the key value pair 
                .cloned(),
            BlockTag::Finalized => self.inner.read().await.finalized_block.clone(),
            BlockTag::Number(number) => self.inner.read().await.blocks.get(&number).cloned(),
        }
    }

    pub async fn get_block_by_hash(&self, hash: H256) -> Option<Block> {
        let inner = self.inner.read().await;
        inner
            .hashes
            .get(&hash)
            .and_then(|number| inner.blocks.get(number))//fetch block from the blocks map for the given nnmber
            .cloned()
    }

    // transaction fetch

    pub async fn get_transaction(&self, hash: H256) -> Option<Transaction> {//hash is transaction hash 
        let inner = self.inner.read().await;
        inner
            .txs
            .get(&hash)//gets txn location from the txs map
            .and_then(|loc| {//if location is found then fetch the block and the transaction from the block
                inner
                    .blocks
                    .get(&loc.block)//gets block from transaction lock 
                    .and_then(|block| match &block.transactions {//gets transaction from the block
                        Transactions::Full(txs) => txs.get(loc.index),
                        Transactions::Hashes(_) => unreachable!(),
                    })
            })
            .cloned()
    }

    pub async fn get_transaction_by_block_and_index(//we have blcok hash and index of the transaction in the block
        &self,
        block_hash: H256,
        index: u64,
    ) -> Option<Transaction> {
        let inner = self.inner.read().await;
        inner
            .hashes
            .get(&block_hash)//hashes is mapping of block hash to block number
            .and_then(|number| inner.blocks.get(number))//blocks is mapping of block number to block
            .and_then(|block| match &block.transactions {//checks the transaction field of block 
                Transactions::Full(txs) => txs.get(index as usize),//if block contains full transactions then fetch the transaction from the index
                Transactions::Hashes(_) => unreachable!(),//if block contains only hashes then it is unreachable
            })
            .cloned()
    }

    // block field fetch

    pub async fn get_state_root(&self, tag: BlockTag) -> Option<H256> {
        self.get_block(tag).await.map(|block| block.state_root)
    }

    pub async fn get_receipts_root(&self, tag: BlockTag) -> Option<H256> {
        self.get_block(tag).await.map(|block| block.receipts_root)
    }

    pub async fn get_base_fee(&self, tag: BlockTag) -> Option<U256> {
        self.get_block(tag)
            .await
            .map(|block| block.base_fee_per_gas)
    }

    pub async fn get_coinbase(&self, tag: BlockTag) -> Option<Address> {
        self.get_block(tag).await.map(|block| block.miner) //address of miner that used to recieve the block reward
    }

    // misc

    pub async fn latest_block_number(&self) -> Option<u64> {
        let inner = self.inner.read().await;
        inner.blocks.last_key_value().map(|entry| *entry.0)
    }

    pub async fn oldest_block_number(&self) -> Option<u64> {
        let inner = self.inner.read().await;
        inner.blocks.first_key_value().map(|entry| *entry.0)
    }
}

#[derive(Default)]
struct Inner {
    blocks: BTreeMap<u64, Block>,
    finalized_block: Option<Block>,
    hashes: HashMap<H256, u64>,
    txs: HashMap<H256, TransactionLocation>,
    history_length: u64,
}

impl Inner {
    pub fn new(history_length: u64) -> Self {
        Self {
            history_length,
            ..Default::default()
        }
    }

    pub fn push_block(&mut self, block: Block) {
        self.hashes.insert(block.hash, block.number.as_u64()); //append block hash and block number in the hash map 
        //maps each transaction hash to its location within the block 
        block
            .transactions
            .hashes()
            .iter()
            .enumerate()
            .for_each(|(i, tx)| {
                let location = TransactionLocation {
                    block: block.number.as_u64(),
                    index: i,
                };
                self.txs.insert(*tx, location);
            });
           //TransactionLocation is struct that contains the block number and the index of the transaction within the block
            //all txs in the pushed block are added to txs hash map in inner struct mapping Hash to TransactionLocation  

        self.blocks.insert(block.number.as_u64(), block);

        //Ensures that the number of blocks stored does not exceed the specified history_length.
        //if new block added then the oldest blcok removed from the blocks
        while self.blocks.len() as u64 > self.history_length {
            if let Some((number, _)) = self.blocks.first_key_value() {
                self.remove_block(*number);
            }
        }
    }
//since finalized block is pushed it means that block is already in the blocks map we only need to add the updated block 
//since on updating the block the block hash would change we need to remove the old block and add the updated block
    pub fn push_finalized_block(&mut self, block: Block) {
        self.finalized_block = Some(block.clone());//clones the block and adds it to the finalized block

        if let Some(old_block) = self.blocks.get(&block.number.as_u64()) {
            if old_block.hash != block.hash {//block is already there with different hash 
                self.remove_block(old_block.number.as_u64());
                self.push_block(block)
            }
        } else {//if not present then add the block
            self.push_block(block);
        }
    }

    fn remove_block(&mut self, number: u64) {
        if let Some(block) = self.blocks.remove(&number) {
            self.hashes.remove(&block.hash);
            //remove each transaction hash from the txs hash map
            block.transactions.hashes().iter().for_each(|tx| {
                self.txs.remove(tx);
            });
        }
    }
}

struct TransactionLocation {
    block: u64,
    index: usize,
}
