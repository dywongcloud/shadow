//! Live witness for per-tenant framed-byte metering: real iroh endpoints, the real
//! `serve_tunnels_full` browser accept path and the real `BrowserPool` invoke path with a meter
//! installed through `hive_p2p::set_browser_meter`, proving the counted bytes equal the framed
//! wire bytes exactly and per endpoint. `cargo run -p hive-p2p --example browser_meter_witness`;
//! exit 0 = every witness line passed.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use hive_browser_proto::{check_len, encode_reply, encode_request, split_request, Op};

/// The fleet-ALPN target `serve_tunnels_full` is handed; this witness never reaches it.
const UNUSED_FLEET_TARGET: &str = "http://127.0.0.1:1";

/// The stand-in admission store: endpoint_id → tenant.
#[derive(Default, Clone)]
struct TenantMap {
    by_endpoint: Arc<Mutex<HashMap<String, String>>>,
    /// tenant -> (inbound, outbound, reports).
    meters: Arc<Mutex<BTreeMap<String, (u64, u64, u64)>>>,
}

impl TenantMap {
    fn admit(&self, endpoint_id: &str, tenant: &str) {
        self.by_endpoint
            .lock()
            .unwrap()
            .insert(endpoint_id.to_string(), tenant.to_string());
    }

    fn record(&self, endpoint_id: String, inbound: u64, outbound: u64) {
        let tenant = self
            .by_endpoint
            .lock()
            .unwrap()
            .get(&endpoint_id)
            .cloned()
            .unwrap_or_else(|| "_unattributed".to_string());
        let mut meters = self.meters.lock().unwrap();
        let row = meters.entry(tenant).or_default();
        row.0 += inbound;
        row.1 += outbound;
        row.2 += 1;
    }

    fn get(&self, tenant: &str) -> (u64, u64, u64) {
        self.meters
            .lock()
            .unwrap()
            .get(tenant)
            .copied()
            .unwrap_or_default()
    }
}

/// A minimal in-process browser peer answering every well-formed `Op::Invoke` with `reply`.
async fn spawn_fake_browser(reply: Vec<u8>) -> anyhow::Result<(iroh::Endpoint, String, String)> {
    let ep = hive_p2p::bind().await?;
    let endpoint_id = ep.id().to_string();
    let addr_json = hive_p2p::addr_json(&ep)
        .ok_or_else(|| anyhow::anyhow!("fake browser addr_json unavailable"))?;
    let accept_ep = ep.clone();
    tokio::spawn(async move {
        while let Some(incoming) = accept_ep.accept().await {
            let reply = reply.clone();
            tokio::spawn(async move {
                let Ok(conn) = incoming.await else { return };
                if conn.alpn() != hive_p2p::BROWSER_ALPN {
                    return;
                }
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    let reply = reply.clone();
                    tokio::spawn(async move {
                        let mut lenb = [0u8; 4];
                        if recv.read_exact(&mut lenb).await.is_err() {
                            return;
                        }
                        let Ok(len) = check_len(lenb) else { return };
                        let mut buf = vec![0u8; len];
                        if recv.read_exact(&mut buf).await.is_err() {
                            return;
                        }
                        let mut trailing = [0u8; 1];
                        if !matches!(recv.read(&mut trailing).await, Ok(None)) {
                            return;
                        }
                        let Ok((Op::Invoke, _payload)) = split_request(&buf) else {
                            return;
                        };
                        if send.write_all(&encode_reply(&reply)).await.is_err() {
                            return;
                        }
                        let _ = send.finish();
                    });
                }
            });
        }
    });
    Ok((ep, endpoint_id, addr_json))
}

