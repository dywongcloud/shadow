//! Demo of anonymous team/role membership (`cargo run -p hive-zkauth --example membership`).

use hive_zkauth::{prove, NullifierSet, Role, Roster, SecretKey};

fn main() {
    let owner = SecretKey::generate();
    let admin_a = SecretKey::generate();
    let admin_b = SecretKey::generate();
    let member = SecretKey::generate();
    let outsider = SecretKey::generate();

    let mut team = Roster::new();
    team.enroll(owner.public(), Role::Owner);
    team.enroll(admin_a.public(), Role::Admin);
    team.enroll(admin_b.public(), Role::Admin);
    team.enroll(member.public(), Role::Member);

    let scope = b"deployment:next-js-boilerplate-2";
    let request = b"GET / (preview access)";

    println!("Team roster: 1 owner, 2 admins, 1 member");
    println!(
        "Admin ring size (role >= Admin): {}\n",
        team.ring(Role::Admin).len()
    );

    let proof = team
        .prove_membership(&admin_b, Role::Admin, scope, request)
        .unwrap();
    println!(
        "admin_b produced a {}-byte anonymous proof",
        proof.to_bytes().len()
    );
    println!("  nullifier: {}", hex(&proof.nullifier()));

    let ok = team.verify_membership(Role::Admin, scope, request, &proof);
    println!("  edge node verifies admin access: {ok}  (cannot tell which admin)\n");
    assert!(ok);

    match team.prove_membership(&member, Role::Admin, scope, request) {
        Err(e) => println!("member tries to prove Admin -> rejected: {e}"),
        Ok(_) => panic!("member should not be able to prove Admin"),
    }

    match prove(&outsider, &team.ring(Role::Member), scope, request) {
        Err(e) => println!("outsider tries to prove Member -> rejected: {e}\n"),
        Ok(_) => panic!("outsider should not be able to prove membership"),
    }

    let mut spent = NullifierSet::new();
    println!(
        "first redemption of admin_b's proof:  {}",
        spent.redeem(&proof)
    );
    let proof2 = team
        .prove_membership(&admin_b, Role::Admin, scope, b"GET /again")
        .unwrap();
    println!(
        "second proof, same admin + scope:      {} (rejected: reuse)",
        spent.redeem(&proof2)
    );

    let other = team
        .prove_membership(&admin_b, Role::Admin, b"deployment:other", request)
        .unwrap();
    println!(
        "same admin, different scope -> different nullifier: {}",
        proof.nullifier() != other.nullifier()
    );

    println!("\nOK — anonymous role membership works end-to-end.");
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
