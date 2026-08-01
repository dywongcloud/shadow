//! Minimal RESP (REdis Serialization Protocol) client — just enough to back the
//! Upstash-style REST surface of the DB gateway: encode one command as a RESP
//! array-of-bulk-strings, read one reply (any RESP2 type, recursively).
//!
//! Hand-rolled on purpose: the REST translator needs exactly encode-command +
//! decode-reply over an existing TcpStream (loopback to the podman container);
//! a full redis crate would be a far larger maintained surface for the same job.

use anyhow::{anyhow, bail, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// Max CUMULATIVE bulk-string bytes buffered across one pipeline/command's
/// replies. The per-bulk cap below (64MiB) bounds a single value; without this,
/// N commands each returning a large value multiply unboundedly (e.g. 1000x GET
/// on a stored ~4MiB value ~= 4GiB) — a small request forcing a huge buffered
/// response. This is checked in addition to, not instead of, the per-bulk cap.
const REPLY_BYTE_BUDGET: i64 = 64 * 1024 * 1024;

/// One decoded RESP reply, mapped to the JSON shapes the REST surface returns.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// +OK simple strings and $-bulk strings (both surface as JSON strings).
    Text(String),
    /// :int
    Int(i64),
    /// $-1 / *-1 null
    Null,
    /// -ERR ... (surfaced as an HTTP-level error, not a value)
    Error(String),
    /// *N array (may nest)
    Array(Vec<Reply>),
}

impl Reply {
    /// The Upstash REST convention: `{"result": <value>}` where OK/simple strings
    /// are strings, ints are numbers, nil is null, arrays are arrays.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Reply::Text(s) => serde_json::Value::String(s.clone()),
            Reply::Int(i) => serde_json::json!(i),
            Reply::Null => serde_json::Value::Null,
            Reply::Error(e) => serde_json::json!({ "error": e }),
            Reply::Array(items) => {
                serde_json::Value::Array(items.iter().map(|r| r.to_json()).collect())
            }
        }
    }
}

