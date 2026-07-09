//! The `earthdistance` extension: great-circle distances on the Earth's
//! surface, over the cube-based "earth" representation.
//!
//! Requires the `cube` extension (`CREATE EXTENSION earthdistance CASCADE`
//! installs it), exactly like PostgreSQL. Points on the surface are
//! three-dimensional cube points; the implementations are transcriptions of
//! the extension's SQL definitions (earthdistance--1.1.sql): `earth()` is
//! the 6378168 m radius, `sec_to_gc`/`gc_to_sec` convert between chord
//! ("secant") and great-circle arc lengths, `ll_to_earth(lat, lon)` places a
//! latitude/longitude on the sphere, `latitude`/`longitude` read them back,
//! `earth_distance` is `sec_to_gc(cube_distance(...))`, and
//! `earth_box(location, radius)` is `cube_enlarge(location,
//! gc_to_sec(radius), 3)` — a bounding cube for `@>` radius searches that
//! may (by design, like PostgreSQL) include points slightly further than
//! the radius.
//!
//! Not implemented: the `earth` domain type itself (any cube is accepted;
//! PostgreSQL enforces the on-sphere constraint through a domain check) and
//! the point-based `<@>` operator (GuardianDB has no `point` type).

use super::{ExtCtx, ExtensionDef, RuntimeStrategy, any_null, arg_cube, arg_f64, cube, no_such};
use crate::relational::SqlValue;
use crate::sql::error::Result;

pub static DEF: ExtensionDef = ExtensionDef {
    name: "earthdistance",
    default_version: "1.2",
    comment: "calculate great-circle distances on the surface of the Earth",
    requires: &["cube"],
    functions: &[
        "earth",
        "sec_to_gc",
        "gc_to_sec",
        "ll_to_earth",
        "latitude",
        "longitude",
        "earth_distance",
        "earth_box",
    ],
    types: &[],
    gucs: &[],
    trusted: true,
    call: Some(call),
    strategy: RuntimeStrategy::Native,
};

/// PostgreSQL's assumed Earth radius in meters (`SELECT earth()` = 6378168).
const EARTH_RADIUS: f64 = 6378168.0;

/// Scalar-function entry point. All functions are strict.
fn call(_ctx: &ExtCtx, name: &str, args: &[SqlValue]) -> Result<SqlValue> {
    if any_null(args) {
        return Ok(SqlValue::Null);
    }
    match name {
        "earth" => Ok(SqlValue::Float8(EARTH_RADIUS)),
        "sec_to_gc" => Ok(SqlValue::Float8(sec_to_gc(arg_f64(args, 0, name)?))),
        "gc_to_sec" => Ok(SqlValue::Float8(gc_to_sec(arg_f64(args, 0, name)?))),
        "ll_to_earth" => {
            let lat = arg_f64(args, 0, name)?;
            let lon = arg_f64(args, 1, name)?;
            Ok(cube::point(ll_to_earth(lat, lon)))
        }
        "latitude" => {
            let (_, _, z) = surface_coords(args, name)?;
            let ratio = z / EARTH_RADIUS;
            let deg = if ratio < -1.0 {
                -90.0
            } else if ratio > 1.0 {
                90.0
            } else {
                ratio.asin().to_degrees()
            };
            Ok(SqlValue::Float8(deg))
        }
        "longitude" => {
            let (x, y, _) = surface_coords(args, name)?;
            Ok(SqlValue::Float8(y.atan2(x).to_degrees()))
        }
        "earth_distance" => {
            let a = corners(args, 0, name)?;
            let b = corners(args, 1, name)?;
            Ok(SqlValue::Float8(sec_to_gc(cube::distance(&a, &b))))
        }
        "earth_box" => {
            let c = corners(args, 0, name)?;
            let radius = arg_f64(args, 1, name)?;
            Ok(cube::enlarge(&c, gc_to_sec(radius), 3).value())
        }
        _ => Err(no_such(name)),
    }
}

/// Chord length -> great-circle arc length, clamped like the SQL definition.
fn sec_to_gc(sec: f64) -> f64 {
    if sec <= 0.0 {
        0.0
    } else if sec >= 2.0 * EARTH_RADIUS {
        std::f64::consts::PI * EARTH_RADIUS
    } else {
        2.0 * EARTH_RADIUS * (sec / (2.0 * EARTH_RADIUS)).asin()
    }
}

