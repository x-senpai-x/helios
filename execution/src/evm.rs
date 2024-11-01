use std::{borrow::BorrowMut, collections::HashMap, str::FromStr, sync::Arc};

use ethers::types::transaction::eip2930::AccessListItem;
use eyre::{Report, Result};
use futures::future::join_all;
use revm::{
    primitives::{
        AccountInfo, Address, Bytecode, Bytes, Env, ExecutionResult, ResultAndState, TransactTo,
        B256, U256,
    },
    Database, Evm as Revm,
};
use tracing::trace;

use crate::{
    constants::PARALLEL_QUERY_BATCH_SIZE, errors::EvmError, rpc::ExecutionRpc, types::CallOpts,
    ExecutionClient,
};
use common::types::BlockTag;

pub struct Evm<R: ExecutionRpc> {
    execution: Arc<ExecutionClient<R>>,
    chain_id: u64,
    tag: BlockTag,
}

impl<R: ExecutionRpc> Evm<R> {
    pub fn new(execution: Arc<ExecutionClient<R>>, chain_id: u64, tag: BlockTag) -> Self {
        Evm {
            execution,
            chain_id,
            tag,
        }
    }
    //exectures transaction
    /*The call_inner function returns a more complex or lower-level result (ExecutionResult) that contains various details about the transaction execution. If you were to call call_inner directly, you’d need to handle all possible outcomes (like Success, Revert, and Halt) every time you invoke the function.

The call function simplifies this by:

	•	Wrapping the successful output (Success) into a usable Vec<u8> and returning it directly.
	•	Converting reverts and halts into standardized error types (EvmError::Revert), so you don’t have to deal with raw low-level results repeatedly.
 */
    pub async fn call(&mut self, opts: &CallOpts) -> Result<Vec<u8>, EvmError> {
        let tx = self.call_inner(opts).await?;

        match tx.result {   
            ExecutionResult::Success { output, .. } => Ok(output.into_data().to_vec()),//output fetched from the transaction and converted to vector 
            ExecutionResult::Revert { output, .. } => {
                Err(EvmError::Revert(Some(output.to_vec().into())))
            }
            ExecutionResult::Halt { .. } => Err(EvmError::Revert(None)),
        }
    }
    pub async fn estimate_gas(&mut self, opts: &CallOpts) -> Result<u64, EvmError> {
        let tx = self.call_inner(opts).await?;

        match tx.result {
            ExecutionResult::Success { gas_used, .. } => Ok(gas_used),
            ExecutionResult::Revert { gas_used, .. } => Ok(gas_used),
            ExecutionResult::Halt { gas_used, .. } => Ok(gas_used),
        }
    }

