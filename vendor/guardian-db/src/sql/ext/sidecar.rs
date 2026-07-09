//! The PostgreSQL **sidecar runtime**: a minimal wire-protocol *client* that
//! lets GuardianDB delegate extensions it cannot reimplement natively
//! (PostGIS, TimescaleDB, pg_stat_statements — C code, planner hooks,
//! background workers) to a real, operator-managed PostgreSQL process.
//!
//! The client speaks protocol 3.0 over a plaintext `tokio::net::TcpStream`:
//! StartupMessage, `AuthenticationOk`/`AuthenticationCleartextPassword`
//! handling, `ParameterStatus`/`BackendKeyData`, simple `Query`, and decoding
//! of `RowDescription`/`DataRow`/`CommandComplete`/`ErrorResponse`/
//! `EmptyQueryResponse` into [`ExecResult`] (text format — the same format
//! GuardianDB's own pgwire server emits). Backend errors surface as
//! [`SqlError::Sidecar`], preserving the sidecar's SQLSTATE and message
//! verbatim.
//!
//! The sidecar is configured per session with the `guardian.sidecar_dsn` GUC
//! (`SET guardian.sidecar_dsn = 'postgres://user:pass@host:port/db?sslmode=disable'`)
//! or, as a fallback, the `GUARDIAN_PG_SIDECAR_DSN` environment variable. The
//! routing rules live in [`crate::sql::engine::Session`]; this module owns the
//! DSN parsing and the connection itself.

use crate::relational::{SqlType, SqlValue};
use crate::sql::error::{Result, SqlError};
use crate::sql::result::{ExecResult, OutField};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Upper bound on a single backend message, to fail fast on framing
/// corruption instead of attempting a multi-gigabyte allocation.
const MAX_MESSAGE_LEN: i32 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// DSN
// ---------------------------------------------------------------------------

/// A parsed `postgres://` connection URI for the sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarDsn {
    pub user: String,
    pub password: Option<String>,
    pub host: String,
    pub port: u16,
    pub database: String,
}

impl SidecarDsn {
    /// Parse `postgres://user[:pass]@host[:port][/db][?params]`.
    ///
    /// `sslmode` must be absent or `disable` — the client is plaintext-only —
    /// and rejecting anything else is a typed `0A000`. Other query parameters
    /// are accepted and ignored. `%XX` escapes are decoded in the user,
    /// password and database parts.
    pub fn parse(dsn: &str) -> Result<Self> {
        let rest = dsn
            .trim()
            .strip_prefix("postgres://")
            .or_else(|| dsn.trim().strip_prefix("postgresql://"))
            .ok_or_else(|| invalid("URI must start with postgres:// or postgresql://"))?;
        let (rest, query) = match rest.split_once('?') {
            Some((r, q)) => (r, Some(q)),
            None => (rest, None),
        };
        if let Some(query) = query {
            for pair in query.split('&').filter(|p| !p.is_empty()) {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                if key == "sslmode" && value != "disable" {
                    return Err(SqlError::FeatureNotSupported(format!(
                        "guardian.sidecar_dsn: sslmode={value} is not supported — the \
                         sidecar client is plaintext-only; use sslmode=disable"
                    )));
                }
                // Every other parameter is accepted and ignored.
            }
        }
        let (userinfo, hostpart) = match rest.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, rest),
        };
        let (hostport, db_part) = match hostpart.split_once('/') {
            Some((hp, db)) => (hp, db),
            None => (hostpart, ""),
        };
        let (user, password) = match userinfo {
            Some(ui) => match ui.split_once(':') {
                Some((u, p)) => (percent_decode(u)?, Some(percent_decode(p)?)),
                None => (percent_decode(ui)?, None),
            },
            None => ("postgres".to_string(), None),
        };
        if user.is_empty() {
            return Err(invalid("empty user"));
        }
        let (host, port) = split_host_port(hostport)?;
        if host.is_empty() {
            return Err(invalid("missing host"));
        }
        let database = if db_part.is_empty() {
            user.clone() // like libpq: the database defaults to the user name
        } else {
            percent_decode(db_part)?
        };
        Ok(Self {
            user,
            password,
            host,
            port,
            database,
        })
    }
}

