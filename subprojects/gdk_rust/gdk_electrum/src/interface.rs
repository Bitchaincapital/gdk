use bitcoin::blockdata::script::{Builder, Script};
use bitcoin::blockdata::transaction::{OutPoint, Transaction, TxIn, TxOut};
use bitcoin::hash_types::PubkeyHash;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::{All, Message, Secp256k1};
use bitcoin::util::address::Address;
use bitcoin::util::bip143::SighashComponents;
use bitcoin::util::bip32::{ChildNumber, DerivationPath, ExtendedPrivKey, ExtendedPubKey};
use bitcoin::{PublicKey, Txid};
use electrum_client::GetHistoryRes;
use elements::{self, AddressParams};
use hex;
use log::debug;
use rand::Rng;
use std::time::Instant;

use gdk_common::mnemonic::Mnemonic;
use gdk_common::model::{CreateTransaction, Settings, TransactionMeta};
use gdk_common::network::{ElementsNetwork, Network, NetworkId};
use gdk_common::util::p2shwpkh_script;
use gdk_common::wally::*;

use crate::db::*;
use crate::error::*;
use crate::model::*;
use electrum_client::Client;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::PathBuf;

pub struct WalletCtx {
    secp: Secp256k1<All>,
    network: Network,
    mnemonic: Mnemonic,
    db: Forest,
    xprv: ExtendedPrivKey,
    xpub: ExtendedPubKey,
    master_blinding: Option<MasterBlindingKey>,
    change_max_deriv: u32,
}

#[derive(Debug)]
pub enum LiqOrBitAddress {
    Liquid(elements::Address),
    Bitcoin(bitcoin::Address),
}

impl LiqOrBitAddress {
    pub fn script_pubkey(&self) -> Script {
        match self {
            LiqOrBitAddress::Liquid(addr) => addr.script_pubkey(),
            LiqOrBitAddress::Bitcoin(addr) => addr.script_pubkey(),
        }
    }
}

impl ToString for LiqOrBitAddress {
    fn to_string(&self) -> String {
        match self {
            LiqOrBitAddress::Liquid(addr) => addr.to_string(),
            LiqOrBitAddress::Bitcoin(addr) => addr.to_string(),
        }
    }
}

pub enum ElectrumUrl {
    Tls(String),
    Plaintext(String),
}

impl WalletCtx {
    pub fn new(
        db_root: &str,
        wallet_name: String,
        mnemonic: Mnemonic,
        network: Network,
        xprv: ExtendedPrivKey,
        xpub: ExtendedPubKey,
        master_blinding: Option<MasterBlindingKey>,
    ) -> Result<Self, Error> {
        let mut path: PathBuf = db_root.into();
        path.push(wallet_name);
        debug!("opening sled db root path: {:?}", path);

        let db = Forest::new(path, xpub)?;

        Ok(WalletCtx {
            mnemonic,
            db,
            network, // TODO: from db
            secp: Secp256k1::gen_new(),
            xprv,
            xpub,
            master_blinding,
            change_max_deriv: 0,
        })
    }

    pub fn get_mnemonic(&self) -> &Mnemonic {
        &self.mnemonic
    }

    fn derive_address(
        &self,
        xpub: &ExtendedPubKey,
        path: &[u32; 2],
    ) -> Result<LiqOrBitAddress, Error> {
        let path: Vec<ChildNumber> = path
            .iter()
            .map(|x| ChildNumber::Normal {
                index: *x,
            })
            .collect();
        let derived = xpub.derive_pub(&self.secp, &path)?;
        if self.network.liquid {}
        match self.network.id() {
            NetworkId::Bitcoin(network) => {
                Ok(LiqOrBitAddress::Bitcoin(Address::p2shwpkh(&derived.public_key, network)))
            }
            NetworkId::Elements(network) => {
                let master_blinding_key = self
                    .master_blinding
                    .as_ref()
                    .expect("we are in elements but master blinding is None");
                let script = p2shwpkh_script(&derived.public_key);
                let blinding_key =
                    asset_blinding_key_to_ec_private_key(&master_blinding_key, &script);
                let public_key = ec_public_key_from_private_key(blinding_key);
                let blinder = Some(public_key);
                let addr = match network {
                    ElementsNetwork::Liquid => elements::Address::p2shwpkh(
                        &derived.public_key,
                        blinder,
                        &AddressParams::LIQUID,
                    ),
                    ElementsNetwork::ElementsRegtest => elements::Address::p2shwpkh(
                        &derived.public_key,
                        blinder,
                        &AddressParams::ELEMENTS,
                    ),
                };
                Ok(LiqOrBitAddress::Liquid(addr))
            }
        }
    }

