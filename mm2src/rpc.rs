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
 ******************************************************************************/
//
//  rpc.rs
//
//  Copyright © 2014-2018 SuperNET. All rights reserved.
//

#![cfg_attr(not(feature = "native"), allow(unused_imports))]
#![cfg_attr(not(feature = "native"), allow(dead_code))]

use bytes::Bytes;
use coins::{get_enabled_coins, get_trade_fee, send_raw_transaction, set_required_confirmations, withdraw, my_tx_history};
use common::{err_to_rpc_json_string, HyRes};
#[cfg(feature = "native")]
use common::wio::{slurp_reqʰ, CORE, CPUPOOL, HTTP};
use common::lift_body::LiftBody;
use common::mm_ctx::MmArc;
#[cfg(feature = "native")]
use common::mm_ctx::ctx2helpers;
#[cfg(feature = "native")]
use common::for_tests::common_wait_for_log_re;
use futures01::{self, Future, Stream};
use futures::compat::{Compat, Future01CompatExt};
use futures::future::{FutureExt, TryFutureExt};
use gstuff;
use http::{Request, Response, Method};
use http::request::Parts;
use http::header::{HeaderValue, ACCESS_CONTROL_ALLOW_ORIGIN, CONTENT_TYPE, CONTENT_LENGTH};
#[cfg(feature = "native")]
use hyper::{self, service::Service};
use serde_json::{self as json, Value as Json};
use std::future::{Future as Future03};
use std::net::SocketAddr;
#[cfg(feature = "native")]
use tokio_core::net::TcpListener;

use crate::mm2::lp_network;
use crate::mm2::lp_ordermatch::{buy, cancel_all_orders, cancel_order, my_orders, order_status, orderbook, sell, set_price};
use crate::mm2::lp_swap::{coins_needed_for_kick_start, import_swaps,  my_swap_status, my_recent_swaps,
                          recover_funds_of_swap, stats_swap_status};

#[path = "rpc/lp_commands.rs"]
pub mod lp_commands;
use self::lp_commands::*;

#[path = "rpc/lp_signatures.rs"]
pub mod lp_signatures;

/// Lists the RPC method not requiring the "userpass" authentication.  
/// None is also public to skip auth and display proper error in case of method is missing
const PUBLIC_METHODS: &[Option<&str>] = &[  // Sorted alphanumerically (on the first letter) for readability.
    Some("balance"),
    Some("balances"),
    Some("fundvalue"),
    Some("getprice"),
    Some("getpeers"),
    Some("getcoins"),
    Some("help"),
    Some("notify"),  // Manually checks the peer's public key.
    Some("orderbook"),
    Some("passphrase"),  // Manually checks the "passphrase".
    Some("pricearray"),
    Some("psock"),
    Some("statsdisp"),
    Some("stats_swap_status"),
    Some("tradesarray"),
    Some("ticker"),
    None
];

#[allow(unused_macros)]
macro_rules! unwrap_or_err_response {
    ($e:expr, $($args:tt)*) => {
        match $e {
            Ok (ok) => ok,
            Err (err) => {return rpc_err_response (500, &ERRL! ("{}", err))}
        }
    }
}

