//! `hive-wg` — ephemeral WireGuard-style secure tunnels for secure compute.
//!
//! Lets a build/function reach a **private backend** (a customer DB in a VPC)
//! over an encrypted tunnel whose keys are short-lived and minted on demand by a
//! **Key Exchange Service (KES)** — the pattern Vercel's Hive uses to cut
//! secure-connection setup from ~90s to ~5s (no long-lived secrets baked into
//! images).
//!
//! The datapath uses WireGuard's exact handshake —
//! `Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s` — via the `snow` Noise framework,
//! running fully in userspace (no kernel module or `wg` tooling).

use std::collections::HashMap;
use std::sync::Mutex;

use base64::{engine::general_purpose::STANDARD, Engine};
use hive_core::now_ms;
use rand::RngCore;
use serde::Serialize;
use snow::{Builder, TransportState};

/// WireGuard's Noise pattern.
const PARAMS: &str = "Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s";

fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn random_psk() -> [u8; 32] {
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b
}

/// A WireGuard keypair (raw bytes).
pub struct Keypair {
    pub private: Vec<u8>,
    pub public: Vec<u8>,
}

/// Generate a fresh x25519 keypair using the Noise resolver.
pub fn gen_keypair() -> Keypair {
    let kp = Builder::new(PARAMS.parse().expect("params"))
        .generate_keypair()
        .expect("keypair");
    Keypair {
        private: kp.private,
        public: kp.public,
    }
}

/// A short-lived credential the KES hands to a connector. The private key never
/// leaves the issuing process; only the public key + metadata are exported.
#[derive(Clone, Debug, Serialize)]
pub struct PeerLease {
    pub id: String,
    /// Base64 public key for this ephemeral peer.
    pub public_key: String,
    pub created_ms: u64,
    /// When this lease expires (unix ms) — the connector is torn down after.
    pub expires_ms: u64,
}

struct Issued {
    private: Vec<u8>,
    lease: PeerLease,
}

/// Key Exchange Service: mints + tracks short-lived peer credentials.
pub struct Kes {
    issued: Mutex<HashMap<String, Issued>>,
    seq: Mutex<u64>,
}

impl Kes {
    pub fn new() -> Kes {
        Kes {
            issued: Mutex::new(HashMap::new()),
            seq: Mutex::new(0),
        }
    }

    /// Mint a new ephemeral peer valid for `ttl_secs`.
    pub fn issue(&self, ttl_secs: u64) -> PeerLease {
        let kp = gen_keypair();
        let id = {
            let mut s = self.seq.lock().unwrap();
            *s += 1;
            format!("wgp_{:08x}", *s)
        };
        let now = now_ms();
        let lease = PeerLease {
            id: id.clone(),
            public_key: b64(&kp.public),
            created_ms: now,
            expires_ms: now + ttl_secs * 1000,
        };
        self.issued.lock().unwrap().insert(
            id,
            Issued {
                private: kp.private,
                lease: lease.clone(),
            },
        );
        lease
    }

    /// Drop expired leases (would tear down their connectors). Returns count removed.
    pub fn gc(&self) -> usize {
        let now = now_ms();
        let mut m = self.issued.lock().unwrap();
        let before = m.len();
        m.retain(|_, v| v.lease.expires_ms > now);
        before - m.len()
    }

    pub fn active(&self) -> usize {
        self.issued.lock().unwrap().len()
    }

    pub fn revoke(&self, id: &str) -> bool {
        self.issued.lock().unwrap().remove(id).is_some()
    }

    fn private_of(&self, id: &str) -> Option<Vec<u8>> {
        self.issued
            .lock()
            .unwrap()
            .get(id)
            .map(|i| i.private.clone())
    }
}

impl Default for Kes {
    fn default() -> Self {
        Self::new()
    }
}

/// An established secure tunnel between a connector and the gateway side. Both
/// transport endpoints run in-process here so we can prove the encrypted
/// datapath end to end; in a distributed deployment the two sides sit on
/// different nodes over UDP.
pub struct SecureLink {
    initiator: TransportState,
    responder: TransportState,
    /// host:port of the private backend this link fronts.
    pub target: String,
}

impl SecureLink {
    /// Establish a tunnel to `target` with a full WireGuard handshake.
    pub fn establish(target: &str) -> Result<SecureLink, String> {
        let responder_kp = gen_keypair();
        let initiator_kp = gen_keypair();
        Self::build(
            target,
            &initiator_kp.private,
            &responder_kp.private,
            &responder_kp.public,
        )
    }

    /// Establish using a KES-issued lease as the connector (initiator) identity.
    pub fn establish_with_kes(
        kes: &Kes,
        target: &str,
        ttl_secs: u64,
    ) -> Result<(PeerLease, SecureLink), String> {
        let lease = kes.issue(ttl_secs);
        let init_priv = kes.private_of(&lease.id).ok_or("lease vanished")?;
        let responder_kp = gen_keypair();
        let link = Self::build(
            target,
            &init_priv,
            &responder_kp.private,
            &responder_kp.public,
        )?;
        Ok((lease, link))
    }