    pub fn get_settings(&self) -> Result<Settings, Error> {
        Ok(self.db.get_settings()?.unwrap_or_default())
    }

    pub fn change_settings(&self, settings: &Settings) -> Result<(), Error> {
        self.db.insert_settings(settings)
    }

    pub fn sync<S: Read + Write>(&self, client: &mut Client<S>) -> Result<(), Error> {
        debug!("start sync");
        let start = Instant::now();

        //let mut client = Client::new("tn.not.fyi:55001")?;
        let mut history_txs_id = HashSet::new();
        let mut heights_set = HashSet::new();
        let mut txid_height = HashMap::new();
        let network =
            self.network.id().get_bitcoin_network().ok_or_else(fn_err("bitcoin network empty"))?;

        let mut last_used = [0u32; 2];
        for i in 0..=1 {
            let int_or_ext = Index::from(i)?;
            let mut batch_count = 0;
            loop {
                let batch = self.db.get_script_batch(int_or_ext, batch_count, network)?;
                let result: Vec<Vec<GetHistoryRes>> = client.batch_script_get_history(&batch)?;
                let max = result
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| !v.is_empty())
                    .map(|(i, _)| i as u32)
                    .max();
                if let Some(max) = max {
                    last_used[i as usize] = max + batch_count * BATCH_SIZE;
                };

                let flattened: Vec<GetHistoryRes> = result.into_iter().flatten().collect();
                debug!("{}/batch({}) {:?}", i, batch_count, flattened.len());

                if flattened.is_empty() {
                    break;
                }

                for el in flattened {
                    if el.height >= 0 {
                        heights_set.insert(el.height as u32);
                        txid_height.insert(el.tx_hash, el.height as u32);
                    }
                    history_txs_id.insert(el.tx_hash);
                }

                batch_count += 1;
            }
        }
        self.db.insert_index(Index::External, last_used[Index::External as usize])?;
        self.db.insert_index(Index::Internal, last_used[Index::Internal as usize])?;
        debug!("last_used: {:?}", last_used,);

        let mut txs_in_db = self.db.get_all_txid()?;
        let txs_to_download: Vec<&Txid> = history_txs_id.difference(&txs_in_db).collect();
        if !txs_to_download.is_empty() {
            let txs_downloaded = client.batch_transaction_get(txs_to_download)?;
            debug!("txs_downloaded {:?}", txs_downloaded.len());
            let mut previous_txs_to_download = HashSet::new();
            for tx in txs_downloaded.iter() {
                self.db.insert_tx(&tx.txid(), &tx)?;
                txs_in_db.insert(tx.txid());
                for input in tx.input.iter() {
                    previous_txs_to_download.insert(input.previous_output.txid);
                }
            }
            let txs_to_download: Vec<&Txid> =
                previous_txs_to_download.difference(&txs_in_db).collect();
            if !txs_to_download.is_empty() {
                let txs_downloaded = client.batch_transaction_get(txs_to_download)?;
                debug!("previous txs_downloaded {:?}", txs_downloaded.len());
                for tx in txs_downloaded.iter() {
                    self.db.insert_tx(&tx.txid(), tx)?;
                }
            }
        }

        let heights_in_db = self.db.get_only_heights()?;
        let heights_to_download: Vec<u32> =
            heights_set.difference(&heights_in_db).cloned().collect();
        if !heights_to_download.is_empty() {
            let headers_downloaded = client.batch_block_header(heights_to_download.clone())?;
            for (header, height) in headers_downloaded.iter().zip(heights_to_download.iter()) {
                self.db.insert_header(*height, header)?;
            }
            debug!("headers_downloaded {:?}", headers_downloaded.len());
        }

