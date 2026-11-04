//! smoldot-light cannot run `TaggedTransactionQueue_validate_transaction` for a
//! PGAS claim on Paseo Asset Hub Next: the call proof it gets from full nodes
//! misses a storage entry the runtime reads. A full node runs the same call on
//! the same block and returns a result.
//!
//! Read-only: it calls `state_call`, never submits anything.
//!
//! Exit status: 0 when smoldot and the RPC node both return a result on every
//! block, 1 when smoldot fails with a storage access error on a block where the
//! RPC node returns a result (the bug), 2 for anything else.

#[cfg(all(feature = "smoldot-light-3", feature = "smoldot-light-2"))]
compile_error!("enable exactly one of the `smoldot-light-3` and `smoldot-light-2` features");

#[cfg(feature = "smoldot-light-2")]
use {smoldot_4 as smoldot, smoldot_light_2 as smoldot_light};
#[cfg(feature = "smoldot-light-3")]
use {smoldot_5 as smoldot, smoldot_light_3 as smoldot_light};

#[cfg(feature = "smoldot-light-2")]
const SMOLDOT_LIGHT_VERSION: &str = "2.1.0";
#[cfg(feature = "smoldot-light-3")]
const SMOLDOT_LIGHT_VERSION: &str = "3.0.0";

mod platform;

use std::{
    iter,
    num::NonZero,
    process::ExitCode,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use smoldot_light::{AddChainConfig, AddChainConfigJsonRpc, ChainId, Client};

const RELAY_SPEC: &str = include_str!("../specs/paseo.json");
const ASSET_HUB_SPEC: &str = include_str!("../specs/paseo-next-v2-asset-hub.json");
const RPC_NODE: &str = "https://paseo-asset-hub-next-rpc.polkadot.io";
/// An unsigned `Pgas.claim_pgas` with the `AsPgas` extension, included in Asset
/// Hub Next block 2699432 (extrinsic 2). Any block after that answers
/// `Invalid(Custom(232))`, which is fine here: the call must still run.
const CLAIM: &str = include_str!("../tx/pgas-claim.hex");
/// Blocks to test, each one the finalized block at that time.
const ROUNDS: usize = 3;

struct Light {
    client: Client<platform::Platform, ()>,
    chain: ChainId,
    responses: mpsc::Receiver<Value>,
    next_id: u64,
}

impl Light {
    fn start() -> Self {
        let mut client = Client::new(platform::Platform::new());
        let relay = client
            .add_chain(AddChainConfig {
                user_data: (),
                specification: RELAY_SPEC,
                database_content: "",
                potential_relay_chains: iter::empty(),
                json_rpc: AddChainConfigJsonRpc::Disabled,
                statement_protocol_config: None,
            })
            .expect("add relay chain")
            .chain_id;
        let added = client
            .add_chain(AddChainConfig {
                user_data: (),
                specification: ASSET_HUB_SPEC,
                database_content: "",
                potential_relay_chains: iter::once(relay),
                json_rpc: AddChainConfigJsonRpc::Enabled {
                    max_pending_requests: NonZero::new(64).unwrap(),
                    max_subscriptions: 64,
                },
                statement_protocol_config: None,
            })
            .expect("add Asset Hub");
        let (sender, responses) = mpsc::channel();
        let mut stream = added.json_rpc_responses.expect("JSON-RPC enabled");
        thread::spawn(move || {
            while let Some(text) = smol::block_on(stream.next()) {
                if let Ok(value) = serde_json::from_str(&text) {
                    if sender.send(value).is_err() {
                        break;
                    }
                }
            }
        });
        Light { client, chain: added.chain_id, responses, next_id: 1 }
    }

    /// One JSON-RPC request. Returns `result`, or `error` as `Err`.
    fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.client
            .json_rpc_request(request.to_string(), self.chain)
            .expect("queue JSON-RPC request");
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.responses.recv_timeout(left) {
                Ok(answer) if answer["id"] == json!(id) => {
                    return match answer.get("result") {
                        Some(result) => Ok(result.clone()),
                        None => Err(answer["error"].clone()),
                    };
                }
                Ok(_) => continue,
                Err(_) => return Err(json!(format!("no answer within {timeout:?}"))),
            }
        }
    }
}

