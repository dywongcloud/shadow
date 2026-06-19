//! # hive-zkauth — anonymous team/role membership (EXPERIMENT)
//!
//! A self-contained proof-of-concept for **anonymous membership proofs**: a user
//! proves they belong to a set (a team, or the subset of members with role ≥ R)
//! **without revealing which member they are**, and emits a per-scope
//! **nullifier** so a verifier can rate-limit / detect reuse without learning the
//! prover's identity.
//!
//! This is the "Semaphore/group-signature" idea from the ZK discussion, built
//! here as a **linkable ring signature** (LSAG-style) over Ristretto255 — a
//! genuine, sound, zero-knowledge-of-identity primitive that needs no SNARK
//! toolchain. Trade-off vs a Semaphore SNARK: proof size is O(ring size) rather
//! than O(1), and the anonymity set is the explicit ring (a team is small, so
//! that's fine). Verification stays cheap; proving is sub-millisecond for small
//! rings — i.e. usable on a request path, unlike zkVM execution proofs.
//!
//! **Safety:** this crate is standalone. Nothing in the platform depends on it;
//! it's an isolated experiment. Integrating it (e.g. into the preview gate) would
//! be a deliberate, separate step.
//!
//! ## How it maps to teams + roles
//! - Each member holds a [`SecretKey`]; its [`PublicKey`] is enrolled in a team
//!   [`Roster`] under a [`Role`].
//! - To access something requiring role ≥ R, the prover signs over the **ring of
//!   all members with role ≥ R** ([`Roster::ring`]). A valid proof means "I am
//!   one of them" — without saying who.
//! - The proof carries a `nullifier`, bound to a `scope` (e.g. the deployment),
//!   so reuse within a scope is detectable while staying unlinkable across scopes.

use curve25519_dalek::{
    constants::RISTRETTO_BASEPOINT_POINT, ristretto::CompressedRistretto, RistrettoPoint, Scalar,
};
use rand_core::OsRng;
use sha2::{Digest, Sha512};

const H2P_DOMAIN: &[u8] = b"hive-zkauth/v1/hash-to-point";
const RING_DOMAIN: &[u8] = b"hive-zkauth/v1/ring";
const CHAL_DOMAIN: &[u8] = b"hive-zkauth/v1/challenge";

/// Errors from proving/verifying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The signer's public key is not present in the ring.
    NotInRing,
    /// The ring is empty.
    EmptyRing,
    /// Serialized bytes were malformed.
    Malformed,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotInRing => write!(f, "signer is not a member of the ring"),
            Error::EmptyRing => write!(f, "ring is empty"),
            Error::Malformed => write!(f, "malformed bytes"),
        }
    }
}
impl std::error::Error for Error {}

/// A member's secret key.
#[derive(Clone)]
pub struct SecretKey(Scalar);

/// A member's public key (enrolled in a team roster).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PublicKey(RistrettoPoint);

impl SecretKey {
    /// Generate a fresh random member key.
    pub fn generate() -> SecretKey {
        SecretKey(Scalar::random(&mut OsRng))
    }
    /// The corresponding public key.
    pub fn public(&self) -> PublicKey {
        PublicKey(RISTRETTO_BASEPOINT_POINT * self.0)
    }
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }
    pub fn from_bytes(b: &[u8; 32]) -> Result<SecretKey, Error> {
        Option::<Scalar>::from(Scalar::from_canonical_bytes(*b))
            .map(SecretKey)
            .ok_or(Error::Malformed)
    }
}

impl PublicKey {
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.compress().to_bytes()
    }
    pub fn from_bytes(b: &[u8; 32]) -> Result<PublicKey, Error> {
        CompressedRistretto(*b).decompress().map(PublicKey).ok_or(Error::Malformed)
    }
}

/// An anonymous membership proof (linkable ring signature).
#[derive(Clone)]
pub struct Proof {
    c0: Scalar,
    s: Vec<Scalar>,
    key_image: RistrettoPoint,
}

impl Proof {
    /// The per-scope nullifier — equal across proofs from the SAME member in the
    /// SAME scope (enables reuse/rate-limit detection), but unlinkable across
    /// scopes and never reveals which member produced it.
    pub fn nullifier(&self) -> [u8; 32] {
        self.key_image.compress().to_bytes()
    }