/// Handle bencoded helper requests.
/// 
/// Example of a helper request (resulting in the "Missing Field: `conf`" error):
/// 
///     curl -v http://127.0.0.1:7783/helper/ctx2helpers \
///       -X POST -H 'X-Helper-Checksum: 815441984' -H 'Content-Type: application/octet-stream' \
///       -d 'd18:secp256k1_key_pair38:.0..Z......g)e.Q.@..d.sn<.v..>0.P....Ie'
/// 
#[cfg(feature = "native")]
async fn helpers (ctx: MmArc, client: SocketAddr, req: Parts,
                  reqᵇ: Box<dyn Stream<Item=Bytes,Error=String>+Send>) -> Result<Response<Vec<u8>>, String> {
    let ct = try_s! (req.headers.get (CONTENT_TYPE) .ok_or ("No Content-Type"));
    if ct.as_bytes() != b"application/octet-stream" {return ERR! ("Unexpected Content-Type")}

    if !client.ip().is_loopback() {return ERR! ("Not local")}

    let reqᵇ = try_s! (reqᵇ.concat2().compat().await);
    //log! ("helpers] " [=req] ", " (gstuff::binprint (&reqᵇ, b'.')));

    let method = req.uri.path();
    if !method.starts_with ("/helper/") {return ERR! ("Bad method")}
    let method = &method[8..];

    let crc32 = try_s! (req.headers.get ("X-Helper-Checksum") .ok_or ("No checksum"));
    let crc32 = try_s! (crc32.to_str());
    let crc32: u32 = if crc32.starts_with ('-') {
        // https://www.npmjs.com/package/crc-32 returns signed values
        let i: i32 = try_s! (crc32.parse());
        i as u32  // Intended as a wrapping conversion.
    } else {try_s! (crc32.parse())};

    let mut hasher = crc32fast::Hasher::default();
    hasher.update (&reqᵇ);
    let expected_checksum = hasher.finalize();
    //log! ([=expected_checksum] ", " [=crc32]);
    if crc32 != expected_checksum {return ERR! ("Damaged goods")}

    let res = match method {
        "broadcast_p2p_msg" => try_s! (lp_network::broadcast_p2p_msgʰ (reqᵇ) .await),
        "p2p_tap" => try_s! (lp_network::p2p_tapʰ (reqᵇ) .await),
        "common_wait_for_log_re" => try_s! (common_wait_for_log_re (reqᵇ) .await),
        "ctx2helpers" => try_s! (ctx2helpers (ctx, reqᵇ) .await),
        "peers_initialize" => try_s! (peers::peers_initialize (reqᵇ) .await),
        "peers_send" => try_s! (peers::peers_send (reqᵇ) .await),
        "peers_recv" => try_s! (peers::peers_recv (reqᵇ) .await),
        "peers_drop_send_handler" => try_s! (peers::peers_drop_send_handlerʰ (reqᵇ) .await),
        "start_client_p2p_loop" => try_s! (lp_network::start_client_p2p_loopʰ (reqᵇ) .await),
        "start_seednode_loop" => try_s! (lp_network::start_seednode_loopʰ (reqᵇ) .await),
        "slurp_req" => try_s! (slurp_reqʰ (reqᵇ) .await),
        _ => return ERR! ("Unknown helper: {}", method)
    };

    let mut hasher = crc32fast::Hasher::default();
    hasher.update (&res);

    let res = try_s! (Response::builder()
        .header (CONTENT_TYPE, "application/octet-stream")
        .header (CONTENT_LENGTH, res.len())
        .header ("X-Helper-Checksum", hasher.finalize())
        .body (res));
    Ok (res)
}

struct RpcService {
    /// Allows us to get the `MmCtx` if it is still around.
    ctx_h: u32,
    /// The IP and port from whence the request is coming from.
    client: SocketAddr,
}

fn auth(json: &Json, ctx: &MmArc) -> Result<(), &'static str> {
    if !PUBLIC_METHODS.contains(&json["method"].as_str()) {
        if !json["userpass"].is_string() {
            return Err("Userpass is not set!");
        }

        if json["userpass"] != ctx.conf["rpc_password"] {
            return Err("Userpass is invalid!");
        }
    }
    Ok(())
}

/// Result of `fn dispatcher`.
pub enum DispatcherRes {
    /// `fn dispatcher` has found a Rust handler for the RPC "method".
    Match (HyRes),
    /// No handler found by `fn dispatcher`. Returning the `Json` request in order for it to be handled elsewhere.
    NoMatch (Json)
}

/// Using async/await (futures 0.3) in `dispatcher`
/// will pave the way for porting the remaining system threading code to async/await green threads.
fn hyres(handler: impl Future03<Output=Result<Response<Vec<u8>>, String>> + Send + 'static) -> HyRes {
    Box::new(handler.boxed().compat())
}