fn rpc_node(method: &str, params: Value) -> Result<Value, Value> {
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let mut answer: Value = ureq::post(RPC_NODE)
        .send_json(&request)
        .map_err(|e| json!(e.to_string()))?
        .body_mut()
        .read_json()
        .map_err(|e| json!(e.to_string()))?;
    match answer.get_mut("result") {
        Some(result) => Ok(result.take()),
        None => Err(answer["error"].take()),
    }
}

/// `TransactionValidity` (`Result<ValidTransaction, TransactionValidityError>`),
/// enough to tell the outcomes apart.
fn describe_validity(hex_result: &str) -> String {
    let bytes = hex::decode(hex_result.trim_start_matches("0x")).unwrap_or_default();
    const INVALID: [&str; 11] = [
        "Call", "Payment", "Future", "Stale", "BadProof", "AncientBirthBlock",
        "ExhaustsResources", "Custom", "BadMandatory", "MandatoryValidation", "BadSigner",
    ];
    match bytes.as_slice() {
        [0, rest @ ..] if rest.len() >= 8 => {
            format!("Valid (priority {})", u64::from_le_bytes(rest[..8].try_into().unwrap()))
        }
        [1, 0, 7, code, ..] => format!("Invalid(Custom({code}))"),
        [1, 0, kind, ..] => format!("Invalid({})", INVALID.get(*kind as usize).unwrap_or(&"?")),
        [1, 1, kind, ..] => format!("Unknown({kind})"),
        _ => "unrecognized".into(),
    }
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("off")).init();
    let claim = CLAIM.trim();
    println!("smoldot-light {SMOLDOT_LIGHT_VERSION}, Paseo Asset Hub Next, RPC node {RPC_NODE}");
    println!("claim: {} bytes", (claim.len() - 2) / 2);

    let mut light = Light::start();
    print!("light client syncing");
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if Instant::now() > deadline {
            println!("\nlight client did not sync within 5 minutes");
            return ExitCode::from(2);
        }
        thread::sleep(Duration::from_secs(3));
        print!(".");
        let _ = std::io::Write::flush(&mut std::io::stdout());
        let health = light.call("system_health", json!([]), Duration::from_secs(10));
        if matches!(health, Ok(ref h) if h["isSyncing"] == json!(false) && h["peers"].as_u64() > Some(0)) {
            break;
        }
    }
    println!(" done");

    let (mut light_failed, mut other) = (0, 0);
    for round in 1..=ROUNDS {
        let block = light
            .call("chain_getFinalizedHead", json!([]), Duration::from_secs(10))
            .expect("finalized head");
        let block = block.as_str().unwrap().to_owned();
        let number = light
            .call("chain_getHeader", json!([block]), Duration::from_secs(30))
            .ok()
            .and_then(|h| h["number"].as_str().map(|n| u64::from_str_radix(&n[2..], 16).unwrap()));
        println!("\n[{round}/{ROUNDS}] finalized block #{} {block}", number.unwrap_or_default());

        let control = light.call(
            "state_call",
            json!(["Core_version", "0x", block]),
            Duration::from_secs(60),
        );
        println!(
            "  smoldot  Core_version: {}",
            if control.is_ok() { "ok" } else { "error" }
        );

        let params = format!("0x02{}{}", &claim[2..], &block[2..]);
        let args = json!(["TaggedTransactionQueue_validate_transaction", params, block]);
        let from_light = light.call("state_call", args.clone(), Duration::from_secs(120));
        let from_node = rpc_node("state_call", args);
        let show = |r: &Result<Value, Value>| match r {
            Ok(v) => describe_validity(v.as_str().unwrap_or_default()),
            Err(e) => format!("error {e}"),
        };
        println!("  smoldot  validate_transaction: {}", show(&from_light));
        println!("  RPC node validate_transaction: {}", show(&from_node));
        let storage_error = |e: &Value| e["message"].as_str().is_some_and(|m| m.contains("access the storage"));
        match (&from_light, &from_node) {
            (Err(e), Ok(_)) if storage_error(e) => light_failed += 1,
            (Ok(_), Ok(_)) => {}
            _ => other += 1,
        }
        thread::sleep(Duration::from_secs(12));
    }

    println!();
    if light_failed > 0 {
        println!("BUG: smoldot failed on {light_failed} of {ROUNDS} blocks where the RPC node answered.");
        ExitCode::from(1)
    } else if other > 0 {
        println!("inconclusive: see the errors above");
        ExitCode::from(2)
    } else {
        println!("OK: smoldot and the RPC node both answered on every block.");
        ExitCode::SUCCESS
    }
}