        // sync heights, which are my txs
        for (txid, height) in txid_height.iter() {
            self.db.insert_height(txid, *height)?; // adding new, but also updating reorged tx
        }
        for txid_db in self.db.get_only_txids()?.iter() {
            if txid_height.get(txid_db).is_none() {
                self.db.remove_height(txid_db)?; // something in the db is not in live list (rbf), removing
            }
        }

        debug!("elapsed {}", start.elapsed().as_millis());

        Ok(())
    }

    pub fn list_tx(&self) -> Result<Vec<TransactionMeta>, Error> {
        debug!("start list_tx");
        let (_, all_txs) = self.db.get_all_spent_and_txs()?;
        let mut txs = vec![];

        for (tx_id, height) in self.db.get_my()? {
            let tx = all_txs.get(&tx_id).ok_or_else(fn_err("no tx"))?;
            let header = height
                .map(|h| self.db.get_header(h)?.ok_or_else(fn_err("no header")))
                .transpose()?;
            let total_output: u64 = tx.output.iter().map(|o| o.value).sum();
            let total_input: u64 = tx
                .input
                .iter()
                .filter_map(|i| all_txs.get_previous_value(&i.previous_output))
                .sum();
            let fee = total_input - total_output;
            let received: u64 = tx
                .output
                .iter()
                .filter(|o| self.db.is_mine(&o.script_pubkey))
                .map(|o| o.value)
                .sum();
            let sent: u64 = tx
                .input
                .iter()
                .filter_map(|i| all_txs.get_previous_output(&i.previous_output))
                .filter(|o| self.db.is_mine(&o.script_pubkey))
                .map(|o| o.value)
                .sum();

            let tx_meta = TransactionMeta::new(
                tx.clone(),
                height,
                header.map(|h| h.time),
                received,
                sent,
                fee,
                self.network.id().get_bitcoin_network().unwrap_or(bitcoin::Network::Bitcoin),
            );

            txs.push(tx_meta);
        }
        txs.sort_by(|a, b| {
            b.height.unwrap_or(std::u32::MAX).cmp(&a.height.unwrap_or(std::u32::MAX))
        });
        Ok(txs)
    }

    fn utxos(&self) -> Result<Vec<(OutPoint, TxOut)>, Error> {
        debug!("start utxos");
        let (spent, all_txs) = self.db.get_all_spent_and_txs()?;
        let mut utxos = vec![];
        for tx_id in self.db.get_only_txids()? {
            let tx = all_txs.get(&tx_id).ok_or_else(fn_err("no tx"))?;
            let tx_utxos: Vec<(OutPoint, TxOut)> = tx
                .output
                .clone()
                .into_iter()
                .enumerate()
                .map(|(vout, output)| (OutPoint::new(tx.txid(), vout as u32), output))
                .filter(|(_, output)| self.db.is_mine(&output.script_pubkey))
                .filter(|(outpoint, _)| !spent.contains(&outpoint))
                .collect();
            utxos.extend(tx_utxos);
        }
        utxos.sort_by(|a, b| b.1.value.cmp(&a.1.value));
        Ok(utxos)
    }

    pub fn balance(&self) -> Result<u64, Error> {
        debug!("start balance");
        Ok(self.utxos()?.iter().fold(0, |sum, i| sum + i.1.value))
    }

    // If request.utxo is None, we do the coin selection
    pub fn create_tx(&self, request: &CreateTransaction) -> Result<TransactionMeta, Error> {
        debug!("create_tx {:?}", request);
        use bitcoin::consensus::serialize;

        let mut tx = Transaction {
            version: 2,
            lock_time: 0,
            input: vec![],
            output: vec![],
        };

        let fee_rate = (request.fee_rate.unwrap_or(1000) as f64) / 1000.0 * 1.3; //TODO 30% increase hack because we compute fee badly

        let mut fee_val = 0;
        let mut outgoing: u64 = 0;
        let mut is_mine = vec![];

        let calc_fee_bytes = |bytes| ((bytes as f64) * fee_rate) as u64;
        fee_val += calc_fee_bytes(tx.get_weight() / 4);

        for out in request.addressees.iter() {
            let new_out = TxOut {
                script_pubkey: out.address.script_pubkey(),
                value: out.satoshi,
            };
            fee_val += calc_fee_bytes(serialize(&new_out).len());

            tx.output.push(new_out);
            is_mine.push(false);

            outgoing += out.satoshi;
        }

        let mut utxos = self.utxos()?;
        debug!("utxos len:{}", utxos.len());

        let mut selected_amount: u64 = 0;
        while selected_amount < outgoing + fee_val {
            debug!("selected_amount:{} outgoing:{} fee_val:{}", selected_amount, outgoing, fee_val);
            let (outpoint, txout) = utxos.pop().ok_or(Error::InsufficientFunds)?;

            let new_in = TxIn {
                previous_output: outpoint,
                script_sig: Script::default(),
                sequence: 0,
                witness: vec![],
            };
            fee_val += calc_fee_bytes(serialize(&new_in).len() + 50); // TODO: adjust 50 based on the signature size

            tx.input.push(new_in);

            selected_amount += txout.value;
        }

        let change_val = selected_amount - outgoing - fee_val;
        if change_val > 546 {
            let change_index = self.db.increment_index(Index::Internal)?;
            let change_address = self.derive_address(&self.xpub, &[1, change_index])?;
            debug!("adding change {:?}", change_address);

            // TODO: we are not accounting for this output
            tx.output.push(TxOut {
                script_pubkey: change_address.script_pubkey(),
                value: change_val,
            });

            is_mine.push(true);
        }
        let mut created_tx = TransactionMeta::new(
            tx,
            None,
            None,
            0,
            outgoing,
            fee_val,
            self.network.id().get_bitcoin_network().unwrap_or(bitcoin::Network::Bitcoin),
        );
        created_tx.create_transaction = Some(request.clone());
        created_tx.sent = Some(outgoing);
        created_tx.satoshi = outgoing;
        debug!("returning: {:?}", created_tx);

        Ok(created_tx)
    }

    // TODO when we can serialize psbt
    //pub fn sign(&self, psbt: PartiallySignedTransaction) -> Result<PartiallySignedTransaction, Error> { Err(Error::Generic("NotImplemented".to_string())) }

    fn internal_sign(
        &self,
        tx: &Transaction,
        input_index: usize,
        path: &DerivationPath,
        value: u64,
    ) -> (PublicKey, Vec<u8>) {
        let privkey = self.xprv.derive_priv(&self.secp, &path).unwrap();
        let pubkey = ExtendedPubKey::from_private(&self.secp, &privkey);

        let witness_script = Address::p2pkh(&pubkey.public_key, pubkey.network).script_pubkey();

        let hash =
            SighashComponents::new(tx).sighash_all(&tx.input[input_index], &witness_script, value);

        let signature = self
            .secp
            .sign(&Message::from_slice(&hash.into_inner()[..]).unwrap(), &privkey.private_key.key);

        //let mut signature = signature.serialize_der().to_vec();
        let mut signature = hex::decode(&format!("{:?}", signature)).unwrap();
        signature.push(0x01 as u8); // TODO how to properly do this?

        (pubkey.public_key, signature)
    }

    pub fn sign(&self, request: &TransactionMeta) -> Result<TransactionMeta, Error> {
        debug!("sign");
        let mut out_tx = request.transaction.clone();

        for i in 0..request.transaction.input.len() {
            let prev_output = request.transaction.input[i].previous_output.clone();
            debug!("input#{} prev_output:{:?}", i, prev_output);
            let tx = self
                .db
                .get_tx(&prev_output.txid)?
                .ok_or_else(|| Error::Generic("cannot find tx in db".into()))?;
            let out = tx.output[prev_output.vout as usize].clone();
            let derivation_path = self
                .db
                .get_path(&out.script_pubkey)?
                .ok_or_else(|| Error::Generic("can't find derivation path".into()))?
                .to_derivation_path()?;
            debug!(
                "input#{} prev_output:{:?} derivation_path:{:?}",
                i, prev_output, derivation_path
            );

            let (pk, sig) =
                self.internal_sign(&request.transaction, i, &derivation_path, out.value);
            let script_sig = script_sig(&pk);
            let witness = vec![sig, pk.to_bytes()];

            out_tx.input[i].script_sig = script_sig;
            out_tx.input[i].witness = witness;
        }

        let wgtx: TransactionMeta = out_tx.into();

        Ok(wgtx)
    }

    pub fn validate_address(&self, _address: Address) -> Result<bool, Error> {
        // if we managed to get here it means that the address is already valid.
        // only other thing we can check is if it the network is right.

        // TODO implement for both Liquid and Bitcoin address
        //Ok(address.network == self.network)
        unimplemented!("validate not implemented");
    }

    pub fn poll(&self, _xpub: WGExtendedPubKey) -> Result<(), Error> {
        Ok(())
    }

    pub fn get_address(&self) -> Result<WGAddress, Error> {
        debug!("get_address");
        let index = self.db.increment_index(Index::External)?;
        let address = self.derive_address(&self.xpub, &[0, index])?.to_string();
        Ok(WGAddress {
            address,
        })
    }
    pub fn xpub_from_xprv(&self, xprv: WGExtendedPrivKey) -> Result<WGExtendedPubKey, Error> {
        Ok(WGExtendedPubKey {
            xpub: ExtendedPubKey::from_private(&self.secp, &xprv.xprv),
        })
    }

    pub fn generate_xprv(&self) -> Result<WGExtendedPrivKey, Error> {
        let random_bytes = rand::thread_rng().gen::<[u8; 32]>();

        Ok(WGExtendedPrivKey {
            xprv: ExtendedPrivKey::new_master(
                self.network.id().get_bitcoin_network().unwrap(),
                &random_bytes,
            )?, // TODO support LIQUID
        })
    }
}