/// Great-circle arc length -> chord length, clamped like the SQL definition.
fn gc_to_sec(gc: f64) -> f64 {
    if gc <= 0.0 {
        0.0
    } else if gc >= std::f64::consts::PI * EARTH_RADIUS {
        2.0 * EARTH_RADIUS
    } else {
        2.0 * EARTH_RADIUS * (gc / (2.0 * EARTH_RADIUS)).sin()
    }
}

/// The 3-D point for a latitude/longitude, with the SQL definition's exact
/// operation order (`earth()*cos(radians(lat))*cos(radians(lon))`, ...).
fn ll_to_earth(lat: f64, lon: f64) -> Vec<f64> {
    let (rlat, rlon) = (lat.to_radians(), lon.to_radians());
    vec![
        EARTH_RADIUS * rlat.cos() * rlon.cos(),
        EARTH_RADIUS * rlat.cos() * rlon.sin(),
        EARTH_RADIUS * rlat.sin(),
    ]
}

/// The (x, y, z) surface coordinates of an earth cube: the first three
/// normalized lower-left coordinates, 0 when absent (like `cube_ll_coord`).
fn surface_coords(args: &[SqlValue], func: &str) -> Result<(f64, f64, f64)> {
    let c = corners(args, 0, func)?;
    Ok((c.min(0), c.min(1), c.min(2)))
}

fn corners(args: &[SqlValue], idx: usize, func: &str) -> Result<cube::Corners> {
    let (ll, ur) = arg_cube(args, idx, func)?;
    Ok(cube::Corners { ll, ur })
}

#[cfg(test)]
mod tests {
    //! Expected values generated from PostgreSQL 16.13 with earthdistance 1.2
    //! (over cube 1.5). Transcendental results are compared with a tight
    //! tolerance rather than bit-for-bit, since libm implementations may
    //! differ in the last ulp.
    use super::*;
    use crate::relational::SqlType;
    use chrono::Utc;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn invoke(name: &str, args: &[SqlValue]) -> Result<SqlValue> {
        let vars = RefCell::new(HashMap::new());
        let ctx = ExtCtx {
            now: Utc::now(),
            vars: &vars,
        };
        call(&ctx, name, args)
    }

    fn f(n: f64) -> SqlValue {
        SqlValue::Float8(n)
    }

    fn f8(v: SqlValue) -> f64 {
        match v {
            SqlValue::Float8(x) => x,
            other => panic!("expected float8, got {other:?}"),
        }
    }

    fn ll(lat: f64, lon: f64) -> SqlValue {
        invoke("ll_to_earth", &[f(lat), f(lon)]).unwrap()
    }

    fn close(actual: f64, expected: f64, tol: f64) {
        assert!(
            (actual - expected).abs() <= tol,
            "expected {expected}, got {actual} (tolerance {tol})"
        );
    }

    #[test]
    fn earth_radius_matches_pg() {
        assert_eq!(f8(invoke("earth", &[]).unwrap()), 6378168.0);
    }

    #[test]
    fn ll_to_earth_matches_pg() {
        // PG: ll_to_earth(0,0) => (6378168, 0, 0) — exact.
        assert_eq!(ll(0.0, 0.0).to_text().unwrap(), "(6378168, 0, 0)");
        // PG: ll_to_earth(45,45) => (3189084.000000001, 3189084, 4510045.844347039)
        match ll(45.0, 45.0) {
            SqlValue::Cube { ll: p, .. } => {
                close(p[0], 3189084.000000001, 1e-3);
                close(p[1], 3189084.0, 1e-3);
                close(p[2], 4510045.844347039, 1e-3);
            }
            other => panic!("expected cube, got {other:?}"),
        }
    }

    #[test]
    fn latitude_longitude_round_trip() {
        // PG: latitude(ll_to_earth(45,100)) => 44.99999999999999
        close(
            f8(invoke("latitude", &[ll(45.0, 100.0)]).unwrap()),
            45.0,
            1e-9,
        );
        assert_eq!(f8(invoke("longitude", &[ll(45.0, 100.0)]).unwrap()), 100.0);
        // PG-verified against Sydney's coordinates.
        close(
            f8(invoke("latitude", &[ll(-33.8688, 151.2093)]).unwrap()),
            -33.8688,
            1e-9,
        );
        close(
            f8(invoke("longitude", &[ll(-33.8688, 151.2093)]).unwrap()),
            151.2093,
            1e-9,
        );
        // Clamped poles.
        close(
            f8(invoke("latitude", &[ll(90.0, 0.0)]).unwrap()),
            90.0,
            1e-9,
        );
    }