    /// Serialize: c0 (32) ‖ key_image (32) ‖ s[0..n] (32 each).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.s.len() * 32);
        out.extend_from_slice(&self.c0.to_bytes());
        out.extend_from_slice(&self.key_image.compress().to_bytes());
        for si in &self.s {
            out.extend_from_slice(&si.to_bytes());
        }
        out
    }

    pub fn from_bytes(b: &[u8]) -> Result<Proof, Error> {
        if b.len() < 64 || (b.len() - 64) % 32 != 0 {
            return Err(Error::Malformed);
        }
        let c0 = scalar_from(&b[0..32])?;
        let key_image = CompressedRistretto(arr(&b[32..64])).decompress().ok_or(Error::Malformed)?;
        let mut s = Vec::new();
        let mut off = 64;
        while off < b.len() {
            s.push(scalar_from(&b[off..off + 32])?);
            off += 32;
        }
        Ok(Proof { c0, s, key_image })
    }
}

/// Prove anonymous membership in `ring` for `scope`, signing `message`.
///
/// `scope` namespaces the nullifier (e.g. a deployment id / topic). The proof
/// reveals nothing about which ring member signed — only that one of them did.
pub fn prove(secret: &SecretKey, ring: &[PublicKey], scope: &[u8], message: &[u8]) -> Result<Proof, Error> {
    let n = ring.len();
    if n == 0 {
        return Err(Error::EmptyRing);
    }
    let me = secret.public();
    let pi = ring.iter().position(|p| *p == me).ok_or(Error::NotInRing)?;

    let rh = ring_hash(ring);
    // Per-key hash-to-point, scoped — used uniformly in the ring equations so the
    // key image binds the nullifier to (member, scope).
    let hp: Vec<RistrettoPoint> = ring.iter().map(|p| hash_to_point(p, scope)).collect();
    let key_image = secret.0 * hp[pi];

    let mut c = vec![Scalar::ZERO; n];
    let mut s = vec![Scalar::ZERO; n];

    // Start the chain at the signer with a random commitment `u`.
    let u = Scalar::random(&mut OsRng);
    let l = RISTRETTO_BASEPOINT_POINT * u;
    let r = hp[pi] * u;
    c[(pi + 1) % n] = challenge(&rh, scope, message, &l, &r);

    // Walk the rest of the ring with random responses.
    let mut i = (pi + 1) % n;
    while i != pi {
        s[i] = Scalar::random(&mut OsRng);
        let l = RISTRETTO_BASEPOINT_POINT * s[i] + ring[i].0 * c[i];
        let r = hp[i] * s[i] + key_image * c[i];
        c[(i + 1) % n] = challenge(&rh, scope, message, &l, &r);
        i = (i + 1) % n;
    }
    // Close the ring at the signer.
    s[pi] = u - c[pi] * secret.0;

    Ok(Proof { c0: c[0], s, key_image })
}

/// Verify an anonymous membership proof against `ring`, `scope` and `message`.
pub fn verify(ring: &[PublicKey], scope: &[u8], message: &[u8], proof: &Proof) -> bool {
    let n = ring.len();
    if n == 0 || proof.s.len() != n {
        return false;
    }
    let rh = ring_hash(ring);
    let mut c = proof.c0;
    for i in 0..n {
        let hp_i = hash_to_point(&ring[i], scope);
        let l = RISTRETTO_BASEPOINT_POINT * proof.s[i] + ring[i].0 * c;
        let r = hp_i * proof.s[i] + proof.key_image * c;
        c = challenge(&rh, scope, message, &l, &r);
    }
    // Constant-time scalar equality (dalek's PartialEq is constant-time).
    c == proof.c0
}

// ---- internal helpers ----

fn ring_hash(ring: &[PublicKey]) -> [u8; 64] {
    let mut h = Sha512::new();
    h.update(RING_DOMAIN);
    h.update((ring.len() as u64).to_le_bytes());
    for p in ring {
        h.update(p.0.compress().as_bytes());
    }
    let mut out = [0u8; 64];
    out.copy_from_slice(&h.finalize());
    out
}

fn hash_to_point(p: &PublicKey, scope: &[u8]) -> RistrettoPoint {
    let mut h = Sha512::new();
    h.update(H2P_DOMAIN);
    h.update(p.0.compress().as_bytes());
    h.update((scope.len() as u64).to_le_bytes());
    h.update(scope);
    let mut wide = [0u8; 64];
    wide.copy_from_slice(&h.finalize());
    RistrettoPoint::from_uniform_bytes(&wide)
}