fn script_sig(public_key: &PublicKey) -> Script {
    let internal = Builder::new()
        .push_int(0)
        .push_slice(&PubkeyHash::hash(&public_key.to_bytes())[..])
        .into_script();
    Builder::new().push_slice(internal.as_bytes()).into_script()
}

#[cfg(test)]
mod test {
    use crate::interface::script_sig;
    use bitcoin::consensus::deserialize;
    use bitcoin::hashes::hash160;
    use bitcoin::hashes::Hash;
    use bitcoin::secp256k1::{All, Message, Secp256k1, SecretKey};
    use bitcoin::util::bip143::SighashComponents;
    use bitcoin::util::bip32::{ChildNumber, ExtendedPrivKey, ExtendedPubKey};
    use bitcoin::util::key::PrivateKey;
    use bitcoin::util::key::PublicKey;
    use bitcoin::Script;
    use bitcoin::{Address, Network, Transaction};
    use std::str::FromStr;

    fn p2pkh_hex(pk: &str) -> (PublicKey, Script) {
        let pk = hex::decode(pk).unwrap();
        let pk = PublicKey::from_slice(pk.as_slice()).unwrap();
        let witness_script = Address::p2pkh(&pk, Network::Bitcoin).script_pubkey();
        (pk, witness_script)
    }

