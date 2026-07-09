//! Native implementation of PostgreSQL's `uuid-ossp` extension.
//!
//! Provides the complete uuid-ossp function surface: the constant generators
//! (`uuid_nil` and the four RFC 4122 namespace UUIDs), the time-based
//! generators (`uuid_generate_v1`, `uuid_generate_v1mc`), the random generator
//! (`uuid_generate_v4`), and the name-based hashing generators
//! (`uuid_generate_v3` over MD5, `uuid_generate_v5` over SHA-1).
//!
//! One deliberate deviation from PostgreSQL: hardware MAC addresses are never
//! read, so `uuid_generate_v1` uses a fresh random 6-byte node per call (the
//! same fallback PostgreSQL uses when no MAC is available), and
//! `uuid_generate_v1mc` forces the multicast bit on that random node exactly
//! as PostgreSQL does. All functions are strict: any SQL NULL argument yields
//! SQL NULL.

use super::{ExtCtx, ExtensionDef, RuntimeStrategy, any_null, arg_text, no_such};
use crate::relational::SqlValue;
use crate::sql::error::{Result, SqlError};
use uuid::Uuid;

pub static DEF: ExtensionDef = ExtensionDef {
    name: "uuid-ossp",
    default_version: "1.1",
    comment: "generate universally unique identifiers (UUIDs)",
    requires: &[],
    functions: &[
        "uuid_nil",
        "uuid_ns_dns",
        "uuid_ns_url",
        "uuid_ns_oid",
        "uuid_ns_x500",
        "uuid_generate_v1",
        "uuid_generate_v1mc",
        "uuid_generate_v3",
        "uuid_generate_v4",
        "uuid_generate_v5",
    ],
    types: &[],
    gucs: &[],
    trusted: true,
    call: Some(call),
    strategy: RuntimeStrategy::Native,
};

fn call(_ctx: &ExtCtx, name: &str, args: &[SqlValue]) -> Result<SqlValue> {
    match name {
        "uuid_nil" => Ok(SqlValue::Uuid(Uuid::nil())),
        "uuid_ns_dns" => Ok(SqlValue::Uuid(Uuid::NAMESPACE_DNS)),
        "uuid_ns_url" => Ok(SqlValue::Uuid(Uuid::NAMESPACE_URL)),
        "uuid_ns_oid" => Ok(SqlValue::Uuid(Uuid::NAMESPACE_OID)),
        "uuid_ns_x500" => Ok(SqlValue::Uuid(Uuid::NAMESPACE_X500)),
        "uuid_generate_v1" => Ok(SqlValue::Uuid(Uuid::now_v1(&random_node()))),
        "uuid_generate_v1mc" => Ok(SqlValue::Uuid(Uuid::now_v1(&multicast_node()))),
        "uuid_generate_v3" => generate_hashed(args, name, Uuid::new_v3),
        "uuid_generate_v4" => Ok(SqlValue::Uuid(Uuid::new_v4())),
        "uuid_generate_v5" => generate_hashed(args, name, Uuid::new_v5),
        _ => Err(no_such(name)),
    }
}

/// Shared body of `uuid_generate_v3` / `uuid_generate_v5`:
/// `(namespace uuid, name text) -> uuid` with strict NULL handling.
fn generate_hashed(
    args: &[SqlValue],
    func: &str,
    make: fn(&Uuid, &[u8]) -> Uuid,
) -> Result<SqlValue> {
    if any_null(args) {
        return Ok(SqlValue::Null);
    }
    let namespace = arg_uuid(args, 0, func)?;
    let name = arg_text(args, 1, func)?;
    Ok(SqlValue::Uuid(make(&namespace, name.as_bytes())))
}

/// Extract a uuid argument at `idx`; text arguments are parsed like the
/// engine's `text -> uuid` cast.
fn arg_uuid(args: &[SqlValue], idx: usize, func: &str) -> Result<Uuid> {
    match args.get(idx) {
        Some(SqlValue::Uuid(u)) => Ok(*u),
        Some(SqlValue::Text(s)) | Some(SqlValue::Citext(s)) => {
            Uuid::parse_str(s.trim()).map_err(|_| SqlError::InvalidTextRepresentation {
                ty: "uuid".into(),
                value: s.clone(),
            })
        }
        Some(other) => Err(SqlError::CannotCoerce {
            from: other.type_of().name(),
            to: format!("uuid (argument {} of {func})", idx + 1),
        }),
        None => Err(SqlError::UndefinedFunction(format!(
            "{func}: missing argument {}",
            idx + 1
        ))),
    }
}

/// Fresh random node identifier, standing in for a MAC address.
fn random_node() -> [u8; 6] {
    rand::random::<[u8; 6]>()
}

