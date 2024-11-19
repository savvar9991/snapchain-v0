use super::shard::ShardStore;
use crate::core::types::{proto, Height};
use crate::proto::snapchain::{Block, ShardChunk, Transaction};
use crate::proto::{message, snapchain};
use crate::storage::db::{RocksDB, RocksDbTransactionBatch};
use crate::storage::store::BlockStore;
use crate::storage::trie::merkle_trie;
use std::iter;
use tokio::sync::mpsc;
use tracing::{error, warn};

// Shard state root and the transactions
#[derive(Clone)]
pub struct ShardStateChange {
    pub shard_id: u32,
    pub new_state_root: Vec<u8>,
    pub transactions: Vec<proto::Transaction>,
}

pub struct ShardEngine {
    shard_id: u32,
    shard_store: ShardStore,
    messages_rx: mpsc::Receiver<message::Message>,
    messages_tx: mpsc::Sender<message::Message>,
    trie: merkle_trie::MerkleTrie,
}

fn encode_vec(data: &[Vec<u8>]) -> String {
    data.iter()
        .map(|vec| hex::encode(vec))
        .collect::<Vec<String>>()
        .join(", ")
}

impl ShardEngine {
    pub fn new(shard_id: u32, shard_store: ShardStore) -> ShardEngine {
        let db = &*shard_store.db;

        // TODO: adding the trie here introduces many calls that want to return errors. Rethink unwrap strategy.
        let mut txn_batch = RocksDbTransactionBatch::new();
        let mut trie = merkle_trie::MerkleTrie::new().unwrap();
        trie.initialize(db, &mut txn_batch).unwrap();

        // TODO: The empty trie currently has some issues with the newly added commit/rollback code. Remove when we can.
        trie.insert(db, &mut txn_batch, vec![vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]])
            .unwrap();
        db.commit(txn_batch).unwrap();
        trie.reload(db).unwrap();

        let (messages_tx, messages_rx) = mpsc::channel::<message::Message>(10_000);
        ShardEngine {
            shard_id,
            shard_store,
            messages_rx,
            messages_tx,
            trie,
        }
    }

    pub fn messages_tx(&self) -> mpsc::Sender<message::Message> {
        self.messages_tx.clone()
    }

    pub(crate) fn trie_root_hash(&self) -> Vec<u8> {
        self.trie.root_hash().unwrap()
    }

    pub fn propose_state_change(&mut self, shard: u32) -> ShardStateChange {
        //TODO: return Result instead of .unwrap() ?
        let it = iter::from_fn(|| self.messages_rx.try_recv().ok());
        let user_messages: Vec<message::Message> = it.collect();

        let mut hashes: Vec<Vec<u8>> = vec![];
        for msg in &user_messages {
            hashes.push(msg.hash.clone());
        }

        let transactions = vec![snapchain::Transaction {
            fid: 1234,                      //TODO
            account_root: vec![5, 5, 6, 6], //TODO
            system_messages: vec![],        //TODO
            user_messages,
        }];

        let db = &*self.shard_store.db;

        let mut txn_batch = RocksDbTransactionBatch::new();
        self.trie.insert(db, &mut txn_batch, hashes);
        let new_root_hash = self.trie.root_hash().unwrap();

        let result = ShardStateChange {
            shard_id: shard,
            new_state_root: new_root_hash.clone(),
            transactions,
        };

        // TODO: this should probably operate automatically via drop trait
        self.trie.reload(db).unwrap();

        result
    }

    fn replay(
        trie: &mut merkle_trie::MerkleTrie,
        db: &RocksDB,
        txn_batch: &mut RocksDbTransactionBatch,
        transactions: &[Transaction],
        shard_root: &[u8],
    ) -> bool {
        let mut hashes: Vec<Vec<u8>> = vec![];
        for msg in &transactions[0].user_messages {
            hashes.push(msg.hash.clone());
        }

        trie.insert(db, txn_batch, hashes).unwrap();
        let root1 = trie.root_hash().unwrap();

        &root1 == shard_root
    }

    pub fn validate_state_change(&mut self, shard_state_change: &ShardStateChange) -> bool {
        let mut txn = RocksDbTransactionBatch::new();

        let transactions = &shard_state_change.transactions;
        let shard_root = &shard_state_change.new_state_root;

        let hashes_match = {
            let db = &*self.shard_store.db;
            Self::replay(&mut self.trie, db, &mut txn, transactions, shard_root)
        };

        self.trie.reload(&*self.shard_store.db).unwrap();

        hashes_match
    }

    pub fn commit_shard_chunk(&mut self, shard_chunk: ShardChunk) {
        let mut txn = RocksDbTransactionBatch::new();

        let shard_root = &shard_chunk.header.as_ref().unwrap().shard_root;
        let transactions = &shard_chunk.transactions;

        let hashes_match = {
            let db = &*self.shard_store.db;
            Self::replay(&mut self.trie, db, &mut txn, transactions, &shard_root)
        };

        if hashes_match {
            let db = &*self.shard_store.db;
            db.commit(txn).unwrap();
        } else {
            error!("panic!");
            panic!("hashes don't match");
        }

        self.trie.reload(&*self.shard_store.db).unwrap();

        match self.shard_store.put_shard_chunk(shard_chunk) {
            Err(err) => {
                error!("Unable to write shard chunk to store {}", err)
            }
            Ok(()) => {}
        }
    }

    pub fn get_confirmed_height(&self) -> Height {
        match self.shard_store.max_block_number() {
            Ok(block_num) => Height::new(self.shard_id, block_num),
            Err(_) => Height::new(self.shard_id, 0),
        }
    }

    pub fn get_last_shard_chunk(&self) -> Option<ShardChunk> {
        match self.shard_store.get_last_shard_chunk() {
            Ok(shard_chunk) => shard_chunk,
            Err(err) => {
                error!("Unable to obtain last shard chunk {:#?}", err);
                None
            }
        }
    }
}

pub struct BlockEngine {
    block_store: BlockStore,
}

impl BlockEngine {
    pub fn new(block_store: BlockStore) -> Self {
        BlockEngine { block_store }
    }

    pub fn commit_block(&mut self, block: Block) {
        let result = self.block_store.put_block(block);
        if result.is_err() {
            error!("Failed to store block: {:?}", result.err());
        }
    }

    pub fn get_last_block(&self) -> Option<Block> {
        match self.block_store.get_last_block() {
            Ok(block) => block,
            Err(err) => {
                error!("Unable to obtain last block {:#?}", err);
                None
            }
        }
    }

    pub fn get_confirmed_height(&self) -> Height {
        let shard_index = 0;
        match self.block_store.max_block_number() {
            Ok(block_num) => Height::new(shard_index, block_num),
            Err(_) => Height::new(shard_index, 0),
        }
    }
}