    async fn call_inner(&mut self, opts: &CallOpts) -> Result<ResultAndState, EvmError> {
        let mut db = ProofDB::new(self.tag, self.execution.clone());//proofDB is wrpapper around evm state
        _ = db.state.prefetch_state(opts).await; //ensures that all necessary accounts, contracts, and storage slots are loaded into memory before execution, reducing latency during execution.
        
        let env = Box::new(self.get_env(opts, self.tag).await);//creates env for evm execution
        let evm: Revm<'_, (), ProofDB<R>> = Revm::builder().with_db(db).with_env(env).build();//creates instance of evm with env variables and db provided 
        let mut ctx: revm::ContextWithHandlerCfg<(), ProofDB<R>> = evm.into_context_with_handler_cfg();

        let tx_res = loop {
            let db = ctx.context.evm.db.borrow_mut();//borrows the db mutably
             if db.state.needs_update() {//if state needs to be updated then updated until then transaction is not processed 
                db.state.update_state().await.unwrap();
            }

            let mut evm = Revm::builder().with_context_with_handler_cfg(ctx).build(); //rebuilds evm with updated context
            let res: std::result::Result<ResultAndState, revm::primitives::EVMError<Report>> = evm.transact();//executes transaction ; res is success or failure 

            ctx = evm.into_context_with_handler_cfg();//ctx updated with latest state 

            if res.is_ok() {//if transaction is successfull then break the loop
                //if the transaction was not successful, the loop continues. The EVM might need to update the state again and retry the transaction
                break res;
            }
        };

        tx_res.map_err(|_| EvmError::Generic("evm error".to_string()))//fter the transaction execution, the result is returned. If there was an error during the execution, the function maps the error to a custom EvmError::Generic with a message “evm error”.
    }
//The get_env function sets up the environment (Env) that the call_inner function later uses to execute the EVM transaction. Here’s a clearer breakdown of how the two functions work together:
    async fn get_env(&self, opts: &CallOpts, tag: BlockTag) -> Env {
        let mut env = Env::default();
        let to = convert_address(&opts.to.unwrap_or_default());//converts to the format expected by evm
        let from = convert_address(&opts.from.unwrap_or_default());

        env.tx.transact_to = TransactTo::Call(to);
        env.tx.caller = from;
        env.tx.value = opts
            .value
            .map(|value| convert_u256(&value))
            .unwrap_or_default();

        env.tx.data = Bytes::from(opts.data.clone().unwrap_or_default().to_vec());
        env.tx.gas_limit = opts.gas.map(|v| v.as_u64()).unwrap_or(u64::MAX);
        env.tx.gas_price = opts
            .gas_price
            .map(|gas_price| convert_u256(&gas_price))
            .unwrap_or_default();

        let block = self.execution.get_block(tag, false).await.unwrap();

        env.block.number = U256::from(block.number.as_u64());
        env.block.coinbase = convert_address(&block.miner);
        env.block.timestamp = U256::from(block.timestamp.as_u64());
        env.block.difficulty = convert_u256(&block.difficulty);
        env.cfg.chain_id = self.chain_id;

        env
    }
}

struct ProofDB<R: ExecutionRpc> {
    state: EvmState<R>,
}

impl<R: ExecutionRpc> ProofDB<R> {
    pub fn new(tag: BlockTag, execution: Arc<ExecutionClient<R>>) -> Self {
        let state = EvmState::new(execution.clone(), tag);
        ProofDB { state }
    }
}

enum StateAccess {
    Basic(Address),
    BlockHash(u64),
    Storage(Address, U256),
}

struct EvmState<R: ExecutionRpc> {
    basic: HashMap<Address, AccountInfo>,
    block_hash: HashMap<u64, B256>,
    storage: HashMap<Address, HashMap<U256, U256>>,
    block: BlockTag,
    access: Option<StateAccess>,// StateAccess might represent details about which parts of the state (such as accounts or storage slots) have been accessed or modified.
    execution: Arc<ExecutionClient<R>>,
}

impl<R: ExecutionRpc> EvmState<R> {
    pub fn new(execution: Arc<ExecutionClient<R>>, block: BlockTag) -> Self {
        Self {
            execution,
            block,
            basic: HashMap::new(),
            storage: HashMap::new(),
            block_hash: HashMap::new(),
            access: None,
        }
    }

    //Update_state function ensures that the EVM state (EvmState) is up-to-date by fetching the latest data from the ExecutionClient when necessary.

    pub async fn update_state(&mut self) -> Result<()> {
        /*	The StateAccess enum has three possible variants that represent what kind of data the EVM state needs to update:
	•	Basic(address): Updates basic account information (balance, nonce, code).
	•	Storage(address, slot): Updates a specific storage slot for a contract.
	•	BlockHash(number): Updates the block hash for a specific block number. */
    // Instead of refreshing the entire state, the function selectively updates only the part of the state indicated by the access field. This makes the state update process more efficient by focusing on specific accounts, storage slots, or blocks that are actually needed for the transaction.

        if let Some(access) = &self.access.take() {
            match access {
                StateAccess::Basic(address) => {
                    let address_ethers = ethers::types::Address::from_slice(address.as_slice());
                    let account = self
                        .execution
                        .get_account(&address_ethers, None, self.block)
                        .await?;

                    let bytecode = Bytecode::new_raw(account.code.into());
                    let code_hash = B256::from_slice(account.code_hash.as_bytes());
                    let balance = convert_u256(&account.balance);

                    let account = AccountInfo::new(balance, account.nonce, code_hash, bytecode);
                    self.basic.insert(*address, account);
                }
                StateAccess::Storage(address, slot) => {
                    let address_ethers = ethers::types::Address::from_slice(address.as_slice());
                    let slot_ethers = ethers::types::H256::from_slice(&slot.to_be_bytes::<32>());
                    let slots = [slot_ethers];
                    let account = self
                        .execution
                        .get_account(&address_ethers, Some(&slots), self.block)
                        .await?;

                    let storage = self.storage.entry(*address).or_default();
                    let value = *account.slots.get(&slot_ethers).unwrap();

                    let mut value_slice = [0u8; 32];
                    value.to_big_endian(value_slice.as_mut_slice());
                    let value = U256::from_be_slice(&value_slice);

                    storage.insert(*slot, value);
                }
                StateAccess::BlockHash(number) => {
                    let block = self
                        .execution
                        .get_block(BlockTag::Number(*number), false)
                        .await?;

                    let hash = B256::from_slice(block.hash.as_bytes());
                    self.block_hash.insert(*number, hash);
                }
            }
        }

        Ok(())
    }

