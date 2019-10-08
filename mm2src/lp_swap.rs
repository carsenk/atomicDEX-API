//! Atomic swap loops and states
//! 
//! # A note on the terminology used
//! 
//! Alice = Buyer = Liquidity receiver = Taker  
//! ("*The process of an atomic swap begins with the person who makes the initial request — this is the liquidity receiver*" - Komodo Whitepaper).
//! 
//! Bob = Seller = Liquidity provider = Market maker  
//! ("*On the other side of the atomic swap, we have the liquidity provider — we call this person, Bob*" - Komodo Whitepaper).
//! 
//! # Algorithm updates
//! 
//! At the end of 2018 most UTXO coins have BIP65 (https://github.com/bitcoin/bips/blob/master/bip-0065.mediawiki).
//! The previous swap protocol discussions took place at 2015-2016 when there were just a few
//! projects that implemented CLTV opcode support:
//! https://bitcointalk.org/index.php?topic=1340621.msg13828271#msg13828271
//! https://bitcointalk.org/index.php?topic=1364951
//! So the Tier Nolan approach is a bit outdated, the main purpose was to allow swapping of a coin
//! that doesn't have CLTV at least as Alice side (as APayment is 2of2 multisig).
//! Nowadays the protocol can be simplified to the following (UTXO coins, BTC and forks):
//! 
//! 1. AFee: OP_DUP OP_HASH160 FEE_RMD160 OP_EQUALVERIFY OP_CHECKSIG
//!
//! 2. BPayment:
//! OP_IF
//! <now + LOCKTIME*2> OP_CLTV OP_DROP <bob_pub> OP_CHECKSIG
//! OP_ELSE
//! OP_SIZE 32 OP_EQUALVERIFY OP_HASH160 <hash(bob_privN)> OP_EQUALVERIFY <alice_pub> OP_CHECKSIG
//! OP_ENDIF
//! 
//! 3. APayment:
//! OP_IF
//! <now + LOCKTIME> OP_CLTV OP_DROP <alice_pub> OP_CHECKSIG
//! OP_ELSE
//! OP_SIZE 32 OP_EQUALVERIFY OP_HASH160 <hash(bob_privN)> OP_EQUALVERIFY <bob_pub> OP_CHECKSIG
//! OP_ENDIF
//! 

/******************************************************************************
 * Copyright © 2014-2018 The SuperNET Developers.                             *
 *                                                                            *
 * See the AUTHORS, DEVELOPER-AGREEMENT and LICENSE files at                  *
 * the top-level directory of this distribution for the individual copyright  *
 * holder information and the developer policies on copyright and licensing.  *
 *                                                                            *
 * Unless otherwise agreed in a custom licensing agreement, no part of the    *
 * SuperNET software, including this file may be copied, modified, propagated *
 * or distributed except according to the terms contained in the LICENSE file *
 *                                                                            *
 * Removal or modification of this copyright notice is prohibited.            *
 *                                                                            *
 ******************************************************************************/
//
//  lp_swap.rs
//  marketmaker
//

#![cfg_attr(not(feature = "native"), allow(dead_code))]

use bigdecimal::BigDecimal;
use rpc::v1::types::{Bytes as BytesJson, H160 as H160Json, H256 as H256Json, H264 as H264Json};
use coins::{lp_coinfind, MmCoinEnum, TradeInfo, TransactionDetails, TransactionEnum};
use common::{block_on, bits256, rpc_response, HyRes, MM_VERSION};
use common::executor::Timer;
use common::log::{TagParam};
use common::mm_ctx::{from_ctx, MmArc};
use futures01::Future;
use futures::future::Either;
use gstuff::{now_float, now_ms, slurp};
use http::Response;
use primitives::hash::{H160, H264};
use serde_json::{self as json, Value as Json};
use serialization::{deserialize, serialize};
use std::collections::{HashSet, HashMap};
use std::ffi::OsStr;
use std::fs::{File, DirEntry};
use std::io::prelude::*;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::thread;
use std::time::{Duration, SystemTime};
use uuid::Uuid;