    fn build(
        target: &str,
        init_priv: &[u8],
        resp_priv: &[u8],
        resp_pub: &[u8],
    ) -> Result<SecureLink, String> {
        let psk = random_psk();
        let params = PARAMS.parse().map_err(|e| format!("params: {e}"))?;
        let mut initiator = Builder::new(params)
            .local_private_key(init_priv)
            .remote_public_key(resp_pub) // IK: initiator knows the gateway's static key
            .psk(2, &psk)
            .build_initiator()
            .map_err(|e| format!("init: {e}"))?;
        let params = PARAMS.parse().map_err(|e| format!("params: {e}"))?;
        let mut responder = Builder::new(params)
            .local_private_key(resp_priv)
            .psk(2, &psk)
            .build_responder()
            .map_err(|e| format!("resp: {e}"))?;

        let mut b1 = vec![0u8; 1024];
        let mut b2 = vec![0u8; 1024];
        let mut tmp = vec![0u8; 1024];

        // -> e, es, s, ss
        let n = initiator
            .write_message(&[], &mut b1)
            .map_err(|e| format!("msg1 write: {e}"))?;
        responder
            .read_message(&b1[..n], &mut tmp)
            .map_err(|e| format!("msg1 read: {e}"))?;
        // <- e, ee, se, psk
        let n = responder
            .write_message(&[], &mut b2)
            .map_err(|e| format!("msg2 write: {e}"))?;
        initiator
            .read_message(&b2[..n], &mut tmp)
            .map_err(|e| format!("msg2 read: {e}"))?;

        let initiator = initiator
            .into_transport_mode()
            .map_err(|e| format!("init transport: {e}"))?;
        let responder = responder
            .into_transport_mode()
            .map_err(|e| format!("resp transport: {e}"))?;
        Ok(SecureLink {
            initiator,
            responder,
            target: target.to_string(),
        })
    }

    /// Send `payload` through the tunnel (encrypted by the connector) and return
    /// what the gateway side decrypts — proving the encrypted datapath.
    pub fn roundtrip(&mut self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let ct = self.seal_c2s(payload)?;
        self.open_c2s(&ct)
    }

    /// Max plaintext per Noise message (64KiB frame minus the 16-byte tag).
    pub const MAX_CHUNK: usize = 65535 - 16;

    /// Encrypt a client→backend chunk (connector side).
    pub fn seal_c2s(&mut self, plain: &[u8]) -> Result<Vec<u8>, String> {
        let mut ct = vec![0u8; plain.len() + 16];
        let n = self
            .initiator
            .write_message(plain, &mut ct)
            .map_err(|e| format!("seal c2s: {e}"))?;
        ct.truncate(n);
        Ok(ct)
    }
    /// Decrypt a client→backend chunk (gateway side).
    pub fn open_c2s(&mut self, ct: &[u8]) -> Result<Vec<u8>, String> {
        let mut pt = vec![0u8; ct.len()];
        let n = self
            .responder
            .read_message(ct, &mut pt)
            .map_err(|e| format!("open c2s: {e}"))?;
        pt.truncate(n);
        Ok(pt)
    }
    /// Encrypt a backend→client chunk (gateway side).
    pub fn seal_s2c(&mut self, plain: &[u8]) -> Result<Vec<u8>, String> {
        let mut ct = vec![0u8; plain.len() + 16];
        let n = self
            .responder
            .write_message(plain, &mut ct)
            .map_err(|e| format!("seal s2c: {e}"))?;
        ct.truncate(n);
        Ok(ct)
    }
    /// Decrypt a backend→client chunk (connector side).
    pub fn open_s2c(&mut self, ct: &[u8]) -> Result<Vec<u8>, String> {
        let mut pt = vec![0u8; ct.len()];
        let n = self
            .initiator
            .read_message(ct, &mut pt)
            .map_err(|e| format!("open s2c: {e}"))?;
        pt.truncate(n);
        Ok(pt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypair_is_32_bytes() {
        let kp = gen_keypair();
        assert_eq!(kp.public.len(), 32);
        assert_eq!(kp.private.len(), 32);
    }

    #[test]
    fn kes_issues_and_expires() {
        let kes = Kes::new();
        let lease = kes.issue(60);
        assert!(lease.public_key.len() > 30);
        assert_eq!(kes.active(), 1);
        let _ = kes.issue(0);
        std::thread::sleep(std::time::Duration::from_millis(2));
        kes.gc();
        assert_eq!(kes.active(), 1);
    }

    #[test]
    fn handshake_and_encrypted_roundtrip() {
        let mut link = SecureLink::establish("10.0.0.5:5432").expect("establish");
        let payload = b"SELECT 1; -- private db traffic";
        let got = link.roundtrip(payload).expect("roundtrip");
        assert_eq!(got, payload, "decrypted payload must match what was sent");
    }

    #[test]
    fn kes_backed_link() {
        let kes = Kes::new();
        let (lease, mut link) =
            SecureLink::establish_with_kes(&kes, "db.internal:6379", 30).expect("kes link");
        assert!(lease.expires_ms > lease.created_ms);
        assert_eq!(link.roundtrip(b"PING").expect("roundtrip"), b"PING");
    }
}