    #[test]
    fn earth_distance_matches_pg() {
        // PG: earth_distance(ll_to_earth(0,0), ll_to_earth(0,180))
        //     => 20037605.732161503 (half the circumference)
        close(
            f8(invoke("earth_distance", &[ll(0.0, 0.0), ll(0.0, 180.0)]).unwrap()),
            20037605.732161503,
            1e-3,
        );
        // PG: London -> New York => 5576489.226133242
        close(
            f8(invoke(
                "earth_distance",
                &[ll(51.5074, -0.1278), ll(40.7128, -74.0060)],
            )
            .unwrap()),
            5576489.226133242,
            1e-3,
        );
        // PG: Paris -> Berlin => 878450.5582390272
        close(
            f8(invoke(
                "earth_distance",
                &[ll(48.8566, 2.3522), ll(52.5200, 13.4050)],
            )
            .unwrap()),
            878450.5582390272,
            1e-3,
        );
        assert_eq!(
            f8(invoke("earth_distance", &[ll(10.0, 10.0), ll(10.0, 10.0)]).unwrap()),
            0.0
        );
    }

    #[test]
    fn sec_gc_conversions_match_pg() {
        // PG: sec_to_gc(0)=0; sec_to_gc(2*earth()+1)=20037605.732161503;
        //     sec_to_gc(1000000)=1001027.0713076061
        assert_eq!(f8(invoke("sec_to_gc", &[f(0.0)]).unwrap()), 0.0);
        close(
            f8(invoke("sec_to_gc", &[f(2.0 * 6378168.0 + 1.0)]).unwrap()),
            20037605.732161503,
            1e-3,
        );
        close(
            f8(invoke("sec_to_gc", &[f(1_000_000.0)]).unwrap()),
            1001027.0713076061,
            1e-3,
        );
        // PG: gc_to_sec(0)=0; gc_to_sec(pi()*earth()+1)=12756336;
        //     gc_to_sec(1000000)=998976.0861827147
        assert_eq!(f8(invoke("gc_to_sec", &[f(0.0)]).unwrap()), 0.0);
        assert_eq!(
            f8(invoke("gc_to_sec", &[f(std::f64::consts::PI * 6378168.0 + 1.0)]).unwrap()),
            12756336.0
        );
        close(
            f8(invoke("gc_to_sec", &[f(1_000_000.0)]).unwrap()),
            998976.0861827147,
            1e-3,
        );
    }

    #[test]
    fn earth_box_bounds_radius_searches() {
        // PG-verified: the 1 km box around (0,0) contains a point ~157 m
        // away and not one ~157 km away.
        let bx = invoke("earth_box", &[ll(0.0, 0.0), f(1000.0)]).unwrap();
        let near = ll(0.001, 0.001);
        let far = ll(1.0, 1.0);
        let contains = |a: &SqlValue, b: &SqlValue| {
            super::super::cube::operator("@>", a, b)
                .unwrap()
                .to_text()
                .unwrap()
        };
        assert_eq!(contains(&bx, &near), "t");
        assert_eq!(contains(&bx, &far), "f");
    }

    #[test]
    fn null_arguments_yield_null() {
        assert!(invoke("sec_to_gc", &[SqlValue::Null]).unwrap().is_null());
        assert!(
            invoke("ll_to_earth", &[f(1.0), SqlValue::Null])
                .unwrap()
                .is_null()
        );
        assert!(
            invoke("earth_distance", &[SqlValue::Null, ll(0.0, 0.0)])
                .unwrap()
                .is_null()
        );
    }

    #[test]
    fn every_registered_function_is_routed() {
        for name in DEF.functions {
            let args = match *name {
                "earth" => vec![],
                "sec_to_gc" | "gc_to_sec" => vec![f(1.0)],
                "ll_to_earth" => vec![f(1.0), f(2.0)],
                "earth_distance" => vec![ll(0.0, 0.0), ll(1.0, 1.0)],
                "earth_box" => vec![ll(0.0, 0.0), f(1000.0)],
                _ => vec![ll(0.0, 0.0)],
            };
            assert!(invoke(name, &args).is_ok(), "{name} not routed");
        }
    }

    #[test]
    fn requires_cube_in_the_registry() {
        assert_eq!(DEF.requires, ["cube"]);
    }

    #[test]
    fn accepts_text_cube_arguments() {
        let v = SqlValue::Text("(6378168, 0, 0)".into());
        close(f8(invoke("latitude", &[v]).unwrap()), 0.0, 1e-12);
        let bad = SqlValue::from_text("a=>1", &SqlType::HStore).unwrap();
        assert!(invoke("latitude", &[bad]).is_err());
    }
}