/// Encode one command (["SET","k","v"]) as a RESP array of bulk strings.
pub fn encode_command(parts: &[String]) -> Vec<u8> {
    let mut out = Vec::with_capacity(parts.iter().map(|p| p.len() + 16).sum());
    out.extend_from_slice(format!("*{}\r\n", parts.len()).as_bytes());
    for p in parts {
        out.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
        out.extend_from_slice(p.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// Read exactly one RESP reply (recursive for arrays). Depth-capped so a
/// malicious/broken server can't stack-overflow us. `budget` is the CUMULATIVE
/// bulk-string bytes remaining across the whole pipeline/command call (shared
/// across recursive/sibling calls via the `&mut` — see [`REPLY_BYTE_BUDGET`]).
pub async fn read_reply(
    r: &mut BufReader<TcpStream>,
    depth: u8,
    budget: &mut i64,
) -> Result<Reply> {
    if depth > 16 {
        bail!("RESP reply nested too deep");
    }
    let line = read_line(r).await?;
    let (tag, rest) = line.split_at(1);
    match tag {
        "+" => Ok(Reply::Text(rest.to_string())),
        "-" => Ok(Reply::Error(rest.to_string())),
        ":" => Ok(Reply::Int(
            rest.parse::<i64>()
                .map_err(|_| anyhow!("bad RESP integer"))?,
        )),
        "$" => {
            let n: i64 = rest.parse().map_err(|_| anyhow!("bad RESP bulk length"))?;
            if n < 0 {
                return Ok(Reply::Null);
            }
            // Bulk payloads are length-prefixed; cap so a lying server can't OOM us.
            if n > 64 * 1024 * 1024 {
                bail!("RESP bulk string too large ({n} bytes)");
            }
            if n > *budget {
                bail!(
                    "cumulative reply size exceeds the {}MiB budget for this call",
                    REPLY_BYTE_BUDGET / (1024 * 1024)
                );
            }
            *budget -= n;
            let mut buf = vec![0u8; n as usize + 2]; // + trailing \r\n
            r.read_exact(&mut buf).await?;
            buf.truncate(n as usize);
            Ok(Reply::Text(String::from_utf8_lossy(&buf).into_owned()))
        }
        "*" => {
            let n: i64 = rest.parse().map_err(|_| anyhow!("bad RESP array length"))?;
            if n < 0 {
                return Ok(Reply::Null);
            }
            if n > 1_000_000 {
                bail!("RESP array too large ({n} items)");
            }
            let mut items = Vec::with_capacity(n as usize);
            for _ in 0..n {
                items.push(Box::pin(read_reply(r, depth + 1, budget)).await?);
            }
            Ok(Reply::Array(items))
        }
        _ => bail!("unknown RESP tag {tag:?}"),
    }
}

async fn read_line(r: &mut BufReader<TcpStream>) -> Result<String> {
    // RESP lines are tiny (tags + lengths); cap defends against a garbage stream.
    let mut line = Vec::with_capacity(32);
    loop {
        let b = r.read_u8().await?;
        if b == b'\r' {
            let nl = r.read_u8().await?;
            if nl != b'\n' {
                bail!("malformed RESP line ending");
            }
            break;
        }
        line.push(b);
        if line.len() > 4096 {
            bail!("RESP line too long");
        }
    }
    if line.is_empty() {
        bail!("empty RESP line");
    }
    String::from_utf8(line).map_err(|_| anyhow!("non-UTF8 RESP line"))
}

/// Connect to the loopback redis engine, AUTH, run `parts`, return the reply.
/// One connection per REST call — correct-first; pooling is a later increment.
pub async fn run_command(port: u16, password: &str, parts: &[String]) -> Result<Reply> {
    let replies = run_pipeline(port, password, std::slice::from_ref(&parts.to_vec())).await?;
    replies
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no reply"))
}

/// Run several commands on ONE connection (the `/pipeline` REST endpoint), in
/// order, collecting one reply per command. Commands are written back-to-back
/// then replies drained — true pipelining, not N round-trips.
pub async fn run_pipeline(
    port: u16,
    password: &str,
    commands: &[Vec<String>],
) -> Result<Vec<Reply>> {
    let stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let mut r = BufReader::new(stream);
    let mut budget = REPLY_BYTE_BUDGET;
    if !password.is_empty() {
        r.get_mut()
            .write_all(&encode_command(&["AUTH".into(), password.into()]))
            .await?;
        match read_reply(&mut r, 0, &mut budget).await? {
            Reply::Text(_) => {}
            Reply::Error(e) => bail!("redis AUTH failed: {e}"),
            other => bail!("unexpected AUTH reply: {other:?}"),
        }
    }
    let mut batch = Vec::new();
    for c in commands {
        if c.is_empty() {
            bail!("empty redis command");
        }
        batch.extend_from_slice(&encode_command(c));
    }
    r.get_mut().write_all(&batch).await?;
    let mut out = Vec::with_capacity(commands.len());
    for _ in commands {
        out.push(read_reply(&mut r, 0, &mut budget).await?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_commands_as_resp_arrays() {
        let b = encode_command(&["SET".into(), "k".into(), "v1".into()]);
        assert_eq!(b, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$2\r\nv1\r\n");
        // Empty values still encode as zero-length bulk strings (SET k "").
        let b = encode_command(&["SET".into(), "k".into(), String::new()]);
        assert_eq!(b, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$0\r\n\r\n");
    }

    #[test]
    fn reply_json_shapes_match_upstash_convention() {
        assert_eq!(Reply::Text("OK".into()).to_json(), serde_json::json!("OK"));
        assert_eq!(Reply::Int(7).to_json(), serde_json::json!(7));
        assert_eq!(Reply::Null.to_json(), serde_json::Value::Null);
        assert_eq!(
            Reply::Array(vec![Reply::Text("a".into()), Reply::Null]).to_json(),
            serde_json::json!(["a", null])
        );
    }

    /// Real Redis round-trip: AUTH + SET/GET + a true pipeline (one connection,
    /// replies in order) + the cumulative reply-byte budget actually tripping.
    /// Requires a live engine: `podman run -d -p 127.0.0.1:56379:6379
    /// docker.io/library/redis:7-alpine --requirepass testpw`.
    #[tokio::test]
    #[ignore = "requires a live redis on 127.0.0.1:56379 with --requirepass testpw (see doc comment)"]
    async fn redis_wire_round_trip_and_pipeline_ordering() {
        let r = run_command(56379, "testpw", &["SET".into(), "k1".into(), "v1".into()])
            .await
            .unwrap();
        assert_eq!(r, Reply::Text("OK".into()));
        let r = run_command(56379, "testpw", &["GET".into(), "k1".into()])
            .await
            .unwrap();
        assert_eq!(r, Reply::Text("v1".into()));

        // True pipeline: replies must come back in the SAME order as commands.
        let cmds = vec![
            vec!["SET".into(), "a".into(), "1".into()],
            vec!["SET".into(), "b".into(), "2".into()],
            vec!["GET".into(), "a".into()],
            vec!["GET".into(), "b".into()],
        ];
        let replies = run_pipeline(56379, "testpw", &cmds).await.unwrap();
        assert_eq!(
            replies[2],
            Reply::Text("1".into()),
            "reply order must match command order"
        );
        assert_eq!(replies[3], Reply::Text("2".into()));

        // Cumulative byte budget: N moderately-sized values must eventually trip
        // the budget even though each individual value is well under the 64MiB
        // per-bulk cap — proving the fix is a real cumulative check, not a no-op.
        let big = "x".repeat(2 * 1024 * 1024); // 2MiB
        run_command(56379, "testpw", &["SET".into(), "big".into(), big])
            .await
            .unwrap();
        let many_gets: Vec<Vec<String>> =
            (0..40).map(|_| vec!["GET".into(), "big".into()]).collect(); // 40 x 2MiB = 80MiB > 64MiB budget
        let err = run_pipeline(56379, "testpw", &many_gets).await.unwrap_err();
        assert!(
            err.to_string().contains("budget"),
            "expected the cumulative byte budget to trip, got: {err}"
        );
    }
}