fn invalid(msg: impl std::fmt::Display) -> SqlError {
    SqlError::InvalidParameter(format!("guardian.sidecar_dsn: {msg}"))
}

/// Split `host[:port]`, allowing bracketed IPv6 literals (`[::1]:5432`).
fn split_host_port(hostport: &str) -> Result<(String, u16)> {
    if let Some(rest) = hostport.strip_prefix('[') {
        let (host, after) = rest
            .split_once(']')
            .ok_or_else(|| invalid("unterminated [ipv6] host"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => parse_port(p)?,
            None if after.is_empty() => 5432,
            None => return Err(invalid(format!("unexpected text after host: \"{after}\""))),
        };
        return Ok((host.to_string(), port));
    }
    match hostport.rsplit_once(':') {
        Some((host, p)) => Ok((host.to_string(), parse_port(p)?)),
        None => Ok((hostport.to_string(), 5432)),
    }
}

fn parse_port(p: &str) -> Result<u16> {
    p.parse()
        .map_err(|_| invalid(format!("invalid port \"{p}\"")))
}

/// Decode `%XX` escapes (invalid escapes are an error, not passed through).
fn percent_decode(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| invalid(format!("invalid percent-escape in \"{s}\"")))?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| invalid(format!("non-UTF-8 percent-escape in \"{s}\"")))
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// One authenticated sidecar connection, pinned by a [`Session`] and dropped
/// with it (closing the TCP stream ends the backend session).
///
/// [`Session`]: crate::sql::engine::Session
pub struct SidecarConn {
    stream: TcpStream,
    /// The DSN string this connection was opened with (for change detection).
    dsn: String,
    /// Set when the stream failed mid-message; the session drops broken
    /// connections and reconnects on the next forwarded statement.
    broken: bool,
}

impl SidecarConn {
    /// Connect, authenticate (trust or cleartext password) and wait for
    /// `ReadyForQuery`.
    pub async fn connect(dsn_str: &str) -> Result<Self> {
        let dsn = SidecarDsn::parse(dsn_str)?;
        let stream = TcpStream::connect((dsn.host.as_str(), dsn.port))
            .await
            .map_err(|e| {
                SqlError::Storage(format!(
                    "could not connect to PostgreSQL sidecar at {}:{}: {e}",
                    dsn.host, dsn.port
                ))
            })?;
        let mut conn = Self {
            stream,
            dsn: dsn_str.to_string(),
            broken: false,
        };
        conn.startup(&dsn).await?;
        Ok(conn)
    }

    /// The DSN this connection was opened with.
    pub fn dsn(&self) -> &str {
        &self.dsn
    }

    /// Whether the stream failed and the connection must be discarded.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    async fn startup(&mut self, dsn: &SidecarDsn) -> Result<()> {
        // StartupMessage: int32 len, int32 196608 (3.0), "key\0value\0"*, \0.
        let mut params = Vec::new();
        for (key, value) in [
            ("user", dsn.user.as_str()),
            ("database", dsn.database.as_str()),
        ] {
            params.extend_from_slice(key.as_bytes());
            params.push(0);
            params.extend_from_slice(value.as_bytes());
            params.push(0);
        }
        params.push(0);
        let mut msg = Vec::with_capacity(8 + params.len());
        msg.extend_from_slice(&(8 + params.len() as i32).to_be_bytes());
        msg.extend_from_slice(&196608i32.to_be_bytes());
        msg.extend_from_slice(&params);
        self.write_all(&msg).await?;