fn challenge(ring_h: &[u8; 64], scope: &[u8], msg: &[u8], l: &RistrettoPoint, r: &RistrettoPoint) -> Scalar {
    let mut h = Sha512::new();
    h.update(CHAL_DOMAIN);
    h.update(ring_h);
    h.update((scope.len() as u64).to_le_bytes());
    h.update(scope);
    h.update((msg.len() as u64).to_le_bytes());
    h.update(msg);
    h.update(l.compress().as_bytes());
    h.update(r.compress().as_bytes());
    let mut wide = [0u8; 64];
    wide.copy_from_slice(&h.finalize());
    Scalar::from_bytes_mod_order_wide(&wide)
}

fn arr(b: &[u8]) -> [u8; 32] {
    let mut a = [0u8; 32];
    a.copy_from_slice(b);
    a
}
fn scalar_from(b: &[u8]) -> Result<Scalar, Error> {
    Option::<Scalar>::from(Scalar::from_canonical_bytes(arr(b))).ok_or(Error::Malformed)
}

// ============================ team / role layer ============================

/// Team roles, ordered (Owner is highest).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Viewer = 0,
    Member = 1,
    Admin = 2,
    Owner = 3,
}

/// A team's public roster: each member's [`PublicKey`] with their [`Role`].
/// Only public keys are stored — secrets never leave the member.
#[derive(Clone, Default)]
pub struct Roster {
    members: Vec<(PublicKey, Role)>,
}

impl Roster {
    pub fn new() -> Roster {
        Roster::default()
    }
    pub fn enroll(&mut self, public: PublicKey, role: Role) {
        // Replace existing entry for this key, else add.
        if let Some(e) = self.members.iter_mut().find(|(p, _)| *p == public) {
            e.1 = role;
        } else {
            self.members.push((public, role));
        }
    }
    pub fn revoke(&mut self, public: &PublicKey) {
        self.members.retain(|(p, _)| p != public);
    }

    /// The ring for "role ≥ `min`": every member who satisfies the threshold.
    /// Sorted by key bytes so provers and verifiers agree on ring order.
    pub fn ring(&self, min: Role) -> Vec<PublicKey> {
        let mut ring: Vec<PublicKey> = self.members.iter().filter(|(_, r)| *r >= min).map(|(p, _)| *p).collect();
        ring.sort_by_key(|p| p.to_bytes());
        ring
    }

    /// Prove "I hold role ≥ `min` in this team", anonymously, for `scope`/`message`.
    pub fn prove_membership(&self, secret: &SecretKey, min: Role, scope: &[u8], message: &[u8]) -> Result<Proof, Error> {
        prove(secret, &self.ring(min), scope, message)
    }

    /// Verify a role-membership proof against the current roster.
    pub fn verify_membership(&self, min: Role, scope: &[u8], message: &[u8], proof: &Proof) -> bool {
        verify(&self.ring(min), scope, message, proof)
    }
}