// NB: Using a macro instead of a function in order to preserve the line numbers in the log.
macro_rules! send {
    ($ctx: expr, $to: expr, $subj: expr, $fallback: expr, $payload: expr) => {{
        // Checksum here helps us visually verify the logistics between the Maker and Taker logs.
        let crc = crc32::checksum_ieee (&$payload);
        log!("Sending '" ($subj) "' (" ($payload.len()) " bytes, crc " (crc) ")");

        block_on (peers::send ($ctx.clone(), $to, Vec::from ($subj.as_bytes()), $fallback, $payload.into()))
    }}
}

// NB: `$validator` is where we should put the decryption and verification in,
// in order for the bogus DHT input to disrupt communication less.
macro_rules! recv_ {
    ($swap: expr, $subj: expr, $timeout_sec: expr, $ec: expr, $validator: expr) => {{
        let recv_subject = fomat! (($subj) '@' ($swap.uuid));
        let recv_subjectᵇ = recv_subject.clone().into_bytes();
        let fallback = ($timeout_sec / 3) .min (30) .max (60) as u8;
        let recv_f = peers::recv ($swap.ctx.clone(), recv_subjectᵇ, fallback, $validator);

        let started = now_float();
        let timeout = (BASIC_COMM_TIMEOUT + $timeout_sec) as f64;
        let timeoutᶠ = Timer::till (started + timeout);
        block_on (async move {
            let r = match futures::future::select (Box::pin (recv_f), timeoutᶠ) .await {
                Either::Left ((r, _)) => r,
                Either::Right (_) => return ERR! ("timeout ({:.1} > {:.1})", now_float() - started, timeout)
            };
            if let Ok (ref payload) = r {
                // Checksum here helps us visually verify the logistics between the Maker and Taker logs.
                let crc = crc32::checksum_ieee (&payload);
                log! ("Received '" (recv_subject) "' (" (payload.len()) " bytes, crc " (crc) ")");
            }
            r
        })
    }}
}

macro_rules! recv {
    ($selff: ident, $subj: expr, $timeout_sec: expr, $ec: expr, $validator: expr) => {
        recv_! ($selff, $subj, $timeout_sec, $ec, $validator)
    };
    // Use this form if there's a sending future to terminate upon receiving the answer.
    ($selff: ident, $sending_f: ident, $subj: expr, $timeout_sec: expr, $ec: expr, $validator: expr) => {{
        let payload = recv_! ($selff, $subj, $timeout_sec, $ec, $validator);
        drop ($sending_f);
        payload
    }};
}

#[path = "lp_swap/maker_swap.rs"]
mod maker_swap;
#[path = "lp_swap/taker_swap.rs"]
mod taker_swap;

use maker_swap::{MakerSavedSwap, stats_maker_swap_file_path};
use taker_swap::{TakerSavedSwap, stats_taker_swap_file_path};
pub use maker_swap::{MakerSwap, run_maker_swap};
pub use taker_swap::{TakerSwap, run_taker_swap};

/// Includes the grace time we add to the "normal" timeouts
/// in order to give different and/or heavy communication channels a chance.
const BASIC_COMM_TIMEOUT: u64 = 90;

/// Default atomic swap payment locktime, in seconds.
/// Maker sends payment with LOCKTIME * 2
/// Taker sends payment with LOCKTIME
const PAYMENT_LOCKTIME: u64 = 3600 * 2 + 300 * 2;
const _SWAP_DEFAULT_NUM_CONFIRMS: u32 = 1;
const _SWAP_DEFAULT_MAX_CONFIRMS: u32 = 6;

#[derive(Debug, PartialEq, Serialize)]
pub enum RecoveredSwapAction {
    RefundedMyPayment,
    SpentOtherPayment,
}

#[derive(Debug, PartialEq)]
pub struct RecoveredSwap {
    action: RecoveredSwapAction,
    coin: String,
    transaction: TransactionEnum,
}

/// Represents the amount of a coin locked by ongoing swap
pub struct LockedAmount {
    coin: String,
    amount: BigDecimal,
}

pub trait AtomicSwap: Send + Sync {
    fn locked_amount(&self) -> LockedAmount;

    fn uuid(&self) -> &str;

    fn maker_coin(&self) -> &str;

    fn taker_coin(&self) -> &str;
}

struct SwapsContext {
    running_swaps: Mutex<Vec<Weak<RwLock<dyn AtomicSwap>>>>,
}