        loop {
            let (ty, payload) = self.read_message().await?;
            match ty {
                b'R' => {
                    let code = read_i32(&payload, 0)?;
                    match code {
                        0 => {} // AuthenticationOk
                        3 => {
                            // AuthenticationCleartextPassword.
                            let password = dsn.password.as_deref().ok_or_else(|| {
                                SqlError::Storage(
                                    "sidecar requested a password but the DSN has none".to_string(),
                                )
                            })?;
                            let mut body = password.as_bytes().to_vec();
                            body.push(0);
                            self.write_typed(b'p', &body).await?;
                        }
                        other => {
                            return Err(SqlError::FeatureNotSupported(format!(
                                "sidecar requested authentication method {other} — only \
                                 trust and cleartext password are supported by the \
                                 sidecar client"
                            )));
                        }
                    }
                }
                b'S' | b'K' | b'N' => {} // ParameterStatus / BackendKeyData / Notice
                b'Z' => return Ok(()),   // ReadyForQuery
                b'E' => return Err(decode_error(&payload)),
                other => {
                    return Err(SqlError::Storage(format!(
                        "unexpected sidecar message '{}' during startup",
                        other as char
                    )));
                }
            }
        }
    }

    /// Run one simple-protocol `Query` and decode every result until
    /// `ReadyForQuery`. A backend `ErrorResponse` is drained to readiness and
    /// returned as [`SqlError::Sidecar`] (the connection stays usable).
    pub async fn simple_query(&mut self, sql: &str) -> Result<Vec<ExecResult>> {
        let mut body = sql.as_bytes().to_vec();
        body.push(0);
        self.write_typed(b'Q', &body).await?;

        let mut results = Vec::new();
        let mut fields: Option<Vec<OutField>> = None;
        let mut rows: Vec<Vec<SqlValue>> = Vec::new();
        let mut first_error: Option<SqlError> = None;
        loop {
            let (ty, payload) = self.read_message().await?;
            match ty {
                b'T' => {
                    fields = Some(decode_row_description(&payload)?);
                    rows.clear();
                }
                b'D' => rows.push(decode_data_row(&payload)?),
                b'C' => {
                    let tag = read_cstring(&payload, 0)?.0;
                    results.push(match fields.take() {
                        Some(fields) => ExecResult::Rows {
                            fields,
                            rows: std::mem::take(&mut rows),
                        },
                        None => ExecResult::Command { tag },
                    });
                }
                b'I' => results.push(ExecResult::Command {
                    // EmptyQueryResponse: PostgreSQL's tag-less completion.
                    tag: String::new(),
                }),
                b'E' => {
                    let err = decode_error(&payload);
                    first_error.get_or_insert(err);
                    fields = None;
                    rows.clear();
                }
                b'Z' => break,
                b'S' | b'N' | b'A' => {} // ParameterStatus / Notice / Notify
                other => {
                    self.broken = true;
                    return Err(SqlError::Storage(format!(
                        "unexpected sidecar message '{}' in query response",
                        other as char
                    )));
                }
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(results),
        }
    }

    async fn write_typed(&mut self, ty: u8, body: &[u8]) -> Result<()> {
        let mut msg = Vec::with_capacity(5 + body.len());
        msg.push(ty);
        msg.extend_from_slice(&(4 + body.len() as i32).to_be_bytes());
        msg.extend_from_slice(body);
        self.write_all(&msg).await
    }

    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.stream.write_all(bytes).await.map_err(|e| {
            self.broken = true;
            SqlError::Storage(format!("sidecar connection write failed: {e}"))
        })
    }

    async fn read_message(&mut self) -> Result<(u8, Vec<u8>)> {
        let mut header = [0u8; 5];
        if let Err(e) = self.stream.read_exact(&mut header).await {
            self.broken = true;
            return Err(SqlError::Storage(format!(
                "sidecar connection read failed: {e}"
            )));
        }
        let len = i32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        if !(4..=MAX_MESSAGE_LEN).contains(&len) {
            self.broken = true;
            return Err(SqlError::Storage(format!(
                "sidecar sent a malformed message length: {len}"
            )));
        }
        let mut payload = vec![0u8; (len - 4) as usize];
        if let Err(e) = self.stream.read_exact(&mut payload).await {
            self.broken = true;
            return Err(SqlError::Storage(format!(
                "sidecar connection read failed: {e}"
            )));
        }
        Ok((header[0], payload))
    }
}

// ---------------------------------------------------------------------------
// Message decoding
// ---------------------------------------------------------------------------

/// RowDescription: int16 nfields, then per field: name\0, table oid (i32),
/// attnum (i16), type oid (i32), typlen (i16), typmod (i32), format (i16).
fn decode_row_description(payload: &[u8]) -> Result<Vec<OutField>> {
    let nfields = read_i16(payload, 0)?;
    let mut pos = 2usize;
    let mut fields = Vec::with_capacity(nfields.max(0) as usize);
    for _ in 0..nfields {
        let (name, after) = read_cstring(payload, pos)?;
        let type_oid = read_i32(payload, after + 6)?;
        pos = after + 18;
        fields.push(OutField::new(name, type_from_oid(type_oid)));
    }
    Ok(fields)
}