    pub fn needs_update(&self) -> bool {
        self.access.is_some() //checks if access field contains any value or not 
    }

    pub fn get_basic(&mut self, address: Address) -> Result<AccountInfo> {
        if let Some(account) = self.basic.get(&address) {
            Ok(account.clone())
        } else {
            self.access = Some(StateAccess::Basic(address));
            eyre::bail!("state missing");
        }
    }

    pub fn get_storage(&mut self, address: Address, slot: U256) -> Result<U256> {
        let storage = self.storage.entry(address).or_default();
        if let Some(slot) = storage.get(&slot) {
            Ok(*slot)
        } else {
            self.access = Some(StateAccess::Storage(address, slot));
            eyre::bail!("state missing");
        }
    }

    pub fn get_block_hash(&mut self, block: u64) -> Result<B256> {
        if let Some(hash) = self.block_hash.get(&block) {
            Ok(*hash)
        } else {
            self.access = Some(StateAccess::BlockHash(block));
            eyre::bail!("state missing");
        }
    }

    pub async fn prefetch_state(&mut self, opts: &CallOpts) -> Result<()> {
        let mut list = self
            .execution//calls execution client
            .rpc//calls execution rpc 
            .create_access_list(opts, self.block)
            .await
            .map_err(EvmError::RpcError)?
            .0;//The access list specifies which addresses and storage keys the transaction is expected to touch during execution. This helps prefetch the required state data in advance.

//This helps the Ethereum Virtual Machine (EVM) understand and preemptively load the necessary state data, which can improve transaction execution efficiency and reduce costs.

        let from_access_entry = AccessListItem {
            address: opts.from.unwrap_or_default(),//from ad dress to check funds 
            storage_keys: Vec::default(),//storage key which are mapped to the address
        };

        let to_access_entry = AccessListItem {
            address: opts.to.unwrap_or_default(),
            storage_keys: Vec::default(),
        };

        let coinbase = self.execution.get_block(self.block, false).await?.miner;//fetched block address which is accessed bt miner
        let producer_access_entry = AccessListItem {
            address: coinbase,
            storage_keys: Vec::default(),
        };
        //list stored as vector 

        let list_addresses = list.iter().map(|elem| elem.address).collect::<Vec<_>>();

        if !list_addresses.contains(&from_access_entry.address) {// checks if the address from from_access_entry is not already present in list_addresses. if not then pushed to list 
            list.push(from_access_entry)
        }

        if !list_addresses.contains(&to_access_entry.address) {
            list.push(to_access_entry)
        }

        if !list_addresses.contains(&producer_access_entry.address) {
            list.push(producer_access_entry)
        }


        let mut account_map = HashMap::new(); //stores result of account entries 

        //Store the successfully retrieved account data in account_map.
        for chunk in list.chunks(PARALLEL_QUERY_BATCH_SIZE) {//controls how many elements are processed concurrently 
            //divides list into chunks of size PARALLEL_QUERY_BATCH_SIZE

            // Maps each AccessListItem in the current chunk to an asynchronous future that retrieves account data. This creates a vector of futures for the chunk.
            let account_chunk_futs = chunk.iter().map(|account| {
                let account_fut = self.execution.get_account(
                    &account.address,
                    Some(account.storage_keys.as_slice()),
                    self.block,
                );
                async move { (account.address, account_fut.await) }
            });

            let account_chunk = join_all(account_chunk_futs).await;//awaits the completion of all futures in the vector and returns a single future that produces a vector of results.
            //filters out any errors and stores the successful results in account_map
            account_chunk
                .into_iter()
                .filter(|i| i.1.is_ok())
                .for_each(|(key, value)| {
                    account_map.insert(key, value.ok().unwrap());
                });
        }
        //account contains address detailes and storage keys
        for (address, account) in account_map {
            let bytecode = Bytecode::new_raw(account.code.into());//Converts the raw bytecode of the account into a Bytecode object, which is a structured representation of the smart contract code associated with the account.
            let code_hash = B256::from_slice(account.code_hash.as_bytes());// Converts the code hash from the account into a B256 type, which represents a 256-bit hash
            let balance = convert_u256(&account.balance);

            let info = AccountInfo::new(balance, account.nonce, code_hash, bytecode);

            let address = convert_address(&address);
            self.basic.insert(address, info);

            for (slot, value) in account.slots {
                let slot = B256::from_slice(slot.as_bytes());
                let value = convert_u256(&value);

                self.storage
                    .entry(address)
                    .or_default()
                    .insert(B256::from(slot).into(), value);//uf no storage entry found then creates a new entry
            }
        }

        Ok(())
    }
}

