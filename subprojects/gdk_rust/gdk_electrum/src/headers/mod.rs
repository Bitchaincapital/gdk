use crate::error::Error;
use crate::headers::bitcoin::HeadersChain;
use crate::headers::liquid::Verifier;
use ::bitcoin::hashes::{sha256d, Hash};
use ::bitcoin::hashes::hex::FromHex;
use ::bitcoin::{TxMerkleNode, Txid};
use electrum_client::GetMerkleRes;
use std::io::Write;
use gdk_common::model::{SPVVerifyTx, SPVVerifyResult};
use gdk_common::NetworkId;
use std::path::PathBuf;
use log::info;

use crate::{determine_electrum_url_from_net, ClientWrap};

pub mod bitcoin;
pub mod liquid;

pub enum ChainOrVerifier {
    Chain(HeadersChain),
    Verifier(Verifier),
}

fn compute_merkle_root(txid: &Txid, merkle: GetMerkleRes) -> Result<TxMerkleNode, Error> {
    let mut pos = merkle.pos;
    let mut current = txid.into_inner();

    for mut hash in merkle.merkle {
        let mut engine = sha256d::Hash::engine();
        hash.reverse();
        if pos % 2 == 0 {
            engine.write(&current)?;
            engine.write(&hash)?;
        } else {
            engine.write(&hash)?;
            engine.write(&current)?;
        }
        current = sha256d::Hash::from_engine(engine).into_inner();
        pos /= 2;
    }

    Ok(TxMerkleNode::from_slice(&current)?)
}

pub fn spv_verify_tx(input: &SPVVerifyTx) -> Result<SPVVerifyResult, Error> {
    info!("spv_verify_tx {:?}", input);
    let txid = Txid::from_hex(&input.txid)?;
    let url = determine_electrum_url_from_net(&input.network)?;
    let mut client = ClientWrap::new(url)?;

    match input.network.id() {
        NetworkId::Bitcoin(bitcoin_network) => {
            let mut path: PathBuf = (&input.path).into();
            path.push(format!("headers_chain_{}", bitcoin_network));
            let mut chain = HeadersChain::new(path, bitcoin_network)?;

            if input.height < chain.height() {
                let proof = client.transaction_get_merkle(&txid, input.height as usize)?;
                if chain.verify_tx_proof(&txid, input.height, proof).is_ok() {
                    Ok(SPVVerifyResult::Verified)
                } else {
                    Ok(SPVVerifyResult::NotVerified)
                }
            } else {
                let headers_to_download = input.headers_to_download.unwrap_or(2016).min(2016);
                let headers = client.block_headers(chain.height() as usize + 1, headers_to_download)?.headers;
                chain.push(headers)?;
                Ok(SPVVerifyResult::CallMeAgain)
            }
        }
        NetworkId::Elements(elements_network) => {
            let proof = client.transaction_get_merkle(&txid, input.height as usize)?;
            let verifier = Verifier::new(elements_network);
            let header_bytes = client.block_header_raw(input.height as usize)?;
            let header : elements::BlockHeader = elements::encode::deserialize(&header_bytes)?;
            if verifier.verify_tx_proof(&txid, proof, &header).is_ok() {
                Ok(SPVVerifyResult::Verified)
            } else {
                Ok(SPVVerifyResult::NotVerified)
            }
        }
    }

}
