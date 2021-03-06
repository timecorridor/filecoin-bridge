#![allow(dead_code)]

#[macro_use]
extern crate lazy_static;

use parking_lot::Mutex;
use std::{sync::Arc, thread, time, marker::PhantomData, collections::HashMap};
use futures::{channel::mpsc, prelude::*};
use tokio::runtime::Runtime;

use num_traits::cast::ToPrimitive;
use bridge::{PacketNonce, SuperviseClient, TokenType, TxMessage, TxSender, TxType, ChainState};
use sp_transaction_pool::{TransactionPool};
use filecoin_bridge_runtime::apis::VendorApi;
use sc_block_builder::BlockBuilderProvider;
use sc_client_api::{backend, BlockchainEvents};
use sp_api::{CallApiAt, ProvideRuntimeApi};
use sp_blockchain::HeaderBackend;
use sp_core::{sr25519, Pair};
use sp_runtime::{generic::{BlockId}, traits::{Block as BlockT}};

use lotus_api_forest::{self, Http as filecoin_http, api::ChainApi};
use interpreter::{self, BlockMessages};
use forest_blocks::{self, Tipset};
use forest_message::{self, UnsignedMessage};
use forest_address::{self, Address};
use forest_encoding::Cbor;

lazy_static! {
    pub static ref STORE_LIST: Mutex<Vec<&'static str>> = Mutex::new(vec!["test"]);
}

#[derive(Debug)]
pub struct FCMessageForward<V, B> {
    pub spv: Arc<V>,
    pub reciver: MessageStreamR<Value>,
    pub a: std::marker::PhantomData<B>,
}

impl<V, B> FCMessageForward<V, B>
where
    V: SuperviseClient<B> + Send + Sync + 'static,
    B: BlockT,
{
    pub fn new(spv: V, rec: mpsc::UnboundedReceiver<(Vec<u8>, Value)>) -> Self {
        FCMessageForward {
            spv: Arc::new(spv),
            reciver: rec,
            a: PhantomData,
        }
    }

    fn submit_tx(&self, data: TxMessage) {
        self.spv.submit_fc_transfer_tss(data);
    }

    fn start_sign_push_fc_message(self) -> impl Future<Output = ()> + 'static {
        let spv = self.spv;
        let stream = {
            self.reciver.for_each(move |(who, value)| {
                spv.submit_fc_transfer_tss(TxMessage::new(TxType::FCDeposit(
                    who,
                    TokenType::FC,
                    value,
                )));
                futures::future::ready(())
            })
        };
        stream
    }
}

type FcPubkeySender = mpsc::UnboundedReceiver<Vec<u8>>;

pub fn start_fc_service<A, B, C, Block>(
    client: Arc<C>,
    pool: Arc<A>,
    mut reciver: FcPubkeySender,
) -> impl Future<Output = ()> + 'static
where
    A: TransactionPool<Block = Block> + 'static,
    Block: BlockT,
    B: backend::Backend<Block> + Send + Sync + 'static,
    C: BlockBuilderProvider<B, Block, C>
        + HeaderBackend<Block>
        + ProvideRuntimeApi<Block>
        + BlockchainEvents<Block>
        + CallApiAt<Block>
        + Send
        + Sync
        + 'static,
    C::Api: VendorApi<Block>,
    Block::Hash: Into<sp_core::H256>,
{
    let key = sr25519::Pair::from_string(&format!("//{}", "Dave"), None)
        .expect("static values are valid; qed");

    let info = client.info();
    let at = BlockId::Hash(info.best_hash);

    let tx_sender = TxSender::new(
        client.clone(),
        pool,
        key,
        Arc::new(Mutex::new(PacketNonce {
            nonce: 0,
            last_block: at,
        })),
    );

    let (fc_parse_sender,
        fc_parse_recvier) = unbundchannel();

    let fc_message_forward =
        FCMessageForward::new(tx_sender, fc_parse_recvier);

    // loop to fetch Message from FileCoin & send to fc_sender
    fc_message_fetch_parse(fc_parse_sender, reciver,ChainState::new(client));
    // main thread to revice & parse FileCoin Message and submit to filecoin
    fc_message_forward.start_sign_push_fc_message()
}