/// Tracks spent nullifiers per scope, so a proof can't be replayed within a
/// scope (rate-limiting / single-use) — without ever learning the prover.
#[derive(Default)]
pub struct NullifierSet {
    seen: std::collections::HashSet<[u8; 32]>,
}
impl NullifierSet {
    pub fn new() -> NullifierSet {
        NullifierSet::default()
    }
    /// Record a proof's nullifier. Returns `true` if it's fresh (accept), `false`
    /// if it was already used (reject / rate-limit).
    pub fn redeem(&mut self, proof: &Proof) -> bool {
        self.seen.insert(proof.nullifier())
    }
    pub fn contains(&self, proof: &Proof) -> bool {
        self.seen.contains(&proof.nullifier())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team(n: usize) -> (Vec<SecretKey>, Roster) {
        let mut secrets = Vec::new();
        let mut roster = Roster::new();
        for i in 0..n {
            let sk = SecretKey::generate();
            let role = if i == 0 { Role::Owner } else if i < 3 { Role::Admin } else { Role::Member };
            roster.enroll(sk.public(), role);
            secrets.push(sk);
        }
        (secrets, roster)
    }

    #[test]
    fn prove_and_verify_roundtrip() {
        let (sk, roster) = team(6);
        let p = roster.prove_membership(&sk[4], Role::Member, b"deploy:app", b"GET /").unwrap();
        assert!(roster.verify_membership(Role::Member, b"deploy:app", b"GET /", &p));
    }

    #[test]
    fn role_threshold_enforced() {
        let (sk, roster) = team(6);
        // A plain Member (index 4) cannot produce an Admin-ring proof — they're
        // not in that ring.
        assert_eq!(roster.prove_membership(&sk[4], Role::Admin, b"s", b"m").err(), Some(Error::NotInRing));
        // An Owner (index 0) can prove Admin-level access (Owner ≥ Admin).
        let p = roster.prove_membership(&sk[0], Role::Admin, b"s", b"m").unwrap();
        assert!(roster.verify_membership(Role::Admin, b"s", b"m", &p));
    }

    #[test]
    fn non_member_cannot_prove() {
        let (_sk, roster) = team(5);
        let outsider = SecretKey::generate();
        assert_eq!(prove(&outsider, &roster.ring(Role::Member), b"s", b"m").err(), Some(Error::NotInRing));
    }

    #[test]
    fn tamper_is_rejected() {
        let (sk, roster) = team(5);
        let p = roster.prove_membership(&sk[3], Role::Member, b"scope", b"msg").unwrap();
        assert!(!roster.verify_membership(Role::Member, b"scope", b"DIFFERENT", &p));
        assert!(!roster.verify_membership(Role::Member, b"OTHER-SCOPE", b"msg", &p));
        assert!(!roster.verify_membership(Role::Admin, b"scope", b"msg", &p)); // different ring
    }

    #[test]
    fn nullifier_links_within_scope_only() {
        let (sk, roster) = team(6);
        // Same member, same scope → same nullifier (reuse detectable).
        let a = roster.prove_membership(&sk[4], Role::Member, b"scope-1", b"m1").unwrap();
        let b = roster.prove_membership(&sk[4], Role::Member, b"scope-1", b"m2").unwrap();
        assert_eq!(a.nullifier(), b.nullifier());
        // Same member, different scope → different nullifier (unlinkable).
        let c = roster.prove_membership(&sk[4], Role::Member, b"scope-2", b"m1").unwrap();
        assert_ne!(a.nullifier(), c.nullifier());
        // Different member, same scope → different nullifier.
        let d = roster.prove_membership(&sk[5], Role::Member, b"scope-1", b"m1").unwrap();
        assert_ne!(a.nullifier(), d.nullifier());
    }

    #[test]
    fn anonymity_two_signers_both_verify_indistinguishably() {
        let (sk, roster) = team(8);
        // Two different members produce valid proofs for the same ring/scope/msg;
        // verification can't tell which one signed (no index is revealed).
        let p4 = roster.prove_membership(&sk[4], Role::Member, b"s", b"m").unwrap();
        let p7 = roster.prove_membership(&sk[7], Role::Member, b"s", b"m").unwrap();
        assert!(roster.verify_membership(Role::Member, b"s", b"m", &p4));
        assert!(roster.verify_membership(Role::Member, b"s", b"m", &p7));
        assert_ne!(p4.nullifier(), p7.nullifier());
    }

    #[test]
    fn nullifier_set_detects_reuse() {
        let (sk, roster) = team(5);
        let mut spent = NullifierSet::new();
        let p = roster.prove_membership(&sk[3], Role::Member, b"vote", b"once").unwrap();
        assert!(spent.redeem(&p)); // first use accepted
        let again = roster.prove_membership(&sk[3], Role::Member, b"vote", b"twice").unwrap();
        assert!(!spent.redeem(&again)); // same member+scope → rejected
    }

    #[test]
    fn proof_serialization_roundtrips() {
        let (sk, roster) = team(5);
        let p = roster.prove_membership(&sk[2], Role::Member, b"s", b"m").unwrap();
        let bytes = p.to_bytes();
        let p2 = Proof::from_bytes(&bytes).unwrap();
        assert_eq!(p.nullifier(), p2.nullifier());
        assert!(roster.verify_membership(Role::Member, b"s", b"m", &p2));
    }

    #[test]
    fn key_serialization_roundtrips() {
        let sk = SecretKey::generate();
        let sk2 = SecretKey::from_bytes(&sk.to_bytes()).unwrap();
        assert_eq!(sk.public(), sk2.public());
        let pk2 = PublicKey::from_bytes(&sk.public().to_bytes()).unwrap();
        assert_eq!(sk.public(), pk2);
    }
}
