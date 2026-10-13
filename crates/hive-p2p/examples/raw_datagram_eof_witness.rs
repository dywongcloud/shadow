//! Live witness for `read_raw_datagram`'s EOF classification: a clean EOF at a frame boundary is
//! `Ok(None)`, a peer dying mid-frame is `Err(InvalidData "truncated datagram frame")`, plus the
//! happy path, the oversize cap both ways and zero-length datagrams.
//! `cargo run -p hive-p2p --example raw_datagram_eof_witness`.

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

    let mut r: &[u8] = &[];
    let res = read_raw_datagram(&mut r).await;
    all &= check("clean_eof_at_boundary_is_none", matches!(res, Ok(None)));

    let mut frame = Vec::new();
    write_raw_datagram(&mut frame, b"hello-mesh").await.unwrap();
    let mut r: &[u8] = &frame;
    let res = read_raw_datagram(&mut r).await;
    all &= check(
        "valid_frame_roundtrip",
        matches!(&res, Ok(Some(d)) if d == b"hello-mesh"),
    );

    let mut r: &[u8] = &frame;
    let first = read_raw_datagram(&mut r).await;
    let second_eof = read_raw_datagram(&mut r).await;
    all &= check(
        "frame_then_clean_eof",
        matches!(&first, Ok(Some(d)) if d == b"hello-mesh") && matches!(second_eof, Ok(None)),
    );

    let mut r: &[u8] = &[0x00, 0x00];
    let res = read_raw_datagram(&mut r).await;
    all &= check(
        "partial_prefix_is_truncated_error",
        matches!(&res, Err(e)
            if e.kind() == std::io::ErrorKind::InvalidData
            && e.to_string().contains("truncated datagram frame")),
    );

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

    let mut r: &[u8] = &((RAW_MAX_DATAGRAM as u32 + 1).to_be_bytes())[..];
    let res = read_raw_datagram(&mut r).await;
    all &= check(
        "oversize_read_refused",
        matches!(&res, Err(e) if e.to_string().contains("frame too large")),
    );

    let big = vec![0u8; RAW_MAX_DATAGRAM + 1];
    let mut sink = Vec::new();
    let res = write_raw_datagram(&mut sink, &big).await;
    all &= check("oversize_write_refused", res.is_err() && sink.is_empty());

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