/// DataRow: int16 ncols, then per column: int32 length (-1 = NULL) + bytes.
/// Values arrive in text format and stay textual — GuardianDB's own results
/// are emitted as text on the wire, so this is lossless end to end.
fn decode_data_row(payload: &[u8]) -> Result<Vec<SqlValue>> {
    let ncols = read_i16(payload, 0)?;
    let mut pos = 2usize;
    let mut row = Vec::with_capacity(ncols.max(0) as usize);
    for _ in 0..ncols {
        let len = read_i32(payload, pos)?;
        pos += 4;
        if len < 0 {
            row.push(SqlValue::Null);
            continue;
        }
        let end = pos + len as usize;
        let bytes = payload
            .get(pos..end)
            .ok_or_else(|| SqlError::Storage("sidecar DataRow truncated".to_string()))?;
        row.push(SqlValue::Text(String::from_utf8_lossy(bytes).into_owned()));
        pos = end;
    }
    Ok(row)
}

/// ErrorResponse: (field-type byte + cstring)* + \0 terminator. `C` carries
/// the SQLSTATE, `M` the primary message; both are preserved verbatim.
fn decode_error(payload: &[u8]) -> SqlError {
    let mut code = String::new();
    let mut message = String::new();
    let mut pos = 0usize;
    while let Some(&field) = payload.get(pos) {
        if field == 0 {
            break;
        }
        pos += 1;
        let Ok((value, after)) = read_cstring(payload, pos) else {
            break;
        };
        match field {
            b'C' => code = value,
            b'M' => message = value,
            _ => {}
        }
        pos = after;
    }
    if message.is_empty() {
        message = "sidecar error with no message".to_string();
    }
    let valid_code = code.len() == 5 && code.bytes().all(|b| b.is_ascii_alphanumeric());
    SqlError::Sidecar {
        sqlstate: if valid_code {
            code
        } else {
            "58030".to_string() // io_error: unclassifiable remote failure
        },
        message,
    }
}

/// Map the common wire type OIDs back onto engine types (everything else is
/// reported as text, which is exactly how the value bytes arrive anyway).
fn type_from_oid(oid: i32) -> SqlType {
    match oid {
        16 => SqlType::Boolean,
        17 => SqlType::Bytea,
        20 => SqlType::BigInt,
        21 => SqlType::SmallInt,
        23 => SqlType::Integer,
        114 => SqlType::Json,
        700 => SqlType::Real,
        701 => SqlType::DoublePrecision,
        1042 => SqlType::Char(None),
        1043 => SqlType::Varchar(None),
        1082 => SqlType::Date,
        1083 => SqlType::Time,
        1114 => SqlType::Timestamp,
        1184 => SqlType::Timestamptz,
        1700 => SqlType::Numeric {
            precision: None,
            scale: None,
        },
        2950 => SqlType::Uuid,
        3802 => SqlType::Jsonb,
        _ => SqlType::Text,
    }
}

fn read_i16(payload: &[u8], pos: usize) -> Result<i16> {
    payload
        .get(pos..pos + 2)
        .map(|b| i16::from_be_bytes([b[0], b[1]]))
        .ok_or_else(|| SqlError::Storage("sidecar message truncated".to_string()))
}

fn read_i32(payload: &[u8], pos: usize) -> Result<i32> {
    payload
        .get(pos..pos + 4)
        .map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| SqlError::Storage("sidecar message truncated".to_string()))
}