impl SwapsContext {
    /// Obtains a reference to this crate context, creating it if necessary.
    fn from_ctx (ctx: &MmArc) -> Result<Arc<SwapsContext>, String> {
        Ok (try_s! (from_ctx (&ctx.swaps_ctx, move || {
            Ok (SwapsContext {
                running_swaps: Mutex::new(vec![]),
            })
        })))
    }
}

/// Get total amount of selected coin locked by all currently ongoing swaps
pub fn get_locked_amount(ctx: &MmArc, coin: &str) -> BigDecimal {
    let swap_ctx = unwrap!(SwapsContext::from_ctx(&ctx));
    let mut swaps = unwrap!(swap_ctx.running_swaps.lock());
    *swaps = swaps.drain_filter(|swap| match swap.upgrade() {
        Some(_) => true,
        None => false,
    }).collect();
    swaps.iter().fold(
        0.into(),
        |total, swap| {
            match swap.upgrade() {
                Some(swap) => {
                    let locked = unwrap!(swap.read()).locked_amount();
                    if locked.coin == coin {
                        total + &locked.amount
                    } else {
                        total
                    }
                },
                None => total,
            }
        }
    )
}

/// Get total amount of selected coin locked by all currently ongoing swaps except the one with selected uuid
fn get_locked_amount_by_other_swaps(ctx: &MmArc, except_uuid: &str, coin: &str) -> BigDecimal {
    let swap_ctx = unwrap!(SwapsContext::from_ctx(&ctx));
    let mut swaps = unwrap!(swap_ctx.running_swaps.lock());
    *swaps = swaps.drain_filter(|swap| match swap.upgrade() {
        Some(_) => true,
        None => false,
    }).collect();
    swaps.iter().fold(
        0.into(),
        |total, swap| {
            match swap.upgrade() {
                Some(swap) => {
                    let locked = unwrap!(swap.read()).locked_amount();
                    if locked.coin == coin && unwrap!(swap.read()).uuid() != except_uuid {
                        total + &locked.amount
                    } else {
                        total
                    }
                },
                None => total,
            }
        }
    )
}

pub fn active_swaps_using_coin(ctx: &MmArc, coin: &str) -> Result<Vec<Uuid>, String> {
    let swap_ctx = try_s!(SwapsContext::from_ctx(&ctx));
    let swaps = try_s!(swap_ctx.running_swaps.lock());
    let mut uuids = vec![];
    for swap in swaps.iter() {
        match swap.upgrade() {
            Some(swap) => {
                let swap = try_s!(swap.read());
                if swap.maker_coin() == coin || swap.taker_coin() == coin {
                    uuids.push(try_s!(swap.uuid().parse()))
                }
            },
            None => (),
        }
    }
    Ok(uuids)
}

/// Some coins are "slow" (block time is high - e.g. BTC average block time is ~10 minutes).
/// https://bitinfocharts.com/comparison/bitcoin-confirmationtime.html
/// We need to increase payment locktime accordingly when at least 1 side of swap uses "slow" coin.
fn lp_atomic_locktime(base: &str, rel: &str) -> u64 {
    if base == "BTC" || rel == "BTC" {
        PAYMENT_LOCKTIME * 10
    } else if base == "BCH" || rel == "BCH" || base == "BTG" || rel == "BTG" || base == "SBTC" || rel == "SBTC" {
        PAYMENT_LOCKTIME * 4
    } else {
        PAYMENT_LOCKTIME
    }
}

fn dex_fee_rate(base: &str, rel: &str) -> BigDecimal {
    if base == "KMD" || rel == "KMD" {
        // 1/777 - 10%
        BigDecimal::from(9) / BigDecimal::from(7770)
    } else {
        BigDecimal::from(1) / BigDecimal::from(777)
    }
}

pub fn dex_fee_amount(base: &str, rel: &str, trade_amount: &BigDecimal) -> BigDecimal {
    let rate = dex_fee_rate(base, rel);
    let min_fee = unwrap!("0.0001".parse());
    let fee_amount = trade_amount * rate;
    if fee_amount < min_fee {
        min_fee
    } else {
        fee_amount
    }
}

/// Data to be exchanged and validated on swap start, the replacement of LP_pubkeys_data, LP_choosei_data, etc.
#[derive(Debug, Default, Deserializable, Eq, PartialEq, Serializable)]
struct SwapNegotiationData {
    started_at: u64,
    payment_locktime: u64,
    secret_hash: H160,
    persistent_pubkey: H264,
}