    #[test]
    fn test_bip() {
        let secp: Secp256k1<All> = Secp256k1::gen_new();

        // https://github.com/bitcoin/bips/blob/master/bip-0143.mediawiki#p2sh-p2wpkh
        let tx_bytes = hex::decode("0100000001db6b1b20aa0fd7b23880be2ecbd4a98130974cf4748fb66092ac4d3ceb1a54770100000000feffffff02b8b4eb0b000000001976a914a457b684d7f0d539a46a45bbc043f35b59d0d96388ac0008af2f000000001976a914fd270b1ee6abcaea97fea7ad0402e8bd8ad6d77c88ac92040000").unwrap();
        let tx: Transaction = deserialize(&tx_bytes).unwrap();

        let private_key_bytes =
            hex::decode("eb696a065ef48a2192da5b28b694f87544b30fae8327c4510137a922f32c6dcf")
                .unwrap();

        let key = SecretKey::from_slice(&private_key_bytes).unwrap();
        let private_key = PrivateKey {
            compressed: true,
            network: Network::Testnet,
            key,
        };

        let (public_key, witness_script) =
            p2pkh_hex("03ad1d8e89212f0b92c74d23bb710c00662ad1470198ac48c43f7d6f93a2a26873");
        assert_eq!(
            hex::encode(witness_script.to_bytes()),
            "76a91479091972186c449eb1ded22b78e40d009bdf008988ac"
        );
        let value = 1_000_000_000;
        let comp = SighashComponents::new(&tx);
        let hash = comp.sighash_all(&tx.input[0], &witness_script, value).into_inner();

        assert_eq!(
            &hash[..],
            &hex::decode("64f3b0f4dd2bb3aa1ce8566d220cc74dda9df97d8490cc81d89d735c92e59fb6")
                .unwrap()[..],
        );

        let signature = secp.sign(&Message::from_slice(&hash[..]).unwrap(), &private_key.key);

        //let mut signature = signature.serialize_der().to_vec();
        let signature_hex = format!("{:?}01", signature); // add sighash type at the end
        assert_eq!(signature_hex, "3044022047ac8e878352d3ebbde1c94ce3a10d057c24175747116f8288e5d794d12d482f0220217f36a485cae903c713331d877c1f64677e3622ad4010726870540656fe9dcb01");

        let script_sig = script_sig(&public_key);

        assert_eq!(
            format!("{}", hex::encode(script_sig.as_bytes())),
            "16001479091972186c449eb1ded22b78e40d009bdf0089"
        );
    }