/// Read a NUL-terminated string at `pos`; returns the string and the offset
/// just past the terminator.
fn read_cstring(payload: &[u8], pos: usize) -> Result<(String, usize)> {
    let rest = payload
        .get(pos..)
        .ok_or_else(|| SqlError::Storage("sidecar message truncated".to_string()))?;
    let end = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| SqlError::Storage("sidecar string not terminated".to_string()))?;
    Ok((
        String::from_utf8_lossy(&rest[..end]).into_owned(),
        pos + end + 1,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_dsn() {
        let dsn =
            SidecarDsn::parse("postgres://alice:s%40cret@db.example:5433/app?sslmode=disable")
                .unwrap();
        assert_eq!(dsn.user, "alice");
        assert_eq!(dsn.password.as_deref(), Some("s@cret"));
        assert_eq!(dsn.host, "db.example");
        assert_eq!(dsn.port, 5433);
        assert_eq!(dsn.database, "app");
    }

    #[test]
    fn parses_minimal_and_default_forms() {
        let dsn = SidecarDsn::parse("postgresql://bob@localhost").unwrap();
        assert_eq!(dsn.user, "bob");
        assert_eq!(dsn.password, None);
        assert_eq!(dsn.port, 5432);
        // The database defaults to the user, like libpq.
        assert_eq!(dsn.database, "bob");

        let dsn = SidecarDsn::parse("postgres://localhost/db").unwrap();
        assert_eq!(dsn.user, "postgres");
        assert_eq!(dsn.database, "db");

        let dsn = SidecarDsn::parse("postgres://u@[::1]:5544/db").unwrap();
        assert_eq!(dsn.host, "::1");
        assert_eq!(dsn.port, 5544);
    }

    #[test]
    fn rejects_non_disable_sslmode() {
        for mode in ["require", "prefer", "verify-full"] {
            let err = SidecarDsn::parse(&format!("postgres://u@h/db?sslmode={mode}")).unwrap_err();
            assert_eq!(err.sqlstate(), "0A000", "{mode}");
            assert!(err.to_string().contains("sslmode"), "{err}");
        }
        // Absent or disable are fine; other parameters are ignored.
        assert!(SidecarDsn::parse("postgres://u@h/db").is_ok());
        assert!(SidecarDsn::parse("postgres://u@h/db?sslmode=disable&connect_timeout=3").is_ok());
    }

    #[test]
    fn rejects_malformed_dsns() {
        for bad in [
            "mysql://u@h/db",
            "postgres://u@h:notaport/db",
            "postgres://u@/db",
            "postgres://u:p%zz@h/db",
            "postgres://u@[::1/db",
        ] {
            let err = SidecarDsn::parse(bad).unwrap_err();
            assert_eq!(err.sqlstate(), "22023", "for `{bad}`: {err}");
        }
    }

    #[test]
    fn error_response_decodes_sqlstate_and_message() {
        // S"ERROR" C"0A000" M"nope" terminator.
        let payload = b"SERROR\0C0A000\0Mnope\0\0";
        let err = decode_error(payload);
        assert_eq!(err.sqlstate(), "0A000");
        assert_eq!(err.to_string(), "nope");
        // A garbage code degrades to io_error instead of panicking.
        let err = decode_error(b"Cbad\0Mx\0\0");
        assert_eq!(err.sqlstate(), "58030");
    }

    #[test]
    fn row_description_and_data_row_decode() {
        // One field named "n", type oid 23 (int4).
        let mut t = Vec::new();
        t.extend_from_slice(&1i16.to_be_bytes());
        t.extend_from_slice(b"n\0");
        t.extend_from_slice(&0i32.to_be_bytes()); // table oid
        t.extend_from_slice(&0i16.to_be_bytes()); // attnum
        t.extend_from_slice(&23i32.to_be_bytes()); // type oid
        t.extend_from_slice(&4i16.to_be_bytes()); // typlen
        t.extend_from_slice(&(-1i32).to_be_bytes()); // typmod
        t.extend_from_slice(&0i16.to_be_bytes()); // format
        let fields = decode_row_description(&t).unwrap();
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].name, "n");
        assert_eq!(fields[0].ty, SqlType::Integer);

        // Two columns: "42" and NULL.
        let mut d = Vec::new();
        d.extend_from_slice(&2i16.to_be_bytes());
        d.extend_from_slice(&2i32.to_be_bytes());
        d.extend_from_slice(b"42");
        d.extend_from_slice(&(-1i32).to_be_bytes());
        let row = decode_data_row(&d).unwrap();
        assert_eq!(row[0].to_text().as_deref(), Some("42"));
        assert!(row[1].is_null());
    }
}