fn my_swaps_dir(ctx: &MmArc) -> PathBuf {
    ctx.dbdir().join("SWAPS").join("MY")
}

pub fn my_swap_file_path(ctx: &MmArc, uuid: &str) -> PathBuf {
    my_swaps_dir(ctx).join(format!("{}.json", uuid))
}

fn save_stats_swap(ctx: &MmArc, swap: &SavedSwap) -> Result<(), String> {
    let (path, content) = match &swap {
        SavedSwap::Maker(maker_swap) => (stats_maker_swap_file_path(ctx, &maker_swap.uuid), try_s!(json::to_vec(&maker_swap))),
        SavedSwap::Taker(taker_swap) => (stats_taker_swap_file_path(ctx, &taker_swap.uuid), try_s!(json::to_vec(&taker_swap))),
    };
    let mut file = try_s!(File::create(path));
    try_s!(file.write_all(&content));
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
enum SavedSwap {
    Maker(MakerSavedSwap),
    Taker(TakerSavedSwap),
}

/// The helper structure that makes easier to parse the response for GUI devs
/// They won't have to parse the events themselves handling possible errors, index out of bounds etc.
#[derive(Debug, Serialize, Deserialize)]
pub struct MySwapInfo {
    my_coin: String,
    other_coin: String,
    my_amount: BigDecimal,
    other_amount: BigDecimal,
    started_at: u64,
}

impl SavedSwap {
    fn is_finished(&self) -> bool {
        match self {
            SavedSwap::Maker(swap) => swap.is_finished(),
            SavedSwap::Taker(swap) => swap.is_finished(),
        }
    }

    fn uuid(&self) -> &str {
        match self {
            SavedSwap::Maker(swap) => &swap.uuid,
            SavedSwap::Taker(swap) => &swap.uuid,
        }
    }

    fn maker_coin_ticker(&self) -> Result<String, String> {
        match self {
            SavedSwap::Maker(swap) => swap.maker_coin(),
            SavedSwap::Taker(swap) => swap.maker_coin(),
        }
    }

    fn taker_coin_ticker(&self) -> Result<String, String> {
        match self {
            SavedSwap::Maker(swap) => swap.taker_coin(),
            SavedSwap::Taker(swap) => swap.taker_coin(),
        }
    }

    fn get_my_info(&self) -> Option<MySwapInfo> {
        match self {
            SavedSwap::Maker(swap) => swap.get_my_info(),
            SavedSwap::Taker(swap) => swap.get_my_info(),
        }
    }

    fn recover_funds(self, ctx: MmArc) -> Result<RecoveredSwap, String> {
        let maker_ticker = try_s!(self.maker_coin_ticker());
        let maker_coin = match block_on(lp_coinfind(&ctx, &maker_ticker)) {
            Ok(Some(c)) => c,
            Ok(None) => return ERR!("Coin {} is not activated", maker_ticker),
            Err(e) => return ERR!("Error {} on {} coin find attempt", e, maker_ticker),
        };

        let taker_ticker = try_s!(self.taker_coin_ticker());
        let taker_coin = match block_on(lp_coinfind(&ctx, &taker_ticker)) {
            Ok(Some(c)) => c,
            Ok(None) => return ERR!("Coin {} is not activated", taker_ticker),
            Err(e) => return ERR!("Error {} on {} coin find attempt", e, taker_ticker),
        };
        match self {
            SavedSwap::Maker(saved) => {
                let (maker_swap, _) = try_s!(MakerSwap::load_from_saved(ctx, maker_coin, taker_coin, saved));
                Ok(try_s!(maker_swap.recover_funds()))
            },
            SavedSwap::Taker(saved) => {
                let (taker_swap, _) = try_s!(TakerSwap::load_from_saved(ctx, maker_coin, taker_coin, saved));
                Ok(try_s!(taker_swap.recover_funds()))
            },
        }
    }

    fn is_recoverable(&self) -> bool {
        match self {
            SavedSwap::Maker(saved) => {
                saved.is_recoverable()
            },
            SavedSwap::Taker(saved) => {
                saved.is_recoverable()
            },
        }
    }

    fn save_to_db(&self, ctx: &MmArc) -> Result<(), String> {
        let path = my_swap_file_path(ctx, self.uuid());
        if path.exists() {
            return ERR!("File already exists");
        };
        let content = try_s!(json::to_vec(self));
        try_s!(std::fs::write(path, &content));
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SwapError {
    error: String,
}

impl Into<SwapError> for String {
    fn into(self) -> SwapError {
        SwapError {
            error: self
        }
    }
}

#[derive(Serialize)]
struct MySwapStatusResponse<'a> {
    #[serde(flatten)]
    swap: &'a SavedSwap,
    my_info: Option<MySwapInfo>,
    recoverable: bool,
}

impl<'a> From<&'a SavedSwap> for MySwapStatusResponse<'a> {
    fn from(swap: &'a SavedSwap) -> MySwapStatusResponse {
        MySwapStatusResponse {
            swap,
            my_info: swap.get_my_info(),
            recoverable: swap.is_recoverable(),
        }
    }
}

/// Returns the status of swap performed on `my` node
pub fn my_swap_status(ctx: MmArc, req: Json) -> HyRes {
    let uuid = try_h!(req["params"]["uuid"].as_str().ok_or("uuid parameter is not set or is not string"));
    let path = my_swap_file_path(&ctx, uuid);
    let content = slurp(&path);
    if content.is_empty() {
        return rpc_response(404, json!({
            "error": "swap data is not found"
        }).to_string());
    }
    let status: SavedSwap = try_h!(json::from_slice(&content));

    rpc_response(200, json!({
        "result": MySwapStatusResponse::from(&status)
    }).to_string())
}

/// Returns the status of requested swap, typically performed by other nodes and saved by `save_stats_swap_status`
pub fn stats_swap_status(ctx: MmArc, req: Json) -> HyRes {
    let uuid = try_h!(req["params"]["uuid"].as_str().ok_or("uuid parameter is not set or is not string"));
    let maker_path = stats_maker_swap_file_path(&ctx, uuid);
    let taker_path = stats_taker_swap_file_path(&ctx, uuid);
    let maker_content = slurp(&maker_path);
    let taker_content = slurp(&taker_path);
    let maker_status: Option<MakerSavedSwap> = if maker_content.is_empty() {
        None
    } else {
        Some(try_h!(json::from_slice(&maker_content)))
    };

    let taker_status: Option<TakerSavedSwap> = if taker_content.is_empty() {
        None
    } else {
        Some(try_h!(json::from_slice(&taker_content)))
    };

    if maker_status.is_none() && taker_status.is_none() {
        return rpc_response(404, json!({
            "error": "swap data is not found"
        }).to_string());
    }

    rpc_response(200, json!({
        "result": {
            "maker": maker_status,
            "taker": taker_status,
        }
    }).to_string())
}

/// Broadcasts `my` swap status to P2P network
fn broadcast_my_swap_status(uuid: &str, ctx: &MmArc) -> Result<(), String> {
    let path = my_swap_file_path(ctx, uuid);
    let content = slurp(&path);
    let mut status: SavedSwap = try_s!(json::from_slice(&content));
    match &mut status {
        SavedSwap::Taker(_) => (), // do nothing for taker
        SavedSwap::Maker(ref mut swap) => swap.hide_secret(),
    };
    try_s!(save_stats_swap(ctx, &status));
    let status_string = json!({
        "method": "swapstatus",
        "data": status,
    }).to_string();
    ctx.broadcast_p2p_msg(&status_string);
    Ok(())
}

/// Saves the swap status notification received from P2P network to local DB.
pub fn save_stats_swap_status(ctx: &MmArc, data: Json) -> HyRes {
    let swap: SavedSwap = try_h!(json::from_value(data));
    try_h!(save_stats_swap(ctx, &swap));
    rpc_response(200, json!({
        "result": "success"
    }).to_string())
}

/// Returns the data of recent swaps of `my` node. Returns no more than `limit` records (default: 10).
/// Skips the first `skip` records (default: 0).
pub fn my_recent_swaps(ctx: MmArc, req: Json) -> HyRes {
    let limit = req["limit"].as_u64().unwrap_or(10);
    let from_uuid = req["from_uuid"].as_str();
    let mut entries: Vec<(SystemTime, DirEntry)> = try_h!(my_swaps_dir(&ctx).read_dir()).filter_map(|dir_entry| {
        let entry = match dir_entry {
            Ok(ent) => ent,
            Err(e) => {
                log!("Error " (e) " reading from dir " (my_swaps_dir(&ctx).display()));
                return None;
            }
        };

        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                log!("Error " (e) " getting file " (entry.path().display()) " meta");
                return None;
            }
        };

        let m_time = match metadata.modified() {
            Ok(time) => time,
            Err(e) => {
                log!("Error " (e) " getting file " (entry.path().display()) " m_time");
                return None;
            }
        };

        if entry.path().extension() == Some(OsStr::new("json")) {
            Some((m_time, entry))
        } else {
            None
        }
    }).collect();
    // sort by m_time in descending order
    entries.sort_by(|(a, _), (b, _)| b.cmp(&a));

    let skip = match from_uuid {
        Some(uuid) => try_h!(entries.iter().position(|(_, entry)| entry.path() == my_swap_file_path(&ctx, uuid)).ok_or(format!("from_uuid {} swap is not found", uuid))) + 1,
        None => 0,
    };

    // iterate over file entries trying to parse the file contents and add to result vector
    let swaps: Vec<Json> = entries.iter().skip(skip).take(limit as usize).map(|(_, entry)|
        match json::from_slice::<SavedSwap>(&slurp(&entry.path())) {
            Ok(swap) => unwrap!(json::to_value(MySwapStatusResponse::from(&swap))),
            Err(e) => {
                log!("Error " (e) " parsing JSON from " (entry.path().display()));
                Json::Null
            },
        },
    ).collect();

    rpc_response(200, json!({
        "result": {
            "swaps": swaps,
            "from_uuid": from_uuid,
            "skipped": skip,
            "limit": limit,
            "total": entries.len(),
        },
    }).to_string())
}