type Value = u128;
type MessageStreamR<V> = mpsc::UnboundedReceiver<(Vec<u8>, V)>;
type MessageStreamS<V> = mpsc::UnboundedSender<(Vec<u8>, V)>;
type CidBytes = Vec<u8>;

fn extract_message(message: UnsignedMessage) -> (CidBytes, Address, Vec<u8>, u128) {

    let revice_address = message.to.clone();
    let _from_address = message.from.clone();
    let deposit_boolid = message.params.bytes().clone();
    let deposit_amount = message.value.clone().to_u128().unwrap();
    let cid = message.cid().unwrap().to_bytes();

    (cid, revice_address, deposit_boolid.into(), deposit_amount)
}

pub fn unbundchannel() -> (MessageStreamS<Value>, MessageStreamR<Value>) {
    let (sender, reciver) = mpsc::unbounded::<(Vec<u8>, Value)>();
    (sender, reciver)
}

pub fn fc_message_fetch_parse<Block,B,C>(sender: mpsc::UnboundedSender<(Vec<u8>, Value)>, _reciver: FcPubkeySender, state: ChainState<Block,B,C>)
    where
        Block: BlockT,
        B: backend::Backend<Block> + Send + Sync + 'static,
        C: BlockBuilderProvider<B, Block, C> + HeaderBackend<Block> + ProvideRuntimeApi<Block> + BlockchainEvents<Block>
        + CallApiAt<Block> + Send + Sync + 'static,
        C::Api: VendorApi<Block>,
{
    thread::spawn(move || {
        let height = 0u64;

        let mut recv_addr: Vec<u8> = Vec::new();
        loop {
            thread::sleep(time::Duration::new(30, 0));
            let pubkey = state.tss_pubkey();
            if pubkey.len() == 0{
                continue;
            }else{
                recv_addr = pubkey;
                break;
            }
        }
        println!("get recvice address {:?}", recv_addr);
        loop {
            thread::sleep(time::Duration::new(7, 0));
            let mut rt = Runtime::new().unwrap();
            let http = filecoin_http::new("http://127.0.0.1:1234/rpc/v0");
            let ret: Tipset = rt.block_on(http.chain_head()).unwrap();

            let new_height = ret.epoch() as u64;
            if new_height == height {
                continue;
            }

            let mut message_set = HashMap::<Vec<u8>, (Vec<u8>, u128)>::new();
            let cids = ret.cids();
            for cid in cids {
                println!("cids = {:?}", cid);
                let block_messages: BlockMessages =
                    rt.block_on(http.chain_get_block_messages(&cid)).unwrap();
                println!("block_messages = {:?}", block_messages);
                let signed_messages = block_messages.messages.clone();
                for message in signed_messages {
                    let (cid, revice_addr, who, val) = extract_message(message.message().clone());
                    if revice_addr == Address::new_secp256k1(&recv_addr).unwrap() {
                        message_set.insert(cid, (who, val));
                    }
                }
            }
            for (_cid, (who, val)) in message_set {
                sender.unbounded_send((who, val));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test() {
        //cargo test --color=always --package fc-signer --lib tests::test -- --exact --nocapture
        let mut rt = Runtime::new().unwrap();
        let http = filecoin_http::Http::new("http://127.0.0.1:1234/rpc/v0");
        let ret: Tipset = rt.block_on(http.chain_head()).unwrap();
        let cids = ret.cids.clone();
        let ret = rt
            .block_on(http.chain_get_block_messages(&cids[0]))
            .unwrap();

        for cid in cids {
            println!("cids = {:?}", cid);
            let bm: BlockMessages = rt.block_on(http.chain_get_block_messages(&cid)).unwrap();
            println!("block_messages = {:?}", bm);
            let signed_messages = bm.messages.clone();
            for message in signed_messages {
                //let (who,val) = extract_message(message.message);
            }
        }
    }
}