/// The dispatcher, with full control over the HTTP result and the way we run the `Future` producing it.
/// 
/// Invoked both directly from the HTTP endpoint handler below and in a delayed fashion from `lp_command_q_loop`.
/// 
/// Returns `None` if the requested "method" wasn't found among the ported RPC methods and has to be handled elsewhere.
pub fn dispatcher (req: Json, ctx: MmArc) -> DispatcherRes {
    //log! ("dispatcher] " (json::to_string (&req) .unwrap()));
    let method = match req["method"].clone() {
        Json::String (method) => method,
        _ => return DispatcherRes::NoMatch (req)
    };
    DispatcherRes::Match (match &method[..] {  // Sorted alphanumerically (on the first latter) for readability.
        // "autoprice" => lp_autoprice (ctx, req),
        "buy" => hyres(buy(ctx, req)),
        "cancel_all_orders" => cancel_all_orders (ctx, req),
        "cancel_order" => cancel_order (ctx, req),
        "coins_needed_for_kick_start" => hyres(coins_needed_for_kick_start(ctx)),
        "disable_coin" => disable_coin(ctx, req),
        // TODO coin initialization performs blocking IO, i.e request.wait(), have to run it on CPUPOOL to avoid blocking shared CORE.
        //      at least until we refactor the functions like `utxo_coin_from_iguana_info` to async versions.
        "enable" => hyres(enable(ctx, req)),
        "electrum" => hyres(electrum(ctx, req)),
        "get_enabled_coins" => get_enabled_coins (ctx),
        "get_trade_fee" => get_trade_fee (ctx, req),
        // "fundvalue" => lp_fundvalue (ctx, req, false),
        "help" => help(),
        "import_swaps" => {
            #[cfg(feature = "native")] {
                Box::new(CPUPOOL.spawn_fn(move || { hyres(import_swaps (ctx, req)) }))
            }
            #[cfg(not(feature = "native"))] {return DispatcherRes::NoMatch (req)}
        },
        // "inventory" => inventory (ctx, req),
        "my_orders" => my_orders (ctx),
        "my_balance" => my_balance (ctx, req),
        "my_tx_history" => my_tx_history(ctx, req),
        "notify" => lp_signatures::lp_notify_recv (ctx, req),  // Invoked usually from the `lp_command_q_loop`
        "orderbook" => orderbook (ctx, req),
        "order_status" => order_status (ctx, req),
        // "passphrase" => passphrase (ctx, req),
        "sell" => hyres(sell(ctx, req)),
        "send_raw_transaction" => send_raw_transaction (ctx, req),
        "setprice" => set_price (ctx, req),
        "stop" => stop (ctx),
        "my_recent_swaps" => my_recent_swaps(ctx, req),
        "my_swap_status" => my_swap_status(ctx, req),
        "recover_funds_of_swap" => {
            #[cfg(feature = "native")] {
                Box::new(CPUPOOL.spawn_fn(move || { hyres(recover_funds_of_swap (ctx, req)) }))
            }
            #[cfg(not(feature = "native"))] {return DispatcherRes::NoMatch (req)}
        },
        "set_required_confirmations" => hyres(set_required_confirmations(ctx, req)),
        "stats_swap_status" => stats_swap_status(ctx, req),
        "version" => version(),
        "withdraw" => withdraw(ctx, req),
        _ => return DispatcherRes::NoMatch (req)
    })
}

type RpcRes = Box<dyn Future<Item=Response<LiftBody<Vec<u8>>>, Error=String> + Send>;

async fn rpc_serviceʹ (ctx: MmArc, req: Parts, reqᵇ: Box<dyn Stream<Item=Bytes, Error=String> + Send>,
                       client: SocketAddr) -> Result<Response<Vec<u8>>, String> {
    if req.method != Method::POST {return ERR! ("Only POST requests are supported!")}

    #[cfg(feature = "native")] {
        // Checksum *tags* the helper requests and serves as a sanity check.
        if req.headers.contains_key ("X-Helper-Checksum") {return helpers (ctx, client, req, reqᵇ) .await}
    }

    let reqᵇ = try_s! (reqᵇ.concat2().compat().await);
    let reqʲ: Json = try_s! (json::from_slice (&reqᵇ));

    // https://github.com/artemii235/SuperNET/issues/368
    let local_only = ctx.conf["rpc_local_only"].as_bool().unwrap_or(true);
    if local_only && !client.ip().is_loopback() && !PUBLIC_METHODS.contains (&reqʲ["method"].as_str()) {
        return ERR! ("Selected method can be called from localhost only!")
    }
    try_s! (auth (&reqʲ, &ctx));

    let handler = match dispatcher (reqʲ, ctx.clone()) {
        DispatcherRes::Match (handler) => handler,
        DispatcherRes::NoMatch (req) => return ERR! ("No such method: {:?}", req["method"])
    };
    let res = try_s! (handler.compat().await);
    Ok (res)
}

#[cfg(feature = "native")]
async fn rpc_service (req: Request<hyper::Body>, ctx_h: u32, client: SocketAddr) -> Response<LiftBody<Vec<u8>>> {
    macro_rules! try_sf {($value: expr) => {match $value {Ok (ok) => ok, Err (err) => {
        log! ("RPC error response: " (err));
        let ebody = err_to_rpc_json_string (&fomat! ((err)));
        return unwrap! (Response::builder().status (500) .body (LiftBody::from (Vec::from (ebody))))
    }}}}

    let ctx = try_sf! (MmArc::from_ffi_handle (ctx_h));
    // https://github.com/artemii235/SuperNET/issues/219
    let rpc_cors = match ctx.conf["rpccors"].as_str() {
        Some(s) => try_sf! (HeaderValue::from_str (s)),
        None => HeaderValue::from_static ("http://localhost:3000"),
    };

    // Convert the native Hyper stream into a portable stream of `Bytes`.
    let (req, reqᵇ) = req.into_parts();
    let reqᵇ = Box::new (reqᵇ.then (|chunk| -> Result<Bytes, String> {
        match chunk {
            Ok (c) => Ok (c.into_bytes()),
            Err (err) => Err (fomat! ((err)))
        }
    }));

    let (mut parts, body) = match rpc_serviceʹ (ctx, req, reqᵇ, client) .await {
        Ok (r) => r.into_parts(),
        Err (err) => {
            log! ("RPC error response: " (err));
            let ebody = err_to_rpc_json_string (&err);
            return unwrap! (Response::builder()
                .status (500)
                .header (ACCESS_CONTROL_ALLOW_ORIGIN, rpc_cors)
                .body (LiftBody::from (Vec::from (ebody))))
        }
    };
    parts.headers.insert(
        ACCESS_CONTROL_ALLOW_ORIGIN,
        rpc_cors
    );
    Response::from_parts (parts, LiftBody::from (body))
}