impl<R: ExecutionRpc> Database for ProofDB<R> {
    type Error = Report;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Report> {
        if is_precompile(&address) {
            return Ok(Some(AccountInfo::default()));
        }

        trace!(
            target: "helios::evm",
            "fetch basic evm state for address=0x{}",
            hex::encode(address.as_slice())
        );

        Ok(Some(self.state.get_basic(address)?))
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Report> {
        trace!(target: "helios::evm", "fetch block hash for block={:?}", number);
        self.state.get_block_hash(number)
    }

    fn storage(&mut self, address: Address, slot: U256) -> Result<U256, Report> {
        trace!(target: "helios::evm", "fetch evm state for address={:?}, slot={}", address, slot);
        self.state.get_storage(address, slot)
    }

    fn code_by_hash(&mut self, _code_hash: B256) -> Result<Bytecode, Report> {
        Err(eyre::eyre!("should never be called"))
    }
}

fn is_precompile(address: &Address) -> bool {
    address.le(&Address::from_str("0x0000000000000000000000000000000000000009").unwrap())
        && address.gt(&Address::ZERO)
}

fn convert_u256(value: &ethers::types::U256) -> U256 {
    let mut value_slice = [0u8; 32];
    value.to_big_endian(value_slice.as_mut_slice());
    U256::from_be_slice(&value_slice)
}

fn convert_address(value: &ethers::types::Address) -> Address {
    Address::from_slice(value.as_bytes())
}

#[cfg(test)]
mod tests {
    use revm::primitives::KECCAK_EMPTY;
    use tokio::sync::{mpsc::channel, watch};

    use crate::{rpc::mock_rpc::MockRpc, state::State};

    use super::*;

    fn get_client() -> ExecutionClient<MockRpc> {
        let (_, block_recv) = channel(256);
        let (_, finalized_recv) = watch::channel(None);
        let state = State::new(block_recv, finalized_recv, 64);
        ExecutionClient::new("testdata/", state).unwrap()
    }

    #[tokio::test]
    async fn test_proof_db() {
        // Construct proofdb params
        let execution = get_client();
        let tag = BlockTag::Latest;

        // Construct the proof database with the given client
        let mut proof_db = ProofDB::new(tag, Arc::new(execution));

        let address = Address::from_str("0x388C818CA8B9251b393131C08a736A67ccB19297").unwrap();
        let info = AccountInfo::new(
            U256::from(500),
            10,
            KECCAK_EMPTY,
            Bytecode::new_raw(Bytes::default()),
        );
        proof_db.state.basic.insert(address, info.clone());

        // Get the account from the proof database
        let account = proof_db.basic(address).unwrap().unwrap();

        assert_eq!(account, info);
    }
}
