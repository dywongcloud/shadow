//! Live witness for `p2p-raw-datagram-truncation` — exercises the EOF
//! classification of `read_raw_datagram` at the byte level: a clean EOF at a
//! frame boundary is `Ok(None)`, but a peer dying mid-frame (partial length
//! prefix or partial payload) must surface as `Err(InvalidData "truncated
//! datagram frame")`, never a silent clean close. Also covers the happy path,
//! the oversize cap on both directions, and zero-length datagrams.
//!
//! Usage: `cargo run -p hive-p2p --example raw_datagram_eof_witness`
//! Exit code 0 = every witness line passed; 1 = at least one failed.

use hive_p2p::{read_raw_datagram, write_raw_datagram, RAW_MAX_DATAGRAM};

fn check(name: &str, ok: bool) -> bool {
    println!(
        "{}: {}",
        if ok { "WITNESS_OK" } else { "WITNESS_FAIL" },
        name
    );
    ok
}

#[tokio::main]
async fn main() {
    let mut all = true;

    // 1. Clean EOF at a frame boundary -> Ok(None).
    let mut r: &[u8] = &[];
    let res = read_raw_datagram(&mut r).await;
    all &= check("clean_eof_at_boundary_is_none", matches!(res, Ok(None)));

    // 2. One valid frame -> Ok(Some(payload)).
    let mut frame = Vec::new();
    write_raw_datagram(&mut frame, b"hello-mesh").await.unwrap();
    let mut r: &[u8] = &frame;
    let res = read_raw_datagram(&mut r).await;
    all &= check(
        "valid_frame_roundtrip",
        matches!(&res, Ok(Some(d)) if d == b"hello-mesh"),
    );

    // 3. Two frames back-to-back, then clean EOF.
    let mut r: &[u8] = &frame;
    let first = read_raw_datagram(&mut r).await;
    let second_eof = read_raw_datagram(&mut r).await;
    all &= check(
        "frame_then_clean_eof",
        matches!(&first, Ok(Some(d)) if d == b"hello-mesh") && matches!(second_eof, Ok(None)),
    );

    // 4. EOF after 2 bytes of the length prefix -> truncated, NOT None.
    let mut r: &[u8] = &[0x00, 0x00];
    let res = read_raw_datagram(&mut r).await;
    all &= check(
        "partial_prefix_is_truncated_error",
        matches!(&res, Err(e)
            if e.kind() == std::io::ErrorKind::InvalidData
            && e.to_string().contains("truncated datagram frame")),
    );

    // 5. Valid prefix (len 4096) + only 100 payload bytes -> truncated.
    let mut buf = (4096u32).to_be_bytes().to_vec();
    buf.extend(std::iter::repeat(0u8).take(100));
    let mut r: &[u8] = &buf;
    let res = read_raw_datagram(&mut r).await;
    all &= check(
        "partial_payload_is_truncated_error",
        matches!(&res, Err(e)
            if e.kind() == std::io::ErrorKind::InvalidData
            && e.to_string().contains("truncated datagram frame")),
    );

    // 6. Oversize length prefix -> clean refusal.
    let mut r: &[u8] = &((RAW_MAX_DATAGRAM as u32 + 1).to_be_bytes())[..];
    let res = read_raw_datagram(&mut r).await;
    all &= check(
        "oversize_read_refused",
        matches!(&res, Err(e) if e.to_string().contains("frame too large")),
    );

    // 7. Oversize write -> refused before any bytes hit the sink.
    let big = vec![0u8; RAW_MAX_DATAGRAM + 1];
    let mut sink = Vec::new();
    let res = write_raw_datagram(&mut sink, &big).await;
    all &= check("oversize_write_refused", res.is_err() && sink.is_empty());

    // 8. Zero-length datagram round-trips as Some(empty), distinct from EOF.
    let mut frame = Vec::new();
    write_raw_datagram(&mut frame, &[]).await.unwrap();
    let mut r: &[u8] = &frame;
    let res = read_raw_datagram(&mut r).await;
    all &= check(
        "zero_length_datagram_is_some_empty",
        matches!(&res, Ok(Some(d)) if d.is_empty()),
    );

    if all {
        println!("WITNESS_OK:ALL");
    } else {
        eprintln!("WITNESS_FAIL: at least one case failed");
        std::process::exit(1);
    }
}