/// Random node with the multicast bit set (bit 0 of the first octet), so it
/// can never collide with a real unicast MAC address — PostgreSQL's
/// `uuid_generate_v1mc` semantics.
fn multicast_node() -> [u8; 6] {
    let mut node = random_node();
    node[0] |= 0x01;
    node
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn run(name: &str, args: &[SqlValue]) -> Result<SqlValue> {
        let vars = RefCell::new(HashMap::new());
        let ctx = ExtCtx {
            now: chrono::Utc::now(),
            vars: &vars,
        };
        call(&ctx, name, args)
    }

    fn uuid_of(v: SqlValue) -> Uuid {
        match v {
            SqlValue::Uuid(u) => u,
            other => panic!("expected uuid, got {other:?}"),
        }
    }

    #[test]
    fn nil_and_namespace_constants() {
        let cases = [
            ("uuid_nil", "00000000-0000-0000-0000-000000000000"),
            ("uuid_ns_dns", "6ba7b810-9dad-11d1-80b4-00c04fd430c8"),
            ("uuid_ns_url", "6ba7b811-9dad-11d1-80b4-00c04fd430c8"),
            ("uuid_ns_oid", "6ba7b812-9dad-11d1-80b4-00c04fd430c8"),
            ("uuid_ns_x500", "6ba7b814-9dad-11d1-80b4-00c04fd430c8"),
        ];
        for (func, want) in cases {
            let got = uuid_of(run(func, &[]).unwrap());
            assert_eq!(got.to_string(), want, "{func}");
        }
    }

    #[test]
    fn v3_and_v5_match_postgresql() {
        // Reference outputs from PostgreSQL:
        //   SELECT uuid_generate_v3(uuid_ns_dns(), 'www.example.com');
        //   SELECT uuid_generate_v5(uuid_ns_dns(), 'www.example.com');
        let ns = SqlValue::Uuid(Uuid::NAMESPACE_DNS);
        let name = SqlValue::Text("www.example.com".into());
        let v3 = uuid_of(run("uuid_generate_v3", &[ns.clone(), name.clone()]).unwrap());
        assert_eq!(v3.to_string(), "5df41881-3aed-3515-88a7-2f4a814cf09e");
        let v5 = uuid_of(run("uuid_generate_v5", &[ns, name]).unwrap());
        assert_eq!(v5.to_string(), "2ed6657d-e927-568b-95e1-2665a8aea6a2");
    }

    #[test]
    fn namespace_argument_accepts_text() {
        let ns = SqlValue::Text("6ba7b810-9dad-11d1-80b4-00c04fd430c8".into());
        let name = SqlValue::Text("www.example.com".into());
        let v5 = uuid_of(run("uuid_generate_v5", &[ns, name]).unwrap());
        assert_eq!(v5.to_string(), "2ed6657d-e927-568b-95e1-2665a8aea6a2");
    }

    #[test]
    fn v4_version_and_variant_bits() {
        let u = uuid_of(run("uuid_generate_v4", &[]).unwrap());
        assert_eq!(u.get_version_num(), 4);
        assert_eq!(u.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn v1_is_time_based_and_unique_per_call() {
        let a = uuid_of(run("uuid_generate_v1", &[]).unwrap());
        let b = uuid_of(run("uuid_generate_v1", &[]).unwrap());
        assert_eq!(a.get_version_num(), 1);
        assert_eq!(a.get_variant(), uuid::Variant::RFC4122);
        assert_ne!(a, b);
    }

    #[test]
    fn v1mc_node_always_has_multicast_bit() {
        // The node identifier is the trailing 6 bytes (octets 10..16); the
        // multicast flag is bit 0 of its first octet. A plain-random node
        // would only set it half the time, so check several draws.
        for _ in 0..16 {
            let u = uuid_of(run("uuid_generate_v1mc", &[]).unwrap());
            assert_eq!(u.get_version_num(), 1);
            assert_eq!(u.as_bytes()[10] & 0x01, 0x01, "multicast bit must be set");
        }
    }

    #[test]
    fn v3_and_v5_are_strict_on_null() {
        let ns = SqlValue::Uuid(Uuid::NAMESPACE_DNS);
        let name = SqlValue::Text("www.example.com".into());
        for func in ["uuid_generate_v3", "uuid_generate_v5"] {
            let r = run(func, &[SqlValue::Null, name.clone()]).unwrap();
            assert!(r.is_null(), "{func}(NULL, name) must be NULL");
            let r = run(func, &[ns.clone(), SqlValue::Null]).unwrap();
            assert!(r.is_null(), "{func}(ns, NULL) must be NULL");
        }
    }

    #[test]
    fn malformed_namespace_text_is_typed_error() {
        let r = run(
            "uuid_generate_v3",
            &[
                SqlValue::Text("not-a-uuid".into()),
                SqlValue::Text("x".into()),
            ],
        );
        assert!(matches!(r, Err(SqlError::InvalidTextRepresentation { .. })));
    }

    #[test]
    fn non_uuid_namespace_cannot_coerce() {
        let r = run(
            "uuid_generate_v5",
            &[SqlValue::Int4(7), SqlValue::Text("x".into())],
        );
        assert!(matches!(r, Err(SqlError::CannotCoerce { .. })));
    }

    #[test]
    fn unknown_function_name_is_not_routed() {
        assert!(matches!(
            run("uuid_generate_v2", &[]),
            Err(SqlError::Internal(_))
        ));
    }

    #[test]
    fn def_lists_every_dispatched_function() {
        for func in DEF.functions {
            let args = match *func {
                "uuid_generate_v3" | "uuid_generate_v5" => vec![
                    SqlValue::Uuid(Uuid::NAMESPACE_DNS),
                    SqlValue::Text("www.example.com".into()),
                ],
                _ => vec![],
            };
            let v = run(func, &args).unwrap_or_else(|e| panic!("{func}: {e}"));
            assert!(matches!(v, SqlValue::Uuid(_)), "{func} must yield a uuid");
        }
    }
}
