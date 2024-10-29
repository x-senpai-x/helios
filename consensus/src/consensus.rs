use std::marker::PhantomData;
use std::process;
use std::sync::Arc;

use chrono::Duration;
use eyre::eyre;
use eyre::Result;
use futures::future::join_all;

use ssz_rs::prelude::*;
use tokio::sync::mpsc::Sender;
use tracing::{debug, error, info, warn};
use zduny_wasm_timer::{SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc::channel;
use tokio::sync::mpsc::Receiver;
use tokio::sync::watch;

use common::types::Block;
use config::CheckpointFallback;
use config::Config;
use config::Network;

use super::rpc::ConsensusRpc;
use crate::constants::MAX_REQUEST_LIGHT_CLIENT_UPDATES;
use crate::database::Database;

use consensus_core::{
    apply_finality_update, apply_optimistic_update, apply_update,
    errors::ConsensusError,
    expected_current_slot, get_bits, is_current_committee_proof_valid,
    types::{
        Bytes32, ExecutionPayload, FinalityUpdate, GenericUpdate, LightClientStore,
        OptimisticUpdate, Update,
    },
    utils::calc_sync_period,
    verify_generic_update,
};

pub struct ConsensusClient<R: ConsensusRpc, DB: Database> {
    pub block_recv: Option<Receiver<Block>>,//channer reciever for recieving blocks from other part of application
    pub finalized_block_recv: Option<watch::Receiver<Option<Block>>>,//watch reciever for recieving finalized blocks 
    pub checkpoint_recv: watch::Receiver<Option<Vec<u8>>>,//watch reciever for recieving checkpoints information 
    genesis_time: u64,//genesis time of the blockchain ... used to caclculate the current slot 
    //used to align local blockchain state with network state
    db: DB,
    phantom: PhantomData<R>,// allows the compiler to know about the type parameter R without actually storing any value of type R in the struct.
}

#[derive(Debug)]
//consensus rpc interface provides way to fetch data from the consensus layer
//about blocks etc

//handles core logic of the consensus client
pub struct Inner<R: ConsensusRpc> {
    pub rpc: R,
    pub store: LightClientStore,//stores current state of the light client
    //struct 
    last_checkpoint: Option<Vec<u8>>,
    block_send: Sender<Block>,//channel sender for sending blocks to other part of application
    finalized_block_send: watch::Sender<Option<Block>>,
    checkpoint_send: watch::Sender<Option<Vec<u8>>>,//channel sender for sending checkpoints information to other part of application
    pub config: Arc<Config>,
}

impl<R: ConsensusRpc, DB: Database> ConsensusClient<R, DB> {
    pub fn new(rpc: &str, config: Arc<Config>) -> Result<ConsensusClient<R, DB>> {
        let (block_send, block_recv) = channel(256);//block updates
        //can hold upto 256 messages i.e 256 blcoks
        let (finalized_block_send, finalized_block_recv) = watch::channel(None);//finalized block updates
        //any changes to the value will be broadcasted to all recievers
        let (checkpoint_send, checkpoint_recv) = watch::channel(None);//checkpoint updates

        let rpc = rpc.to_string();
        let genesis_time = config.chain.genesis_time;
        let db = DB::new(&config)?;//db initialized
        //loads checkpoint from config else from db else from default checkpoint
        let initial_checkpoint = config.checkpoint.clone().unwrap_or_else(|| {
            db.load_checkpoint()//resuming from last known state in database 
                .unwrap_or(config.default_checkpoint.clone())
        });

        #[cfg(not(target_arch = "wasm32"))]
        let run = tokio::spawn;

        #[cfg(target_arch = "wasm32")]
        let run = wasm_bindgen_futures::spawn_local;
        //spawns asynchronous task on the tokio runtime

        run(async move {
            //inner struct initailized 
            //INITIALIZED IN THE CONSENSUSCLIENT ITSELF

            let mut inner = Inner::<R>::new(
                &rpc,
                block_send,
                finalized_block_send,
                checkpoint_send,
                config.clone(),
            );

            let res = inner.sync(&initial_checkpoint).await;
            //syncs the light client with the initial checkpoint 

            //if sync fails then we try to sync with the fallbacks 
            if let Err(err) = res {
                if config.load_external_fallback {//first checks if load_external_fallback is enabled in the config
                    let res = sync_all_fallbacks(&mut inner, config.chain.chain_id).await; //syncs using external fallback service 
                    if let Err(err) = res {
                        error!(target: "helios::consensus", err = %err, "sync failed");
                        process::exit(1);//if this also fails then application exits 
                    }
                } else if let Some(fallback) = &config.fallback {//else if an external fallback url is already specified by the user 
                    let res = sync_fallback(&mut inner, fallback).await;
                    if let Err(err) = res {
                        error!(target: "helios::consensus", err = %err, "sync failed");
                        process::exit(1);//exits 
                    }
                } else {
                    error!(target: "helios::consensus", err = %err, "sync failed");
                    process::exit(1);//If neither external fallback nor a specific fallback is configured, it simply logs the error and exits. 
                }
            }
            //after synchronezing the light client with the network we start the main loop
            //sending blocks starts 
            //sends blocks to various parts of the cpplication 
            _ = inner.send_blocks().await;
            //calls send_blocks and waits for it to complete 
            // sends the execution payloads and other updates to the appropriate channels.
            //_= ignotes the result 

           // loop starts when the asynchronous block of code is executed.

            loop {
                //calculates duration until next update is reqd
                zduny_wasm_timer::Delay::new(inner.duration_until_next_update().to_std().unwrap())
                //timer introduces delay in the loop until updated
                    .await
                    .unwrap();

                let res = inner.advance().await;
                //verifies and applies finality and optimistic updates 
                

                if let Err(err) = res {
                    warn!(target: "helios::consensus", "advance error: {}", err);
                    continue;
                    //statrs the next iteration 
                }

                let res = inner.send_blocks().await;
                //captures the result of the send_blocks function
                //sends blocks
                if let Err(err) = res {
                    warn!(target: "helios::consensus", "send error: {}", err);
                    continue;
                }
            }
        });


        Ok(ConsensusClient {
            block_recv: Some(block_recv),
            finalized_block_recv: Some(finalized_block_recv),
            checkpoint_recv,
            genesis_time,
            db,
            phantom: PhantomData,
        })
    }
    //checkppoint saved in the database and shutdown
    pub fn shutdown(&self) -> Result<()> {
        let checkpoint = self.checkpoint_recv.borrow();//reference of current checkpoint reciver borrowed
        if let Some(checkpoint) = checkpoint.as_ref() {//if a checkpoint is prresent then we save it in the database
            self.db.save_checkpoint(checkpoint)?;
        }

        Ok(())
    }

    pub fn expected_current_slot(&self) -> u64 {
        let now = SystemTime::now();

        expected_current_slot(now, self.genesis_time)
    }
}

async fn sync_fallback<R: ConsensusRpc>(inner: &mut Inner<R>, fallback: &str) -> Result<()> {
    let checkpoint = CheckpointFallback::fetch_checkpoint_from_api(fallback).await?;//since fallback is implemented we fetch the checkpoint from the fallback service given by the user
    inner.sync(checkpoint.as_bytes()).await
}

async fn sync_all_fallbacks<R: ConsensusRpc>(inner: &mut Inner<R>, chain_id: u64) -> Result<()> {
    let network = Network::from_chain_id(chain_id)?;
    let checkpoint = CheckpointFallback::new()//since no fallback is provided we use external fallback service
        .build()
        .await?
        .fetch_latest_checkpoint(&network)
        .await?;

    inner.sync(checkpoint.as_bytes()).await//we find the latest checkpoint and then synced to that checkpoint from the current checkpoint fetched through the fallback service  
}

impl<R: ConsensusRpc> Inner<R> {
    pub fn new(
        rpc: &str,
        block_send: Sender<Block>,
        finalized_block_send: watch::Sender<Option<Block>>,
        checkpoint_send: watch::Sender<Option<Vec<u8>>>,
        config: Arc<Config>,
    ) -> Inner<R> {
        let rpc = R::new(rpc);//rpc initialized 

        Inner {
            rpc,
            store: LightClientStore::default(),
            last_checkpoint: None,
            block_send,
            finalized_block_send,
            checkpoint_send,
            config,
        }
    }

    pub async fn check_rpc(&self) -> Result<()> {
        let chain_id = self.rpc.chain_id().await?;

        if chain_id != self.config.chain.chain_id {
            Err(ConsensusError::IncorrectRpcNetwork.into())
        } else {
            Ok(())
        }
    }

    pub async fn get_execution_payload(&self, slot: &Option<u64>) -> Result<ExecutionPayload> {
        //first get the slot of optimistic header and then the blocok 
        let slot = slot.unwrap_or(self.store.optimistic_header.slot.into());
        // If slot is Some(u64), it uses that value. 
        //If slot is None, it defaults to the slot from self.store.optimistic_header
        let mut block = self.rpc.get_block(slot).await?;
        let block_hash = block.hash_tree_root()?;//hash computed 

        let latest_slot = self.store.optimistic_header.slot;
        let finalized_slot = self.store.finalized_header.slot;

        let verified_block_hash = if slot == latest_slot.as_u64() {//checks if slot given is sameas the latest slot then we compute the hash of optimistic header
            self.store.optimistic_header.clone().hash_tree_root()?
        } else if slot == finalized_slot.as_u64() {//if slot matches funalized sllot then hash of funalized header is computed
            self.store.finalized_header.clone().hash_tree_root()?
        } else {
            return Err(ConsensusError::PayloadNotFound(slot).into());
        };
            //we check if verified block hash computed above is same as block hash 
            //if it is then we return the execution payload of the block
            //else returns an error 
        if verified_block_hash != block_hash {
            Err(ConsensusError::InvalidHeaderHash(
                block_hash.to_string(),
                verified_block_hash.to_string(),
            )
            .into())
        } else {
            Ok(block.body.execution_payload().clone())
        }
    }

    pub async fn get_payloads(
        &self,
        start_slot: u64,
        end_slot: u64,
    ) -> Result<Vec<ExecutionPayload>> {
        //retrieves a sequence of execution payload instances as vectors
        let payloads_fut = (start_slot..end_slot)
            .rev()
            .map(|slot| self.rpc.get_block(slot));
//.rev() ensures blocks retrieved from most trecent to start slot
        let mut prev_parent_hash: Bytes32 = self //ensures that the parent hash of the current block is the same as the hash of the previous block
            .rpc
            .get_block(end_slot)
            .await?
            .body
            .execution_payload()
            .parent_hash() //defined in block struct 
            .clone();

        let mut payloads: Vec<ExecutionPayload> = Vec::new();
        for result in join_all(payloads_fut).await {
            if result.is_err() {
                continue;
            }
            let payload = result.unwrap().body.execution_payload().clone();
            if payload.block_hash() != &prev_parent_hash { 
                warn!(
                    target: "helios::consensus",
                    error = %ConsensusError::InvalidHeaderHash(
                        format!("{prev_parent_hash:02X?}"),
                        format!("{:02X?}", payload.parent_hash()),
                    ),
                    "error while backfilling blocks"
                );
                break;
            }
            prev_parent_hash = payload.parent_hash().clone();
            payloads.push(payload);
        }
        Ok(payloads)
    }

    pub async fn sync(&mut self, checkpoint: &[u8]) -> Result<()> {
        self.store = LightClientStore::default();//stores finalized beader, current sync committee etc
        self.last_checkpoint = None;

        self.bootstrap(checkpoint).await?;

        let current_period = calc_sync_period(self.store.finalized_header.slot.into());
        let updates = self
            .rpc
            .get_updates(current_period, MAX_REQUEST_LIGHT_CLIENT_UPDATES)
            .await?;

        for update in updates {
            self.verify_update(&update)?;
            self.apply_update(&update);
        }

        let finality_update = self.rpc.get_finality_update().await?;
        self.verify_finality_update(&finality_update)?;
        self.apply_finality_update(&finality_update);

        let optimistic_update = self.rpc.get_optimistic_update().await?;
        self.verify_optimistic_update(&optimistic_update)?;
        self.apply_optimistic_update(&optimistic_update);

        info!(
            target: "helios::consensus",
            "consensus client in sync with checkpoint: 0x{}",
            hex::encode(checkpoint)
        );

        Ok(())
    }

    pub async fn advance(&mut self) -> Result<()> {
        let finality_update = self.rpc.get_finality_update().await?;//struct formed 

        self.verify_finality_update(&finality_update)?;
        self.apply_finality_update(&finality_update);

        let optimistic_update = self.rpc.get_optimistic_update().await?;
        self.verify_optimistic_update(&optimistic_update)?;
        self.apply_optimistic_update(&optimistic_update);

        if self.store.next_sync_committee.is_none() {//if there is no next sync committee then we check for updates
            debug!(target: "helios::consensus", "checking for sync committee update");
            let current_period = calc_sync_period(self.store.finalized_header.slot.into());
            //epoch=slot/32
            //256 epochs in a period(sync committee )
            let mut updates = self.rpc.get_updates(current_period, 1).await?;
            //gets the updates from the network for current sync committee

            //if only one update is recieved then we verify and apply the update
            if updates.len() == 1 {
                let update = updates.get_mut(0).unwrap();
                let res = self.verify_update(update);
                //a more general update type that can represent various types of updates, 
                //these are generic updates and do not finalize the state of blockchain  


                if res.is_ok() {
                    info!(target: "helios::consensus", "updating sync committee");
                    self.apply_update(update);
                }
            }
        }

        Ok(())
    }

    pub async fn send_blocks(&self) -> Result<()> {
        //optimistic header is the block header of the likely block that might be added in the blockchain
        //slot extracts the slot number from the optimistic header
        let slot = self.store.optimistic_header.slot.as_u64(); //gets slot number ... current slot being processed 
        let payload = self.get_execution_payload(&Some(slot)).await?;//executitonpayload: contains the block data (transactions and state changes  ) of the optimistic header 
        //extracts info using slot 
        //These blocks are gossiped only between consensus (Eth2) nodes.
        //. The .await keyword is used to pause the execution of the function until the asynchronous operation completes.

        let finalized_slot = self.store.finalized_header.slot.as_u64();//represents a block that has been finalized 
        let finalized_payload = self.get_execution_payload(&Some(finalized_slot)).await?;

        self.block_send.send(payload.into()).await?;//sends payload to the channel so that updates can be given to other parts of application as well
        self.finalized_block_send
            .send(Some(finalized_payload.into()))?;
        self.checkpoint_send.send(self.last_checkpoint.clone())?;

        Ok(())
    }

    /// Gets the duration until the next update
    /// Updates are scheduled for 4 seconds into each slot
    pub fn duration_until_next_update(&self) -> Duration {
        let current_slot = self.expected_current_slot();
        let next_slot = current_slot + 1;
        let next_slot_timestamp = self.slot_timestamp(next_slot);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let time_to_next_slot = next_slot_timestamp - now;
        let next_update = time_to_next_slot + 4;
        //adds buuffer of  4 seconds to account for network delays 

        Duration::try_seconds(next_update as i64).unwrap()
    }
//get_bootstrap:The function loads bootstrap data from a local JSON file, deserializes it, and returns it. 
//This data is typically used to initialize a node with a known state or configuration in a blockchain or distributed system.
    pub async fn bootstrap(&mut self, checkpoint: &[u8]) -> Result<()> {
        let mut bootstrap = self
            .rpc
            .get_bootstrap(checkpoint)
            .await
            .map_err(|_| eyre!("could not fetch bootstrap"))?;

        let is_valid = self.is_valid_checkpoint(bootstrap.header.slot.into());//checks the blockhash slot age and returns true if it is less than 14 days old


        if !is_valid {
            if self.config.strict_checkpoint_age {
                return Err(ConsensusError::CheckpointTooOld.into());
            } else {
                warn!(target: "helios::consensus", "checkpoint too old, consider using a more recent block");
            }
        }

        let committee_valid = is_current_committee_proof_valid(
            &bootstrap.header,
            &mut bootstrap.current_sync_committee,
            &bootstrap.current_sync_committee_branch,
        );

        let header_hash = bootstrap.header.hash_tree_root()?.to_string();
        let expected_hash = format!("0x{}", hex::encode(checkpoint));
        let header_valid = header_hash == expected_hash;

        if !header_valid {
            return Err(ConsensusError::InvalidHeaderHash(expected_hash, header_hash).into());
        }

        if !committee_valid {
            return Err(ConsensusError::InvalidCurrentSyncCommitteeProof.into());
        }

        self.store = LightClientStore {
            finalized_header: bootstrap.header.clone(),
            current_sync_committee: bootstrap.current_sync_committee,
            next_sync_committee: None,
            optimistic_header: bootstrap.header.clone(),
            previous_max_active_participants: 0,
            current_max_active_participants: 0,
        };

        Ok(())
    }

    pub fn verify_update(&self, update: &Update) -> Result<()> {
        let update = GenericUpdate::from(update);
        let expected_current_slot = self.expected_current_slot();

        verify_generic_update(
            &update,
            expected_current_slot,
            &self.store,
            self.config.chain.genesis_root.clone().try_into().unwrap(),
            &self.config.forks,
        )
    }
    //updates the checkpoint
    pub fn apply_update(&mut self, update: &Update) {
        let new_checkpoint = apply_update(&mut self.store, update);
        if new_checkpoint.is_some() {
            self.last_checkpoint = new_checkpoint;
        }
    }
    //validates the finality update
    fn verify_finality_update(&self, update: &FinalityUpdate) -> Result<()> {
        let update = GenericUpdate::from(update);//converts finalityUpdate instance in GenericUpdate instance
        // The GenericUpdate type is a more generalized structure that can represent various types of updates, including FinalityUpdate, OptimisticUpdate, and Update
        let expected_current_slot = self.expected_current_slot();
            //we are basically calling the below function 
            //and verifying the update
            //acutal validation
        verify_generic_update(
            &update,
            expected_current_slot,
            &self.store,
            self.config.chain.genesis_root.clone().try_into().unwrap(),
            &self.config.forks,
        )
    }
    //validates the optimistic update
    fn verify_optimistic_update(&self, update: &OptimisticUpdate) -> Result<()> {
        let update = GenericUpdate::from(update);
        let expected_current_slot = self.expected_current_slot();

        verify_generic_update(
            &update,
            expected_current_slot,
            &self.store,
            self.config.chain.genesis_root.clone().try_into().unwrap(),
            &self.config.forks,
        )
    }

    fn apply_finality_update(&mut self, update: &FinalityUpdate) {
        let new_checkpoint = apply_finality_update(&mut self.store, update);
        if new_checkpoint.is_some() {
            self.last_checkpoint = new_checkpoint;
        }
        self.log_finality_update(update);
    }

    fn log_finality_update(&self, update: &FinalityUpdate) {
        //just logs the finality update after it is applied 
        let participation =
            get_bits(&update.sync_aggregate.sync_committee_bits) as f32 / 512f32 * 100f32;
            //checks number  of '1's in the bitfield and calculates the participation percentage of the sync committee in consensus process 
            //and stored as a float value and then percentage calculated 
        let decimals = if participation == 100.0 { 1 } else { 2 };

        //If participation is exactly 100%, it uses 1 decimal place (1); otherwise, it uses 2 decimal places (2).
       
        let age = self.age(self.store.finalized_header.slot.as_u64());//checks the age of the finalized header

        info!(
            target: "helios::consensus",
            "finalized slot             slot={}  confidence={:.decimals$}%  age={:02}:{:02}:{:02}:{:02}",
            self.store.finalized_header.slot.as_u64(),
            participation,
            age.num_days(),
            age.num_hours() % 24,
            age.num_minutes() % 60,
            age.num_seconds() % 60,
        );
    }

    fn apply_optimistic_update(&mut self, update: &OptimisticUpdate) {
        let new_checkpoint = apply_optimistic_update(&mut self.store, update);
        if new_checkpoint.is_some() {
            self.last_checkpoint = new_checkpoint;
        }
        self.log_optimistic_update(update);
    }

    fn log_optimistic_update(&self, update: &OptimisticUpdate) {
        let participation =
            get_bits(&update.sync_aggregate.sync_committee_bits) as f32 / 512f32 * 100f32;
        let decimals = if participation == 100.0 { 1 } else { 2 };
        let age = self.age(self.store.optimistic_header.slot.as_u64());

        info!(
            target: "helios::consensus",
            "updated head               slot={}  confidence={:.decimals$}%  age={:02}:{:02}:{:02}:{:02}",
            self.store.optimistic_header.slot.as_u64(),
            participation,
            age.num_days(),
            age.num_hours() % 24,
            age.num_minutes() % 60,
            age.num_seconds() % 60,
        );
    }

    fn age(&self, slot: u64) -> Duration {
        let expected_time = self.slot_timestamp(slot);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
        let delay = now - std::time::Duration::from_secs(expected_time);
        chrono::Duration::from_std(delay).unwrap()
    }
        //tells expected current slot in the netwoork based on current time
    pub fn expected_current_slot(&self) -> u64 {
        let now = SystemTime::now();

        expected_current_slot(now, self.config.chain.genesis_time)
    }

    fn slot_timestamp(&self, slot: u64) -> u64 {
        slot * 12 + self.config.chain.genesis_time
    }

    // Determines blockhash_slot age and returns true if it is less than 14 days old
    fn is_valid_checkpoint(&self, blockhash_slot: u64) -> bool {
        let current_slot = self.expected_current_slot();
        let current_slot_timestamp = self.slot_timestamp(current_slot);
        let blockhash_slot_timestamp = self.slot_timestamp(blockhash_slot);

        let slot_age = current_slot_timestamp
            .checked_sub(blockhash_slot_timestamp)
            .unwrap_or_default();

        slot_age < self.config.max_checkpoint_age
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{
        consensus::calc_sync_period,
        constants::MAX_REQUEST_LIGHT_CLIENT_UPDATES,
        rpc::{mock_rpc::MockRpc, ConsensusRpc},
        Inner,
    };
    use consensus_core::errors::ConsensusError;
    use consensus_core::types::{BLSPubKey, Header, SignatureBytes};

    use config::{networks, Config};
    use tokio::sync::{mpsc::channel, watch};

    async fn get_client(strict_checkpoint_age: bool, sync: bool) -> Inner<MockRpc> {
        let base_config = networks::mainnet();
        let config = Config {
            consensus_rpc: String::new(),
            execution_rpc: String::new(),
            chain: base_config.chain,
            forks: base_config.forks,
            strict_checkpoint_age,
            ..Default::default()
        };

        let checkpoint =
            hex::decode("5afc212a7924789b2bc86acad3ab3a6ffb1f6e97253ea50bee7f4f51422c9275")
                .unwrap();

        let (block_send, _) = channel(256);
        let (finalized_block_send, _) = watch::channel(None);
        let (channel_send, _) = watch::channel(None);

        let mut client = Inner::new(
            "testdata/",
            block_send,
            finalized_block_send,
            channel_send,
            Arc::new(config),
        );

        if sync {
            client.sync(&checkpoint).await.unwrap()
        } else {
            client.bootstrap(&checkpoint).await.unwrap();
        }

        client
    }

    #[tokio::test]
    async fn test_verify_update() {
        let client = get_client(false, false).await;
        let period = calc_sync_period(client.store.finalized_header.slot.into());
        let updates = client
            .rpc
            .get_updates(period, MAX_REQUEST_LIGHT_CLIENT_UPDATES)
            .await
            .unwrap();

        let update = updates[0].clone();
        client.verify_update(&update).unwrap();
    }

    #[tokio::test]
    async fn test_verify_update_invalid_committee() {
        let client = get_client(false, false).await;
        let period = calc_sync_period(client.store.finalized_header.slot.into());
        let updates = client
            .rpc
            .get_updates(period, MAX_REQUEST_LIGHT_CLIENT_UPDATES)
            .await
            .unwrap();

        let mut update = updates[0].clone();
        update.next_sync_committee.pubkeys[0] = BLSPubKey::default();

        let err = client.verify_update(&update).err().unwrap();
        assert_eq!(
            err.to_string(),
            ConsensusError::InvalidNextSyncCommitteeProof.to_string()
        );
    }

    #[tokio::test]
    async fn test_verify_update_invalid_finality() {
        let client = get_client(false, false).await;
        let period = calc_sync_period(client.store.finalized_header.slot.into());
        let updates = client
            .rpc
            .get_updates(period, MAX_REQUEST_LIGHT_CLIENT_UPDATES)
            .await
            .unwrap();

        let mut update = updates[0].clone();
        update.finalized_header = Header::default();

        let err = client.verify_update(&update).err().unwrap();
        assert_eq!(
            err.to_string(),
            ConsensusError::InvalidFinalityProof.to_string()
        );
    }

    #[tokio::test]
    async fn test_verify_update_invalid_sig() {
        let client = get_client(false, false).await;
        let period = calc_sync_period(client.store.finalized_header.slot.into());
        let updates = client
            .rpc
            .get_updates(period, MAX_REQUEST_LIGHT_CLIENT_UPDATES)
            .await
            .unwrap();

        let mut update = updates[0].clone();
        update.sync_aggregate.sync_committee_signature = SignatureBytes::default();

        let err = client.verify_update(&update).err().unwrap();
        assert_eq!(
            err.to_string(),
            ConsensusError::InvalidSignature.to_string()
        );
    }

    #[tokio::test]
    async fn test_verify_finality() {
        let client = get_client(false, true).await;

        let update = client.rpc.get_finality_update().await.unwrap();

        client.verify_finality_update(&update).unwrap();
    }

    #[tokio::test]
    async fn test_verify_finality_invalid_finality() {
        let client = get_client(false, true).await;

        let mut update = client.rpc.get_finality_update().await.unwrap();
        update.finalized_header = Header::default();

        let err = client.verify_finality_update(&update).err().unwrap();
        assert_eq!(
            err.to_string(),
            ConsensusError::InvalidFinalityProof.to_string()
        );
    }

    #[tokio::test]
    async fn test_verify_finality_invalid_sig() {
        let client = get_client(false, true).await;

        let mut update = client.rpc.get_finality_update().await.unwrap();
        update.sync_aggregate.sync_committee_signature = SignatureBytes::default();

        let err = client.verify_finality_update(&update).err().unwrap();
        assert_eq!(
            err.to_string(),
            ConsensusError::InvalidSignature.to_string()
        );
    }

    #[tokio::test]
    async fn test_verify_optimistic() {
        let client = get_client(false, true).await;

        let update = client.rpc.get_optimistic_update().await.unwrap();
        client.verify_optimistic_update(&update).unwrap();
    }

    #[tokio::test]
    async fn test_verify_optimistic_invalid_sig() {
        let client = get_client(false, true).await;

        let mut update = client.rpc.get_optimistic_update().await.unwrap();
        update.sync_aggregate.sync_committee_signature = SignatureBytes::default();

        let err = client.verify_optimistic_update(&update).err().unwrap();
        assert_eq!(
            err.to_string(),
            ConsensusError::InvalidSignature.to_string()
        );
    }

    #[tokio::test]
    #[should_panic]
    async fn test_verify_checkpoint_age_invalid() {
        get_client(true, false).await;
    }
}