fn check(label: &str, expected: (u64, u64, u64), actual: (u64, u64, u64), failures: &mut usize) {
    if expected == actual {
        println!(
            "WITNESS_OK:{label}: in={} out={} reports={}",
            actual.0, actual.1, actual.2
        );
    } else {
        println!(
            "WITNESS_FAIL:{label}: expected in={} out={} reports={}, got in={} out={} reports={}",
            expected.0, expected.1, expected.2, actual.0, actual.1, actual.2
        );
        *failures += 1;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut failures = 0usize;

    let map = TenantMap::default();
    let recorder = map.clone();
    hive_p2p::set_browser_meter(Some(Arc::new(move |endpoint_id, inbound, outbound| {
        recorder.record(endpoint_id, inbound, outbound);
    })));

    let fleet_ep = hive_p2p::bind().await?;
    let fleet_addr_json = hive_p2p::addr_json(&fleet_ep)
        .ok_or_else(|| anyhow::anyhow!("fleet addr_json unavailable"))?;
    let known: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let admission_known = known.clone();
    let admission: hive_p2p::BrowserAdmissionHandler = Arc::new(move |endpoint_id: String| {
        let known = admission_known.clone();
        Box::pin(async move { known.lock().unwrap().contains(&endpoint_id) })
    });
    tokio::spawn(hive_p2p::serve_tunnels_full(
        fleet_ep.clone(),
        UNUSED_FLEET_TARGET.into(),
        32,
        None,
        None,
        None,
        None,
        Some(admission),
        None,
        &[],
    ));
    let pool = hive_p2p::BrowserPool::new(fleet_ep);

    let reply_a = format!("{{\"tenant\":\"alpha\",\"pad\":\"{}\"}}", "a".repeat(480)).into_bytes();
    let reply_b = format!("{{\"tenant\":\"beta\",\"pad\":\"{}\"}}", "b".repeat(480)).into_bytes();
    let (ep_a, id_a, addr_a) = spawn_fake_browser(reply_a.clone()).await?;
    let (_ep_b, id_b, addr_b) = spawn_fake_browser(reply_b.clone()).await?;
    map.admit(&id_a, "tenant-alpha");
    map.admit(&id_b, "tenant-beta");
    known.lock().unwrap().extend([id_a.clone(), id_b.clone()]);
    println!("WITNESS_BROWSER_A:{id_a} (tenant-alpha)");
    println!("WITNESS_BROWSER_B:{id_b} (tenant-beta)");

    let digest = "a".repeat(64);
    let request_json = "x".repeat(64 * 1024);
    let invoke_payload = hive_browser_proto::encode_invoke(&digest, &request_json)?;
    let expected_frame = encode_request(Op::Invoke, &invoke_payload).len() as u64;
    println!(
        "WITNESS_INVOKE_REQUEST_BYTES:{expected_frame} (4 prefix + 1 op + 64 digest + 65536 payload)"
    );
    let reply = pool
        .invoke(&id_a, &addr_a, &digest, &request_json)
        .await
        .map_err(|e| anyhow::anyhow!("invoke a failed: {e}"))?;
    assert_eq!(reply, reply_a, "fake browser A replied verbatim");
    let expected_in_a = 4 + reply_a.len() as u64;
    check(
        "invoke-64kib-tenant-alpha",
        (expected_in_a, expected_frame, 2),
        map.get("tenant-alpha"),
        &mut failures,
    );
    check(
        "invoke-64kib-tenant-beta-untouched",
        (0, 0, 0),
        map.get("tenant-beta"),
        &mut failures,
    );

    let reply = pool
        .invoke(&id_b, &addr_b, &digest, &request_json)
        .await
        .map_err(|e| anyhow::anyhow!("invoke b failed: {e}"))?;
    assert_eq!(reply, reply_b, "fake browser B replied verbatim");
    let expected_in_b = 4 + reply_b.len() as u64;
    check(
        "invoke-64kib-tenant-beta",
        (expected_in_b, expected_frame, 2),
        map.get("tenant-beta"),
        &mut failures,
    );
    check(
        "invoke-64kib-tenant-alpha-unchanged",
        (expected_in_a, expected_frame, 2),
        map.get("tenant-alpha"),
        &mut failures,
    );

    let fleet_addr: iroh::EndpointAddr = serde_json::from_str(&fleet_addr_json)?;
    let conn = ep_a.connect(fleet_addr, hive_p2p::BROWSER_ALPN).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let echo_payload = vec![0x5au8; 1000];
    let echo_frame = encode_request(Op::Echo, &echo_payload);
    println!(
        "WITNESS_ECHO_REQUEST_BYTES:{} (4 prefix + 1 op + 1000 payload)",
        echo_frame.len()
    );
    send.write_all(&echo_frame).await?;
    send.finish()?;
    let mut lenb = [0u8; 4];
    recv.read_exact(&mut lenb).await?;
    let len = check_len(lenb)?;
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await?;
    assert_eq!(body, echo_payload, "echo returns the payload verbatim");
    check(
        "inbound-echo-tenant-alpha",
        (expected_in_a + 5 + 1000, expected_frame + 4 + 1000, 4),
        map.get("tenant-alpha"),
        &mut failures,
    );
    check(
        "inbound-echo-tenant-beta-unchanged",
        (expected_in_b, expected_frame, 2),
        map.get("tenant-beta"),
        &mut failures,
    );

    if failures == 0 {
        println!(
            "WITNESS_OK:ALL: per-tenant framed-byte metering is exact and separately attributed"
        );
        Ok(())
    } else {
        println!("WITNESS_FAIL:{failures} check(s) failed");
        std::process::exit(1);
    }
}
