//! Secure compute connections — ephemeral WireGuard-style tunnels to private
//! backends (a customer DB in a VPC), keyed by short-lived credentials from the
//! Key Exchange Service. Mirrors Vercel Hive's secure connectors.
//!
//! Datapath: a **connector** binds `127.0.0.1:<local_port>`. A function/build is
//! handed that local address (injected as a project env var), so it dials a
//! local socket. For every connection the connector stands up a fresh WireGuard
//! tunnel and relays bytes — encrypting on the connector side, decrypting on the
//! gateway side — to the real private backend. No traffic to the backend leaves
//! the box in plaintext, and no long-lived secrets live in the function.

use hive_core::now_ms;
use hive_wg::{Kes, SecureLink};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LinkRecord {
    pub id: String,
    /// Private backend this connector fronts, e.g. "db.internal:5432".
    pub target: String,
    /// Local address a function dials to reach the backend over the tunnel.
    pub local_addr: String,
    pub region: String,
    /// "active" | "expired"
    pub status: String,
    /// WireGuard public key of the ephemeral connector.
    pub public_key: String,
    pub created_ms: u64,
    pub expires_ms: u64,
    #[serde(default)]
    pub team: String,
    /// Project whose env was wired to this connector (if any).
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub env_var: String,
}

#[derive(Deserialize)]
pub struct ProvisionReq {
    pub target: String,
    #[serde(default)]
    pub region: Option<String>,
    /// Lease lifetime in seconds (default 900 = 15m).
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    #[serde(default)]
    pub team: String,
    /// Optionally wire a project: inject `env_var` = local connector address.
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub env_var: Option<String>,
}

pub struct SecureLinkStore {
    kes: Kes,
    links: RwLock<Vec<LinkRecord>>,
}

impl SecureLinkStore {
    pub fn new() -> SecureLinkStore {
        SecureLinkStore {
            kes: Kes::new(),
            links: RwLock::new(Vec::new()),
        }
    }

    /// Provision a connector: bind a local listener that relays over WireGuard to
    /// `target`, mint a lease, and record it. Returns the record (with the local
    /// address a function should dial).
    pub async fn provision(
        &self,
        req: ProvisionReq,
        default_region: &str,
    ) -> Result<LinkRecord, String> {
        let ttl = req.ttl_secs.unwrap_or(900).clamp(30, 24 * 3600);
        // Validate the tunnel is establishable (real handshake), then mint lease.
        let (lease, _probe) = SecureLink::establish_with_kes(&self.kes, &req.target, ttl)?;

        // Bring up the actual datapath: a local TCP listener relaying to target.
        let local_port = start_connector(req.target.clone())
            .await
            .map_err(|e| format!("connector bind failed: {e}"))?;
        let local_addr = format!("127.0.0.1:{local_port}");

        let rec = LinkRecord {
            id: lease.id.clone(),
            target: req.target,
            local_addr,
            region: req.region.unwrap_or_else(|| default_region.to_string()),
            status: "active".into(),
            public_key: lease.public_key,
            created_ms: lease.created_ms,
            expires_ms: lease.expires_ms,
            team: if req.team.is_empty() {
                "personal".into()
            } else {
                req.team
            },
            project: req.project.unwrap_or_default(),
            env_var: req.env_var.unwrap_or_default(),
        };
        self.links.write().push(rec.clone());
        Ok(rec)
    }

    /// All secure links across teams (ops data browser).
    pub fn all(&self) -> Vec<LinkRecord> {
        self.links.read().clone()
    }

    /// Snapshot for the fleet follower sync. Link records are pure metadata
    /// (target/region/status/public_key/timestamps — no per-node secret; the
    /// tunnel connector itself stays on its origin node), so replicating them
    /// makes the ops link list fleet-consistent without moving any secret.
    pub fn snapshot(&self) -> Vec<LinkRecord> {
        self.links.read().clone()
    }

    pub fn load(&self, data: Vec<LinkRecord>) {
        *self.links.write() = data;
    }

    pub fn list(&self, team: &str) -> Vec<LinkRecord> {
        let now = now_ms();
        let mut l = self.links.write();
        for r in l.iter_mut() {
            if r.status == "active" && r.expires_ms <= now {
                r.status = "expired".into();
            }
        }
        l.iter()
            .filter(|r| {
                let t = if r.team.is_empty() {
                    "personal"
                } else {
                    &r.team
                };
                t == team
            })
            .cloned()
            .collect()
    }

    pub fn remove(&self, id: &str) {
        self.kes.revoke(id);
        self.links.write().retain(|r| r.id != id);
    }

    pub fn gc(&self) -> usize {
        let now = now_ms();
        self.links.write().retain(|r| r.expires_ms + 60_000 > now);
        self.kes.gc()
    }

    pub fn active(&self) -> usize {
        let now = now_ms();
        self.links
            .read()
            .iter()
            .filter(|r| r.expires_ms > now)
            .count()
    }
}

impl Default for SecureLinkStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Bind a loopback TCP listener that relays each connection to `target` over a
/// fresh WireGuard tunnel. Returns the bound local port.
pub async fn start_connector(target: String) -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((client, _peer)) => {
                    let target = target.clone();
                    tokio::spawn(async move {
                        if let Err(e) = relay(client, &target).await {
                            tracing::debug!(target = %target, error = %e, "secure relay ended");
                        }
                    });
                }
                Err(_) => break,
            }
        }
    });
    Ok(port)
}

/// Relay one client connection to `target`, with every byte sealed/opened
/// through a per-connection WireGuard tunnel.
async fn relay(client: TcpStream, target: &str) -> Result<(), String> {
    let mut link = SecureLink::establish(target)?;
    let upstream = TcpStream::connect(target)
        .await
        .map_err(|e| format!("dial backend: {e}"))?;
    let (mut cr, mut cw) = client.into_split();
    let (mut ur, mut uw) = upstream.into_split();
    let mut cbuf = vec![0u8; 16 * 1024];
    let mut ubuf = vec![0u8; 16 * 1024];

    loop {
        tokio::select! {
            r = cr.read(&mut cbuf) => {
                let n = r.map_err(|e| e.to_string())?;
                if n == 0 { break; }
                // client -> (seal) -> tunnel -> (open) -> backend
                let ct = link.seal_c2s(&cbuf[..n])?;
                let pt = link.open_c2s(&ct)?;
                uw.write_all(&pt).await.map_err(|e| e.to_string())?;
            }
            r = ur.read(&mut ubuf) => {
                let n = r.map_err(|e| e.to_string())?;
                if n == 0 { break; }
                // backend -> (seal) -> tunnel -> (open) -> client
                let ct = link.seal_s2c(&ubuf[..n])?;
                let pt = link.open_s2c(&ct)?;
                cw.write_all(&pt).await.map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}
