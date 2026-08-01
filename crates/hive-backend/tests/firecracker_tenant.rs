//! REAL Firecracker microVM tenancy test.
//!
//! No-op unless running on Linux with `/dev/kvm`, the `firecracker` binary, and
//! the kernel/rootfs artifacts present (i.e. inside the Lima nested-virt VM:
//! `limactl shell hive`). On macOS / CI it skips, so the workspace suite stays
//! green everywhere while this still proves the real path when the host can.
//!
//! What it proves when it runs: our `FirecrackerBackend` boots TWO real aarch64
//! microVMs owned by different tenants, the kernel actually executes (console
//! shows a Linux boot), and each VM's host-side artifacts are partitioned under
//! its own tenant subtree (`<run_dir>/<tenant>/<cell>`).

use hive_backend::firecracker::{FirecrackerBackend, FirecrackerConfig};
use hive_backend::{CellBackend, CellSpec};
use hive_core::{CellId, ResourceSpec};
use std::time::Duration;

fn spec(tenant: &str) -> CellSpec {
    CellSpec {
        id: CellId::new(),
        image: "default".into(),
        resources: ResourceSpec {
            vcpus: 1,
            mem_mib: 128,
            disk_mib: 1024,
            timeout_secs: 0,
        },
        tenant: tenant.into(),
        container: None,
    }
}

#[tokio::test]
async fn firecracker_boots_real_microvms_isolated_per_tenant() {
    let cfg = FirecrackerConfig::default();
    let run_dir = cfg.run_dir.clone();
    let be = FirecrackerBackend::new(cfg);
    if !be.is_supported() {
        eprintln!("skipping: firecracker unsupported here (need Linux + /dev/kvm + binary)");
        return;
    }

    // Boot two microVMs owned by different tenants.
    let a = be
        .provision(&spec("alpha"))
        .await
        .expect("provision alpha microVM");
    let b = be
        .provision(&spec("beta"))
        .await
        .expect("provision beta microVM");

    // Host-side artifacts (api sockets, overlay disks, vsock, console) are
    // partitioned per tenant — the host analogue of the per-VM kernel boundary.
    assert!(
        a.root.starts_with(run_dir.join("alpha")),
        "alpha run dir not under its tenant: {:?}",
        a.root
    );
    assert!(
        b.root.starts_with(run_dir.join("beta")),
        "beta run dir not under its tenant: {:?}",
        b.root
    );
    assert_ne!(
        a.root.parent(),
        b.root.parent(),
        "tenants must not share a run-dir parent"
    );
    assert!(
        a.root.join("api.sock").exists(),
        "firecracker API socket missing (VM not started)"
    );

    // The kernel actually executed (console shows a Linux boot) — proof a REAL
    // microVM booted, not just an accepted API call.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let console = std::fs::read_to_string(a.root.join("console.log")).unwrap_or_default();
    assert!(
        console.contains("Linux version") || console.to_lowercase().contains("booting"),
        "no kernel boot output in console.log (VM did not boot):\n{console}"
    );

    be.terminate(&a).await.unwrap();
    be.terminate(&b).await.unwrap();
}