/// Find out the swaps that need to be kick-started, continue from the point where swap was interrupted
/// Return the tickers of coins that must be enabled for swaps to continue
pub fn swap_kick_starts(ctx: MmArc) -> HashSet<String> {
    let mut coins = HashSet::new();
    let entries: Vec<DirEntry> = unwrap!(my_swaps_dir(&ctx).read_dir()).filter_map(|dir_entry| {
        let entry = match dir_entry {
            Ok(ent) => ent,
            Err(e) => {
                log!("Error " (e) " reading from dir " (my_swaps_dir(&ctx).display()));
                return None;
            }
        };

        if entry.path().extension() == Some(OsStr::new("json")) {
            Some(entry)
        } else {
            None
        }
    }).collect();

    entries.iter().for_each(|entry| {
        match json::from_slice::<SavedSwap>(&slurp(&entry.path())) {
            Ok(swap) => {
                if !swap.is_finished() {
                    log!("Kick starting the swap " [swap.uuid()]);
                    let maker_coin_ticker = match swap.maker_coin_ticker() {
                        Ok(t) => t,
                        Err(e) => {
                            log!("Error " (e) " getting maker coin of swap " (swap.uuid()));
                            return;
                        }
                    };
                    let taker_coin_ticker = match swap.taker_coin_ticker() {
                        Ok(t) => t,
                        Err(e) => {
                            log!("Error " (e) " getting taker coin of swap " (swap.uuid()));
                            return;
                        }
                    };
                    coins.insert(maker_coin_ticker.clone());
                    coins.insert(taker_coin_ticker.clone());
                    thread::spawn({
                        let ctx = ctx.clone();
                        move || {
                            let mut taker_coin;
                            loop {
                                taker_coin = match block_on(lp_coinfind(&ctx, &taker_coin_ticker)) {
                                    Ok(c) => c,
                                    Err(e) => {
                                        log!("Error " (e) " on " (taker_coin_ticker) " find attempt");
                                        return;
                                    }
                                };
                                if taker_coin.is_some() {
                                    break;
                                }
                                log!("Can't kickstart the swap " (swap.uuid()) " until the coin " (taker_coin_ticker) " is activated");
                                thread::sleep(Duration::from_secs(5));
                            };

                            let mut maker_coin;
                            loop {
                                maker_coin = match block_on(lp_coinfind(&ctx, &maker_coin_ticker)) {
                                    Ok(c) => c,
                                    Err(e) => {
                                        log!("Error " (e) " on " (maker_coin_ticker) " find attempt");
                                        return;
                                    }
                                };
                                if maker_coin.is_some() {
                                    break;
                                }
                                log!("Can't kickstart the swap " (swap.uuid()) " until the coin " (maker_coin_ticker) " is activated");
                                thread::sleep(Duration::from_secs(5));
                            };
                            match swap {
                                SavedSwap::Maker(swap) => match MakerSwap::load_from_saved(
                                    ctx,
                                    maker_coin.unwrap(),
                                    taker_coin.unwrap(),
                                    swap,
                                ) {
                                    Ok((maker, command)) => run_maker_swap(maker, command),
                                    Err(e) => log!([e]),
                                },
                                SavedSwap::Taker(swap) => match TakerSwap::load_from_saved(
                                    ctx,
                                    maker_coin.unwrap(),
                                    taker_coin.unwrap(),
                                    swap,
                                ) {
                                    Ok((taker, command)) => run_taker_swap(taker, command),
                                    Err(e) => log!([e]),
                                },
                            }
                        }
                    });
                }
            },
            Err(_) => (),
        }
    });
    coins
}

