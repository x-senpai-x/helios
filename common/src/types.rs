use std::fmt::Display;

use ethers_core::types::{Address, Bytes, Transaction, H256, U256, U64};
use serde::{de::Error, ser::SerializeSeq, Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Block {
    pub number: U64,
    pub base_fee_per_gas: U256,
    pub difficulty: U256,
    pub extra_data: Bytes,
    pub gas_limit: U64,
    pub gas_used: U64,
    pub hash: H256,
    pub logs_bloom: Bytes,
    pub miner: Address,
    pub mix_hash: H256,
    pub nonce: String,
    pub parent_hash: H256,
    pub receipts_root: H256,
    pub sha3_uncles: H256,
    pub size: U64,
    pub state_root: H256,
    pub timestamp: U64,
    pub total_difficulty: U64,
    pub transactions: Transactions,
    pub transactions_root: H256,
    pub uncles: Vec<H256>,
}

#[derive(Deserialize, Debug, Clone)]
pub enum Transactions {//holds transaction hashes or full data of transactions
    Hashes(Vec<H256>),
    Full(Vec<Transaction>),//vector that contains transaction elements
    
}


impl Transactions {
    pub fn hashes(&self) -> Vec<H256> {
        match self {
            Self::Hashes(hashes) => hashes.clone(),
            Self::Full(txs) => txs.iter().map(|tx| tx.hash).collect(),//txs is a vector of transactions and we are mapping each transaction to its hash
            //i.e if Transactions enum contains hashes then directly hash is returned else we are mapping each transaction to its hash
            //so hashes fn returns a list of hashes 
        }
    }
}

impl Serialize for Transactions {//serializing transactions based on whether they contain hashes or full data
    fn serialize<S>(&self, s: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Transactions::Hashes(hashes) => {
                let mut seq = s.serialize_seq(Some(hashes.len()))?;
                for hash in hashes {
                    seq.serialize_element(&hash)?;
                }

                seq.end()
            }
            Transactions::Full(txs) => {
                let mut seq = s.serialize_seq(Some(txs.len()))?;
                for tx in txs {
                    seq.serialize_element(&tx)?;
                }

                seq.end()
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum BlockTag {//different ways to refer to block 
    Latest,//most recent block
    Finalized,//last finalized block
    Number(u64),
}

impl Display for BlockTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let formatted = match self {
            Self::Latest => "latest".to_string(),
            Self::Finalized => "finalized".to_string(),
            Self::Number(num) => num.to_string(),
        };

        write!(f, "{formatted}")
    }
}

impl<'de> Deserialize<'de> for BlockTag {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let block: String = serde::Deserialize::deserialize(deserializer)?;
        let parse_error = D::Error::custom("could not parse block tag");

        let block_tag = match block.as_str() {
            "latest" => BlockTag::Latest,
            "finalized" => BlockTag::Finalized,
            _ => match block.strip_prefix("0x") {
                Some(hex_block) => {
                    let num = u64::from_str_radix(hex_block, 16).map_err(|_| parse_error)?;

                    BlockTag::Number(num)
                }
                None => {
                    let num = block.parse().map_err(|_| parse_error)?;

                    BlockTag::Number(num)
                }
            },
        };

        Ok(block_tag)
    }
}