    #[test]
    fn test_my_tx() {
        let secp: Secp256k1<All> = Secp256k1::gen_new();
        let xprv = ExtendedPrivKey::from_str("tprv8jdzkeuCYeH5hi8k2JuZXJWV8sPNK62ashYyUVD9Euv5CPVr2xUbRFEM4yJBB1yBHZuRKWLeWuzH4ptmvSgjLj81AvPc9JhV4i8wEfZYfPb").unwrap();
        let xpub = ExtendedPubKey::from_private(&secp, &xprv);
        let private_key = xprv.private_key;
        let public_key = xpub.public_key;
        let public_key_bytes = public_key.to_bytes();
        let public_key_str = format!("{}", hex::encode(&public_key_bytes));

        let address = Address::p2shwpkh(&public_key, Network::Testnet);
        assert_eq!(format!("{}", address), "2NCEMwNagVAbbQWNfu7M7DNGxkknVTzhooC");

        assert_eq!(
            public_key_str,
            "0386fe0922d694cef4fa197f9040da7e264b0a0ff38aa2e647545e5a6d6eab5bfc"
        );
        let tx_hex = "020000000001010e73b361dd0f0320a33fd4c820b0c7ac0cae3b593f9da0f0509cc35de62932eb01000000171600141790ee5e7710a06ce4a9250c8677c1ec2843844f0000000002881300000000000017a914cc07bc6d554c684ea2b4af200d6d988cefed316e87a61300000000000017a914fda7018c5ee5148b71a767524a22ae5d1afad9a9870247304402206675ed5fb86d7665eb1f7950e69828d0aa9b41d866541cedcedf8348563ba69f022077aeabac4bd059148ff41a36d5740d83163f908eb629784841e52e9c79a3dbdb01210386fe0922d694cef4fa197f9040da7e264b0a0ff38aa2e647545e5a6d6eab5bfc00000000";

        let tx_bytes = hex::decode(tx_hex).unwrap();
        let tx: Transaction = deserialize(&tx_bytes).unwrap();

        let (_, witness_script) = p2pkh_hex(&public_key_str);
        assert_eq!(
            hex::encode(witness_script.to_bytes()),
            "76a9141790ee5e7710a06ce4a9250c8677c1ec2843844f88ac"
        );
        let value = 10_202;
        let comp = SighashComponents::new(&tx);
        let hash = comp.sighash_all(&tx.input[0], &witness_script, value);

        assert_eq!(
            &hash.into_inner()[..],
            &hex::decode("58b15613fc1701b2562430f861cdc5803531d08908df531082cf1828cd0b8995")
                .unwrap()[..],
        );

        let signature = secp.sign(&Message::from_slice(&hash[..]).unwrap(), &private_key.key);

        //let mut signature = signature.serialize_der().to_vec();
        let signature_hex = format!("{:?}01", signature); // add sighash type at the end
        let signature = hex::decode(&signature_hex).unwrap();

        assert_eq!(signature_hex, "304402206675ed5fb86d7665eb1f7950e69828d0aa9b41d866541cedcedf8348563ba69f022077aeabac4bd059148ff41a36d5740d83163f908eb629784841e52e9c79a3dbdb01");
        assert_eq!(tx.input[0].witness[0], signature);
        assert_eq!(tx.input[0].witness[1], public_key_bytes);

        let script_sig = script_sig(&public_key);
        assert_eq!(tx.input[0].script_sig, script_sig);
    }
}