pub async fn coins_needed_for_kick_start(ctx: MmArc) -> Result<Response<Vec<u8>>, String> {
    let res = try_s!(json::to_vec(&json!({
        "result": *(try_s!(ctx.coins_needed_for_kick_start.lock()))
    })));
    Ok(try_s!(Response::builder().body(res)))
}

pub async fn recover_funds_of_swap(ctx: MmArc, req: Json) -> Result<Response<Vec<u8>>, String> {
    let uuid = try_s!(req["params"]["uuid"].as_str().ok_or("uuid parameter is not set or is not string"));
    let path = my_swap_file_path(&ctx, uuid);
    let content = slurp(&path);
    if content.is_empty() { return ERR!("swap data is not found") }

    let swap: SavedSwap = try_s!(json::from_slice(&content));

    let recover_data = try_s!(swap.recover_funds(ctx));
    let res = try_s!(json::to_vec(&json!({
        "result": {
            "action": recover_data.action,
            "coin": recover_data.coin,
            "tx_hash": recover_data.transaction.tx_hash(),
            "tx_hex": BytesJson::from(recover_data.transaction.tx_hex()),
        }
    })));
    Ok(try_s!(Response::builder().body(res)))
}

pub async fn import_swaps(ctx: MmArc, req: Json) -> Result<Response<Vec<u8>>, String> {
    let swaps: Vec<SavedSwap> = try_s!(json::from_value(req["swaps"].clone()));
    let mut imported = vec![];
    let mut skipped = HashMap::new();
    for swap in swaps {
        match swap.save_to_db(&ctx) {
            Ok(_) => imported.push(swap.uuid().to_owned()),
            Err(e) => { skipped.insert(swap.uuid().to_owned(), e); },
        }
    };
    let res = try_s!(json::to_vec(&json!({
        "result": {
            "imported": imported,
            "skipped": skipped,
        }
    })));
    Ok(try_s!(Response::builder().body(res)))
}