#[cfg(feature = "native")]
impl Service for RpcService {
    type ReqBody = hyper::Body;
    type ResBody = LiftBody<Vec<u8>>;
    type Error = String;
    type Future = RpcRes;

    fn call(&mut self, req: Request<hyper::Body>) -> Self::Future {
        let f = rpc_service (req, self.ctx_h, self.client.clone());
        let f = Compat::new (Box::pin (f.map (|r|->Result<_,String>{Ok(r)})));
        Box::new (f)
    }
}

#[cfg(feature = "native")]
pub extern fn spawn_rpc(ctx_h: u32) {
    // NB: We need to manually handle the incoming connections in order to get the remote IP address,
    // cf. https://github.com/hyperium/hyper/issues/1410#issuecomment-419510220.
    // Although if the ability to access the remote IP address is solved by the Hyper in the future
    // then we might want to refactor into starting it ideomatically in order to benefit from a more graceful shutdown,
    // cf. https://github.com/hyperium/hyper/pull/1640.

    let ctx = unwrap! (MmArc::from_ffi_handle (ctx_h), "No context");

    let rpc_ip_port = unwrap! (ctx.rpc_ip_port());
    let listener = unwrap! (TcpListener::bind2 (&rpc_ip_port), "Can't bind on {}", rpc_ip_port);

    let server = listener
        .incoming()
        .for_each(move |(socket, _my_sock)| {
            let client = match socket.peer_addr() {
                Ok (addr) => addr,
                Err (err) => {
                    log! ({"spawn_rpc] No peer_addr: {}", err});
                    return Ok(())
                }
            };

            unwrap!(CORE.lock()).spawn(
                HTTP.serve_connection(
                    socket,
                    RpcService {
                        ctx_h,
                        client
                    },
                )
                .map(|_| ())
                .map_err (|err| log! ({"spawn_rpc] HTTP error: {}", err}))
            );
            Ok(())
        })
        .map_err (|err| log! ({"spawn_rpc] accept error: {}", err}));

    // Finish the server `Future` when `shutdown_rx` fires.

    let (shutdown_tx, shutdown_rx) = futures01::sync::oneshot::channel::<()>();
    let server = server.select2 (shutdown_rx) .then (|_| Ok(()));
    let mut shutdown_tx = Some (shutdown_tx);
    ctx.on_stop (Box::new (move || {
        if let Some (shutdown_tx) = shutdown_tx.take() {
            log! ("on_stop] firing shutdown_tx!");
            if let Err (_) = shutdown_tx.send(()) {log! ("on_stop] Warning, shutdown_tx already closed")}
            Ok(())
        } else {ERR! ("on_stop callback called twice!")}
    }));

    let rpc_ip_port = unwrap! (ctx.rpc_ip_port());
    unwrap! (CORE.lock()) .spawn ({
        log!(">>>>>>>>>> DEX stats " (rpc_ip_port.ip())":"(rpc_ip_port.port()) " \
                DEX stats API enabled at unixtime." (gstuff::now_ms() / 1000) " <<<<<<<<<");
        let _ = ctx.rpc_started.pin (true);
        server
    });
}

#[cfg(not(feature = "native"))]
pub extern fn spawn_rpc(_ctx_h: u32) {unimplemented!()}

#[cfg(not(feature = "native"))]
pub fn init_header_slots() {
    use common::header::RPC_SERVICE;
    use std::pin::Pin;

    fn rpc_service_fn (
        ctx: MmArc, req: Parts, reqᵇ: Box<dyn Stream<Item=Bytes, Error=String> + Send>, client: SocketAddr)
        -> Pin<Box<dyn Future03<Output=Result<Response<Vec<u8>>, String>> + Send>> {
            Box::pin (rpc_serviceʹ (ctx, req, reqᵇ, client))}
    let _ = RPC_SERVICE.pin (rpc_service_fn);
}