#[cfg(test)]
mod lp_swap_tests {
    use super::*;

    #[test]
    fn test_dex_fee_amount() {
        let base = "BTC";
        let rel = "ETH";
        let amount = 1.into();
        let actual_fee = dex_fee_amount(base, rel, &amount);
        let expected_fee = amount / 777;
        assert_eq!(expected_fee, actual_fee);

        let base = "KMD";
        let rel = "ETH";
        let amount = 1.into();
        let actual_fee = dex_fee_amount(base, rel, &amount);
        let expected_fee = amount * BigDecimal::from(9) / 7770;
        assert_eq!(expected_fee, actual_fee);

        let base = "BTC";
        let rel = "KMD";
        let amount = 1.into();
        let actual_fee = dex_fee_amount(base, rel, &amount);
        let expected_fee = amount * BigDecimal::from(9) / 7770;
        assert_eq!(expected_fee, actual_fee);

        let base = "BTC";
        let rel = "KMD";
        let amount = unwrap!("0.001".parse());
        let actual_fee = dex_fee_amount(base, rel, &amount);
        let expected_fee: BigDecimal = unwrap!("0.0001".parse());
        assert_eq!(expected_fee, actual_fee);
    }

    #[test]
    fn test_serde_swap_negotiation_data() {
        let data = SwapNegotiationData::default();
        let bytes = serialize(&data);
        let deserialized = unwrap!(deserialize(bytes.as_slice()));
        assert_eq!(data, deserialized);
    }
}
