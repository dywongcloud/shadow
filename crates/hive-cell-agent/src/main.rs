//! `hive-cell-agent` — the **cell daemon**, running inside the microVM (guest).
//!
//! It is the in-guest counterpart of Hive's cell daemon: it listens on vsock,
//! receives a [`AgentRequest::Run`] from the box daemon, executes the build,
//! streams [`AgentEvent::Log`] frames, and finishes with [`AgentEvent::Done`].
//!
//! Designed to run as the guest's `init` (PID 1) via `init=/sbin/hive-cell-agent`
//! in the kernel cmdline, but also works as an ordinary process (e.g. launched
//! from a systemd unit). It is Linux-only at runtime; on other platforms `main`
//! is a stub so the workspace still builds on macOS.

fn protocol_probe_requested() -> bool {
    let Some(arg) = std::env::args_os().nth(1) else {
        return false;
    };
    match arg.to_str() {
        Some("--runtime-artifact-protocol") => {
            println!("{}", hive_core::RUNTIME_ARTIFACT_PROTOCOL_VERSION);
            true
        }
        Some("--agent-wire-protocol") => {
            println!("{}", hive_core::AGENT_WIRE_PROTOCOL_VERSION);
            true
        }
        Some("--agent-wire-capabilities") => {
            println!("{}", hive_core::AGENT_WIRE_CAPABILITIES);
            true
        }
        Some("--agent-protocol-fact") => {
            println!(
                "{{\"rootfs_schema\":{},\"runtime_artifact_protocol\":{},\"agent_wire_protocol\":{},\"agent_wire_capabilities\":{}}}",
                hive_core::RUNTIME_ARTIFACT_ROOTFS_SCHEMA_VERSION,
                hive_core::RUNTIME_ARTIFACT_PROTOCOL_VERSION,
                hive_core::AGENT_WIRE_PROTOCOL_VERSION,
                hive_core::AGENT_WIRE_CAPABILITIES,
            );
            true
        }
        _ => false,
    }
}

#[cfg(target_os = "linux")]
fn main() {
    if protocol_probe_requested() {
        return;
    }
    linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    if protocol_probe_requested() {
        return;
    }
    eprintln!(
        "hive-cell-agent runs inside a Linux microVM (needs AF_VSOCK). \
         Build it for the guest with: cargo build --release -p hive-cell-agent (on Linux/aarch64)."
    );
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod linux {
    use hive_core::{
        agent_handshake_transcript, now_ms, AgentBootProof, AgentEvent, AgentHandshake,
        AgentHandshakeReady, AgentProtocolFault, AgentProtocolFaultCode, AgentRequest, BuildJob,
        BuildResult, ExecRequest, FunctionLaunch, LogLine, LogStream, RuntimeArtifactIdentity,
        RuntimeArtifactRootfsMarker, AGENT_HANDSHAKE_NONCE_BYTES, AGENT_WIRE_CAPABILITIES,
        AGENT_WIRE_PROTOCOL_VERSION, CELL_AGENT_PORT, CELL_FUNCTION_PORT, CELL_GUEST_CID,
        RUNTIME_ARTIFACT_MARKER_FILE, RUNTIME_ARTIFACT_PROTOCOL_VERSION,
        RUNTIME_ARTIFACT_ROOTFS_MARKER_PATH, RUNTIME_ARTIFACT_ROOTFS_SCHEMA_VERSION,
    };
    use sha2::{Digest, Sha256};
    use std::io::{BufReader, Read, Write};
    use std::net::TcpStream;
    use std::os::fd::{AsRawFd, FromRawFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    use std::path::{Component, Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    /// Live exec child PIDs, keyed by `ExecRequest.id`, so a `KillExec` arriving
    /// on a SEPARATE connection can signal a command started on another
    /// connection/thread. Guest-process-global (single agent process per cell).
    static EXEC_REGISTRY: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, u32>>,
    > = std::sync::OnceLock::new();
    fn exec_registry() -> &'static std::sync::Mutex<std::collections::HashMap<String, u32>> {
        EXEC_REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    /// Set exactly once during `minimal_init`, before any connection is served:
    /// `/dev/vdb` (the host's attached data image) existed but the `mount(2)`
    /// syscall itself failed. Distinct from "no `/dev/vdb`" (a normal build
    /// cell, `false`) and "mounted fine" (also `false`) — this flag exists so a
    /// later `start_function` can refuse loudly instead of treating the
    /// `create_dir_all`-created, still-empty `/workspace` as a valid cwd.
    /// `create_dir_all` runs unconditionally before the syscall (it must, to
    /// give `mount(2)` a mountpoint), so directory existence alone was never
    /// proof the mount happened — only the syscall's own return code is.
    static WORKSPACE_MOUNT_FAILED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    pub fn run() {
        let as_init = std::process::id() == 1;
        if as_init {
            minimal_init();
        }
        if let Err(e) = serve() {
            eprintln!("hive-cell-agent fatal: {e}");
        }
        if as_init {
            // As PID1 we must not return; power the VM off so the host reaps it.
            unsafe {
                libc::sync();
                libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
            }
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        }
    }

    /// Bring up the bare minimum when we are PID 1. Best-effort: a failure here
    /// shouldn't stop us from serving (the kernel may already have mounted some).
    fn minimal_init() {
        mount("proc", "/proc", "proc");
        mount("sysfs", "/sys", "sysfs");
        mount("devtmpfs", "/dev", "devtmpfs");
        mount("tmpfs", "/tmp", "tmpfs");
        // Bring up the loopback interface. The function server binds 0.0.0.0:$PORT
        // (works without lo), but we reach it over 127.0.0.1 to bridge it to vsock
        // — and 127.0.0.1 is unreachable until `lo` is UP. Without this every
        // function looks like it "did not bind its port".
        bring_up_loopback();
        // If a second drive (the delivered build output) is attached, mount it at
        // /workspace so a monorepo app retains its checkout-relative cwd. NOT
        // best-effort once the device exists: a build cell with no second drive
        // is a normal, expected no-op, but a cell whose host attached `/dev/vdb`
        // and whose guest mount then failed must never silently continue with an
        // empty `/workspace` — that is exactly the "ran, but on nothing" failure
        // mode `start_function`'s workdir gate exists to catch (see
        // `WORKSPACE_MOUNT_FAILED`).
        if std::path::Path::new("/dev/vdb").exists() && !mount("/dev/vdb", "/workspace", "ext4") {
            WORKSPACE_MOUNT_FAILED.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Set IFF_UP on the loopback interface via SIOCSIFFLAGS (no `ip`/iproute2
    /// dependency in the guest rootfs). Best-effort.
    fn bring_up_loopback() {
        // Mirrors `struct ifreq` for the flags ioctls: 16-byte name + flags.
        #[repr(C)]
        struct IfReqFlags {
            name: [libc::c_char; 16],
            flags: libc::c_short,
            _pad: [u8; 22],
        }
        // ioctl request arg type differs by libc (c_ulong on glibc, c_int on
        // musl); `as _` coerces to whichever this target expects.
        const SIOCGIFFLAGS: u64 = 0x8913;
        const SIOCSIFFLAGS: u64 = 0x8914;
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
            if fd < 0 {
                return;
            }
            let mut req: IfReqFlags = std::mem::zeroed();
            req.name[0] = b'l' as libc::c_char;
            req.name[1] = b'o' as libc::c_char;
            if libc::ioctl(fd, SIOCGIFFLAGS as _, &mut req) == 0 {
                req.flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
                let _ = libc::ioctl(fd, SIOCSIFFLAGS as _, &req);
            }
            libc::close(fd);
        }
    }

    /// Returns whether the `mount(2)` syscall itself reported success. Callers
    /// that treat a mount as best-effort (proc/sys/dev/tmp — the kernel may
    /// already have mounted some of these) are free to ignore the result; a
    /// caller staging tenant data may not, because `create_dir_all` below
    /// always leaves `target` existing as an empty directory regardless of
    /// whether the mount happened, so directory existence can never stand in
    /// for a checked syscall result.
    fn mount(src: &str, target: &str, fstype: &str) -> bool {
        let _ = std::fs::create_dir_all(target);
        let c_src = cstr(src);
        let c_tgt = cstr(target);
        let c_fs = cstr(fstype);
        unsafe {
            libc::mount(
                c_src.as_ptr(),
                c_tgt.as_ptr(),
                c_fs.as_ptr(),
                0,
                std::ptr::null(),
            ) == 0
        }
    }

    fn cstr(s: &str) -> std::ffi::CString {
        std::ffi::CString::new(s).unwrap()
    }

    /// Listen on AF_VSOCK and serve requests until a build completes.
    fn serve() -> std::io::Result<()> {
        let listen_fd = vsock_listen(CELL_AGENT_PORT)?;
        eprintln!(
            "cell agent listening on vsock cid={} port={}",
            CELL_GUEST_CID, CELL_AGENT_PORT
        );
        loop {
            let conn_fd =
                unsafe { libc::accept(listen_fd, std::ptr::null_mut(), std::ptr::null_mut()) };
            if conn_fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: conn_fd is a valid connected SOCK_STREAM fd; UnixStream is
            // just a typed wrapper around read()/write() on it.
            let stream = unsafe { UnixStream::from_raw_fd(conn_fd) };
            match handle_conn(stream) {
                Ok(true) => return Ok(()), // a build ran; cell is single-use
                Ok(false) => continue,     // ping/keepalive/exec/function; keep listening
                Err(e) => eprintln!("connection error: {e}"),
            }
        }
    }

    const MAX_ACCEPTED_HANDSHAKE_NONCES: usize = 4096;
    static ACCEPTED_HANDSHAKE_NONCES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashSet<String>>,
    > = std::sync::OnceLock::new();

    fn accepted_handshake_nonces() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
        ACCEPTED_HANDSHAKE_NONCES
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
    }

    fn protocol_fault(
        code: AgentProtocolFaultCode,
        message: impl Into<String>,
    ) -> AgentProtocolFault {
        AgentProtocolFault::new(code, message)
    }

    fn refuse_protocol(
        stream: &mut UnixStream,
        code: AgentProtocolFaultCode,
        message: impl Into<String>,
    ) -> std::io::Result<bool> {
        send(
            stream,
            &AgentEvent::ProtocolFault(protocol_fault(code, message)),
        )?;
        let _ = stream.flush();
        Ok(false)
    }

    fn parse_agent_request(frame: &[u8]) -> Result<AgentRequest, AgentProtocolFault> {
        serde_json::from_slice(frame).map_err(|error| {
            protocol_fault(
                AgentProtocolFaultCode::Malformed,
                format!("agent request is not valid protocol JSON: {error}"),
            )
        })
    }

    /// The one pre-handshake launch shape frozen for a guest-first rollout. It
    /// is the exact field set serialized by the actual pre-upgrade host, with
    /// runtime_artifact absent. A future host cannot smuggle a new field through
    /// this path, and the path never emits RuntimeArtifactReady.
    fn frozen_legacy_launch_frame(frame: &[u8], launch: &FunctionLaunch) -> bool {
        if launch.runtime_artifact.is_some() {
            return false;
        }
        let Ok(serde_json::Value::Object(outer)) = serde_json::from_slice(frame) else {
            return false;
        };
        if outer.len() != 1 {
            return false;
        }
        let Some(serde_json::Value::Object(fields)) = outer.get("StartFunction") else {
            return false;
        };
        const LEGACY_FIELDS: [&str; 13] = [
            "start_cmd",
            "env",
            "workdir",
            "port",
            "max_concurrency",
            "memory_mib",
            "cpus",
            "pids",
            "runtime",
            "raw_proxy",
            "udp_ports",
            "tcp_ports",
            "gpu",
        ];
        fields.len() == LEGACY_FIELDS.len()
            && !fields.contains_key("runtime_artifact")
            && LEGACY_FIELDS
                .iter()
                .all(|field| fields.contains_key(*field))
    }

    fn valid_handshake_nonce(nonce: &str) -> bool {
        nonce.len() == AGENT_HANDSHAKE_NONCE_BYTES * 2 && lowercase_sha256(nonce)
    }

    fn handshake_transcript_sha256(nonce: &str, proof: &AgentBootProof) -> String {
        let digest = Sha256::digest(agent_handshake_transcript(nonce, proof));
        let mut value = String::with_capacity(64);
        use std::fmt::Write as _;
        for byte in digest {
            let _ = write!(value, "{byte:02x}");
        }
        value
    }

    fn reserve_handshake_nonce(nonce: &str) -> Result<(), AgentProtocolFault> {
        let mut accepted = accepted_handshake_nonces()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if accepted.contains(nonce) {
            return Err(protocol_fault(
                AgentProtocolFaultCode::Replay,
                "agent handshake nonce was already accepted",
            ));
        }
        if accepted.len() >= MAX_ACCEPTED_HANDSHAKE_NONCES {
            return Err(protocol_fault(
                AgentProtocolFaultCode::Replay,
                "agent handshake replay window is full; refusing new launches",
            ));
        }
        accepted.insert(nonce.to_string());
        Ok(())
    }

    fn validate_handshake(
        handshake: &AgentHandshake,
    ) -> Result<AgentHandshakeReady, AgentProtocolFault> {
        let expected = &handshake.expected_boot;
        if expected.agent_wire_protocol != AGENT_WIRE_PROTOCOL_VERSION {
            return Err(protocol_fault(
                AgentProtocolFaultCode::UnsupportedWireProtocol,
                format!(
                    "host requires agent wire protocol {}, guest implements {}",
                    expected.agent_wire_protocol, AGENT_WIRE_PROTOCOL_VERSION
                ),
            ));
        }
        if expected.agent_wire_capabilities != AGENT_WIRE_CAPABILITIES {
            return Err(protocol_fault(
                AgentProtocolFaultCode::CapabilityMismatch,
                format!(
                    "host requires agent capability set {:#x}, guest implements {:#x}",
                    expected.agent_wire_capabilities, AGENT_WIRE_CAPABILITIES
                ),
            ));
        }
        if expected.runtime_artifact_protocol != RUNTIME_ARTIFACT_PROTOCOL_VERSION {
            return Err(protocol_fault(
                AgentProtocolFaultCode::RuntimeArtifactProtocolMismatch,
                format!(
                    "host requires runtime-artifact protocol {}, guest implements {}",
                    expected.runtime_artifact_protocol, RUNTIME_ARTIFACT_PROTOCOL_VERSION
                ),
            ));
        }
        if !valid_handshake_nonce(&handshake.nonce) {
            return Err(protocol_fault(
                AgentProtocolFaultCode::InvalidNonce,
                "agent handshake nonce must be 32 random bytes encoded as lowercase hex",
            ));
        }
        if expected.rootfs_schema != RUNTIME_ARTIFACT_ROOTFS_SCHEMA_VERSION
            || !lowercase_sha256(&expected.agent_sha256)
            || !lowercase_sha256(&expected.rootfs_image_sha256)
            || expected.rootfs_image_bytes == 0
        {
            return Err(protocol_fault(
                AgentProtocolFaultCode::RootfsProofMismatch,
                "host presented an invalid rootfs boot proof",
            ));
        }

        let observed = validate_rootfs_agent_protocol().map_err(|error| {
            protocol_fault(
                AgentProtocolFaultCode::RootfsProofMismatch,
                format!("guest could not prove its running rootfs agent: {error}"),
            )
        })?;
        let proof = AgentBootProof {
            rootfs_schema: observed.schema,
            runtime_artifact_protocol: observed.protocol,
            agent_wire_protocol: observed.agent_wire_protocol,
            agent_wire_capabilities: observed.agent_wire_capabilities,
            agent_sha256: observed.agent_sha256,
            // The immutable image digest is host-observed. The guest binds it to
            // its independently-observed marker and executable in the transcript.
            rootfs_image_sha256: expected.rootfs_image_sha256.clone(),
            rootfs_image_bytes: expected.rootfs_image_bytes,
        };
        if &proof != expected {
            return Err(protocol_fault(
                AgentProtocolFaultCode::RootfsProofMismatch,
                "running guest proof differs from the host-verified rootfs proof",
            ));
        }
        reserve_handshake_nonce(&handshake.nonce)?;
        Ok(AgentHandshakeReady {
            nonce: handshake.nonce.clone(),
            transcript_sha256: handshake_transcript_sha256(&handshake.nonce, &proof),
            proof,
        })
    }

    fn handle_versioned_launch(
        stream: &mut UnixStream,
        launch: FunctionLaunch,
    ) -> std::io::Result<bool> {
        if !matches!(
            (&launch.runtime_artifact, &launch.workdir),
            (Some(_), Some(_))
        ) {
            return refuse_protocol(
                stream,
                AgentProtocolFaultCode::OutOfOrder,
                "a handshaken launch must carry both runtime-artifact identity and guest workdir",
            );
        }
        match validate_runtime_artifact(&launch) {
            Ok(identity) => {
                send(stream, &AgentEvent::RuntimeArtifactReady(identity))?;
                match start_function(&launch, false) {
                    Ok(()) => send(stream, &AgentEvent::FunctionReady)?,
                    Err(error) => send(stream, &AgentEvent::FunctionError(error.to_string()))?,
                }
            }
            Err(error) => send(stream, &AgentEvent::FunctionError(error.to_string()))?,
        }
        let _ = stream.flush();
        Ok(false)
    }

    fn handle_legacy_launch(
        stream: &mut UnixStream,
        launch: FunctionLaunch,
    ) -> std::io::Result<bool> {
        // This path deliberately performs neither half of runtime-artifact H1/H2:
        // no host identity is trusted, no artifact marker is asserted, and no
        // RuntimeArtifactReady authorization event is emitted.
        match start_function(&launch, true) {
            Ok(()) => send(stream, &AgentEvent::FunctionReady)?,
            Err(error) => send(stream, &AgentEvent::FunctionError(error.to_string()))?,
        }
        let _ = stream.flush();
        Ok(false)
    }

    /// Returns Ok(true) if a build was executed (caller should stop serving).
    /// Takes the stream BY VALUE (not &mut): the Exec branch moves it into a
    /// dedicated thread that outlives this call, so the accept loop can serve
    /// the next connection without waiting for the command to finish.
    fn handle_conn(mut stream: UnixStream) -> std::io::Result<bool> {
        let first_frame = read_frame(&mut stream)?;
        let first = match parse_agent_request(&first_frame) {
            Ok(request) => request,
            Err(fault) => {
                send(&mut stream, &AgentEvent::ProtocolFault(fault))?;
                let _ = stream.flush();
                return Ok(false);
            }
        };

        match first {
            AgentRequest::Handshake(handshake) => {
                let ready = match validate_handshake(&handshake) {
                    Ok(ready) => ready,
                    Err(fault) => {
                        send(&mut stream, &AgentEvent::ProtocolFault(fault))?;
                        let _ = stream.flush();
                        return Ok(false);
                    }
                };
                send(&mut stream, &AgentEvent::HandshakeReady(ready))?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let next_frame = match read_frame(&mut stream) {
                    Ok(frame) => frame,
                    Err(error) => {
                        return refuse_protocol(
                            &mut stream,
                            AgentProtocolFaultCode::OutOfOrder,
                            format!(
                                "handshake was not followed by one launch frame within 5 seconds: {error}"
                            ),
                        );
                    }
                };
                stream.set_read_timeout(None)?;
                let next = match parse_agent_request(&next_frame) {
                    Ok(request) => request,
                    Err(fault) => {
                        send(&mut stream, &AgentEvent::ProtocolFault(fault))?;
                        let _ = stream.flush();
                        return Ok(false);
                    }
                };
                match next {
                    AgentRequest::StartFunction(launch) => {
                        handle_versioned_launch(&mut stream, launch)
                    }
                    AgentRequest::Handshake(_) => refuse_protocol(
                        &mut stream,
                        AgentProtocolFaultCode::DuplicateHandshake,
                        "duplicate handshake on one connection",
                    ),
                    _ => refuse_protocol(
                        &mut stream,
                        AgentProtocolFaultCode::OutOfOrder,
                        "handshake must be followed immediately by StartFunction",
                    ),
                }
            }
            AgentRequest::StartFunction(launch) => {
                if frozen_legacy_launch_frame(&first_frame, &launch) {
                    handle_legacy_launch(&mut stream, launch)
                } else {
                    refuse_protocol(
                        &mut stream,
                        AgentProtocolFaultCode::HandshakeRequired,
                        "StartFunction requires an authenticated agent handshake",
                    )
                }
            }
            AgentRequest::Ping => {
                send(&mut stream, &AgentEvent::Pong)?;
                Ok(false)
            }
            AgentRequest::Run(job) => {
                let result = run_build(&mut stream, &job)?;
                send(&mut stream, &AgentEvent::Done(result))?;
                let _ = stream.flush();
                Ok(true)
            }
            // Only valid as a reply during a build (handled in cache_restore).
            AgentRequest::CacheData { .. } => Ok(false),
            AgentRequest::Exec(req) => {
                std::thread::spawn(move || {
                    let mut stream = stream;
                    run_exec(&mut stream, req);
                });
                Ok(false)
            }
            AgentRequest::KillExec { id } => {
                kill_registered_exec(&id);
                send(&mut stream, &AgentEvent::Pong)?;
                let _ = stream.flush();
                Ok(false)
            }
        }
    }

    /// Run one `ExecRequest` to completion on its OWN dedicated connection,
    /// streaming `ExecOutput` lines with stdout/stderr kept distinct, then a
    /// final `ExecDone`. Registers/deregisters the child PID so `KillExec`
    /// (arriving on a different connection) can find and signal it.
    fn run_exec(stream: &mut UnixStream, req: ExecRequest) {
        let id = req.id.clone();
        let exit_code = match spawn_exec(stream, &req) {
            Ok(code) => code,
            Err(e) => {
                let _ = send(
                    stream,
                    &AgentEvent::ExecOutput {
                        id: id.clone(),
                        stream: LogStream::System,
                        line: format!("exec failed: {e}"),
                    },
                );
                None
            }
        };
        exec_registry().lock().unwrap().remove(&id);
        let _ = send(stream, &AgentEvent::ExecDone { id, exit_code });
        let _ = stream.flush();
    }

    fn spawn_exec(stream: &mut UnixStream, req: &ExecRequest) -> std::io::Result<Option<i32>> {
        let mut cmd = if req.shell {
            // Explicit, informed opt-in only (validated one layer up too).
            let mut full = req.cmd.clone();
            for a in &req.args {
                full.push(' ');
                full.push_str(a);
            }
            let mut c = Command::new("/bin/sh");
            c.arg("-c").arg(full);
            c
        } else {
            // Default: real argv exec, no shell — shell injection is
            // structurally impossible on this path (no string is ever
            // reparsed by a shell).
            let mut c = Command::new(&req.cmd);
            c.args(&req.args);
            c
        };

        if req.sudo {
            // Only ever honored if `sudo` actually exists in this guest image;
            // otherwise fail loudly rather than silently running unprivileged.
            let sudo_path = ["/usr/bin/sudo", "/bin/sudo"]
                .iter()
                .find(|p| std::path::Path::new(p).exists());
            match sudo_path {
                Some(sudo) => {
                    let mut wrapped = Command::new(sudo);
                    wrapped.arg("-n"); // never prompt for a password
                    if req.shell {
                        wrapped.arg("/bin/sh").arg("-c");
                        let mut full = req.cmd.clone();
                        for a in &req.args {
                            full.push(' ');
                            full.push_str(a);
                        }
                        wrapped.arg(full);
                    } else {
                        wrapped.arg(&req.cmd).args(&req.args);
                    }
                    cmd = wrapped;
                }
                None => {
                    let _ = send(
                        stream,
                        &AgentEvent::ExecOutput {
                            id: req.id.clone(),
                            stream: LogStream::System,
                            line: "sudo requested but not available in this sandbox image".into(),
                        },
                    );
                    return Ok(None);
                }
            }
        }

        let cwd = if req.cwd.is_empty() {
            "/build"
        } else {
            req.cwd.as_str()
        };
        let _ = std::fs::create_dir_all(cwd);
        cmd.current_dir(cwd)
            .env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("HOME", "/root")
            .envs(req.env.iter())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        exec_registry()
            .lock()
            .unwrap()
            .insert(req.id.clone(), child.id());

        // Two reader threads (stdout/stderr) so neither pipe's backpressure can
        // stall the other — both feed the SAME connection, guarded by a mutex
        // (UnixStream doesn't support concurrent writers safely otherwise).
        let write_lock = std::sync::Arc::new(std::sync::Mutex::new(()));
        let mut handles = Vec::new();
        if let Some(out) = child.stdout.take() {
            handles.push(spawn_reader(
                out,
                stream.try_clone()?,
                req.id.clone(),
                LogStream::Stdout,
                write_lock.clone(),
            ));
        }
        if let Some(err) = child.stderr.take() {
            handles.push(spawn_reader(
                err,
                stream.try_clone()?,
                req.id.clone(),
                LogStream::Stderr,
                write_lock.clone(),
            ));
        }
        for h in handles {
            let _ = h.join();
        }
        let status = child.wait()?;
        Ok(status.code())
    }

    fn spawn_reader(
        pipe: impl Read + Send + 'static,
        mut stream: UnixStream,
        id: String,
        which: LogStream,
        write_lock: std::sync::Arc<std::sync::Mutex<()>>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            // BOUNDED capture — see `hive_core::logcap`. `lines()` retains an
            // unbounded String when the child writes without newlines, and the
            // resulting frame is then serialized and pushed to the HOST, so an
            // unbounded line here is an unbounded allocation on both sides of
            // the vsock. Cap it at the source.
            let mut reader = BufReader::new(pipe);
            loop {
                let l = match hive_core::logcap::read_capped_line_blocking(
                    &mut reader,
                    hive_core::MAX_LOG_LINE_BYTES,
                ) {
                    Ok(Some(l)) => l,
                    _ => break,
                };
                let _guard = write_lock.lock().unwrap();
                if send(
                    &mut stream,
                    &AgentEvent::ExecOutput {
                        id: id.clone(),
                        stream: which,
                        line: l.text,
                    },
                )
                .is_err()
                {
                    break;
                }
            }
        })
    }

    /// Deliver SIGKILL to a live exec's child process by request id. Returns
    /// true if a live registration was found (does not guarantee the process
    /// had not already exited — that race is harmless: `kill` on a reaped pid
    /// just errors, ignored).
    fn kill_registered_exec(id: &str) -> bool {
        let pid = exec_registry().lock().unwrap().get(id).copied();
        match pid {
            Some(pid) => {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
                true
            }
            None => false,
        }
    }

    fn lowercase_sha256(value: &str) -> bool {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    fn hash_reader_sha256(mut reader: impl Read) -> std::io::Result<String> {
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 1024 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let digest = hasher.finalize();
        let mut value = String::with_capacity(64);
        use std::fmt::Write as _;
        for byte in digest {
            let _ = write!(value, "{byte:02x}");
        }
        Ok(value)
    }

    fn validate_rootfs_agent_protocol() -> std::io::Result<RuntimeArtifactRootfsMarker> {
        let marker_path = Path::new(RUNTIME_ARTIFACT_ROOTFS_MARKER_PATH);
        let mut marker = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(marker_path)
            .map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!(
                        "guest rootfs protocol marker {} is unavailable: {error} ({})",
                        marker_path.display(),
                        hive_core::fault::NODE_RUNTIME_MISSING
                    ),
                )
            })?;
        let metadata = marker.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != 0
            || metadata.nlink() != 1
            || metadata.mode() & 0o222 != 0
            || metadata.len() == 0
            || metadata.len() > 4096
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "guest rootfs protocol marker must be a root-owned, read-only, single-link regular file ({})",
                    hive_core::fault::NODE_RUNTIME_MISSING
                ),
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        std::io::Read::by_ref(&mut marker)
            .take(metadata.len() + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != metadata.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "guest rootfs protocol marker changed during exact-length read ({})",
                    hive_core::fault::NODE_RUNTIME_MISSING
                ),
            ));
        }
        let observed: RuntimeArtifactRootfsMarker =
            serde_json::from_slice(&bytes).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "guest rootfs protocol marker is invalid JSON: {error} ({})",
                        hive_core::fault::NODE_RUNTIME_MISSING
                    ),
                )
            })?;
        if observed.schema != RUNTIME_ARTIFACT_ROOTFS_SCHEMA_VERSION
            || observed.protocol != RUNTIME_ARTIFACT_PROTOCOL_VERSION
            || observed.agent_wire_protocol != AGENT_WIRE_PROTOCOL_VERSION
            || observed.agent_wire_capabilities != AGENT_WIRE_CAPABILITIES
            || !lowercase_sha256(&observed.agent_sha256)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "guest rootfs marker has an unsupported schema, runtime-artifact protocol, agent-wire protocol/capability set, or agent digest ({})",
                    hive_core::fault::NODE_RUNTIME_MISSING
                ),
            ));
        }
        let agent_sha256 = hash_reader_sha256(std::fs::File::open("/proc/self/exe")?)?;
        if agent_sha256 != observed.agent_sha256 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "running cell agent does not match the rootfs protocol marker ({})",
                    hive_core::fault::NODE_RUNTIME_MISSING
                ),
            ));
        }
        Ok(observed)
    }

    fn validate_runtime_artifact(
        launch: &FunctionLaunch,
    ) -> std::io::Result<RuntimeArtifactIdentity> {
        let expected = launch.runtime_artifact.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "function launch is missing runtime artifact protocol v{} identity ({})",
                    RUNTIME_ARTIFACT_PROTOCOL_VERSION,
                    hive_core::fault::NODE_RUNTIME_MISSING
                ),
            )
        })?;
        if expected.protocol != RUNTIME_ARTIFACT_PROTOCOL_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "function launch requires unsupported runtime artifact protocol {} ({})",
                    expected.protocol,
                    hive_core::fault::NODE_RUNTIME_MISSING
                ),
            ));
        }
        let _ = validate_rootfs_agent_protocol()?;
        if WORKSPACE_MOUNT_FAILED.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                format!(
                    "/dev/vdb was attached but failed to mount at /workspace; refusing to run \
                     tenant code against an empty cwd ({})",
                    hive_core::fault::NODE_IMAGE_MISSING
                ),
            ));
        }

        let marker_path = Path::new("/workspace").join(RUNTIME_ARTIFACT_MARKER_FILE);
        let mut marker = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&marker_path)
            .map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!(
                        "runtime artifact marker {} is unavailable: {error} ({})",
                        marker_path.display(),
                        hive_core::fault::NODE_IMAGE_MISSING
                    ),
                )
            })?;
        let metadata = marker.metadata()?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.len() == 0
            || metadata.len() > 4096
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "runtime artifact marker {} has invalid type, link count, or size ({})",
                    marker_path.display(),
                    hive_core::fault::NODE_IMAGE_MISSING
                ),
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        std::io::Read::by_ref(&mut marker)
            .take(metadata.len() + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != metadata.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "runtime artifact marker changed during exact-length read ({})",
                    hive_core::fault::NODE_IMAGE_MISSING
                ),
            ));
        }
        let observed: RuntimeArtifactIdentity =
            serde_json::from_slice(&bytes).map_err(|error| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "runtime artifact marker is invalid JSON: {error} ({})",
                        hive_core::fault::NODE_IMAGE_MISSING
                    ),
                )
            })?;
        if &observed != expected {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "runtime artifact marker does not match the host-provided identity ({})",
                    hive_core::fault::NODE_IMAGE_MISSING
                ),
            ));
        }
        Ok(observed)
    }

    /// Default PATH for function processes. A launch may provide a custom PATH,
    /// but every entry is validated below before it can participate in lookup.
    const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

    type BridgeHandle = std::thread::JoinHandle<()>;
    static FUNCTION_BRIDGE: std::sync::OnceLock<std::sync::Mutex<Option<BridgeHandle>>> =
        std::sync::OnceLock::new();

    fn function_bridge() -> &'static std::sync::Mutex<Option<BridgeHandle>> {
        FUNCTION_BRIDGE.get_or_init(|| std::sync::Mutex::new(None))
    }

    fn image_fault(kind: std::io::ErrorKind, message: impl std::fmt::Display) -> std::io::Error {
        std::io::Error::new(
            kind,
            format!("{message} ({})", hive_core::fault::NODE_IMAGE_MISSING),
        )
    }

    struct ValidatedWorkdir {
        path: PathBuf,
        dir: std::fs::File,
    }

    fn validate_function_workdir(
        launch: &FunctionLaunch,
        legacy_unverified: bool,
    ) -> std::io::Result<ValidatedWorkdir> {
        let workdir = match (&launch.runtime_artifact, launch.workdir.as_deref()) {
            (Some(_), Some(workdir)) if !legacy_unverified => workdir,
            (None, Some(workdir)) if legacy_unverified => workdir,
            (None, None) => {
                if WORKSPACE_MOUNT_FAILED.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(image_fault(
                        std::io::ErrorKind::NotConnected,
                        "/dev/vdb was attached but failed to mount at /workspace; refusing to run \
                         tenant code against an empty cwd",
                    ));
                }
                "/workspace"
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "runtime artifact identity and workdir must be supplied together ({})",
                        hive_core::fault::NODE_RUNTIME_MISSING
                    ),
                ));
            }
        };

        let workdir_path = Path::new(workdir);
        let mut normalized = PathBuf::from("/");
        let mut components = workdir_path.components();
        if components.next() != Some(Component::RootDir) {
            return Err(image_fault(
                std::io::ErrorKind::InvalidInput,
                "runtime workdir must be an absolute /workspace path",
            ));
        }
        let Some(Component::Normal(first)) = components.next() else {
            return Err(image_fault(
                std::io::ErrorKind::InvalidInput,
                "runtime workdir must name /workspace",
            ));
        };
        if first != "workspace" {
            return Err(image_fault(
                std::io::ErrorKind::PermissionDenied,
                "runtime workdir must stay beneath /workspace",
            ));
        }
        normalized.push(first);
        for component in components {
            let Component::Normal(name) = component else {
                return Err(image_fault(
                    std::io::ErrorKind::InvalidInput,
                    "runtime workdir must be lexically normalized",
                ));
            };
            normalized.push(name);
        }
        if normalized.as_os_str().as_bytes() != workdir_path.as_os_str().as_bytes() {
            return Err(image_fault(
                std::io::ErrorKind::InvalidInput,
                "runtime workdir must be lexically normalized",
            ));
        }

        let mut cursor = PathBuf::from("/");
        for component in normalized.components() {
            if let Component::Normal(name) = component {
                cursor.push(name);
                let metadata = std::fs::symlink_metadata(&cursor).map_err(|error| {
                    image_fault(
                        error.kind(),
                        format!(
                            "runtime artifact path {} is unavailable: {error}",
                            cursor.display()
                        ),
                    )
                })?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(image_fault(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "runtime artifact path {} must be a real directory",
                            cursor.display()
                        ),
                    ));
                }
            }
        }

        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&normalized)
            .map_err(|error| {
                image_fault(
                    error.kind(),
                    format!(
                        "validated runtime workdir {} could not be opened: {error}",
                        normalized.display()
                    ),
                )
            })?;
        if !dir.metadata()?.is_dir() {
            return Err(image_fault(
                std::io::ErrorKind::InvalidData,
                format!(
                    "validated runtime workdir {} is not a directory",
                    normalized.display()
                ),
            ));
        }
        Ok(ValidatedWorkdir {
            path: normalized,
            dir,
        })
    }

    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }

    const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
    const RESOLVE_BENEATH: u64 = 0x08;

    fn open_at(dir: RawFd, path: &Path, beneath: bool) -> std::io::Result<std::fs::File> {
        if path.as_os_str().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "empty executable path",
            ));
        }
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "executable path contains a NUL byte",
            )
        })?;
        let how = OpenHow {
            flags: (libc::O_RDONLY | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve: RESOLVE_NO_MAGICLINKS | if beneath { RESOLVE_BENEATH } else { 0 },
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                dir,
                path.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL)) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    format!(
                        "guest kernel cannot perform bounded executable resolution: {error} ({})",
                        hive_core::fault::NODE_RUNTIME_MISSING
                    ),
                ));
            }
            return Err(error);
        }
        Ok(unsafe { std::fs::File::from_raw_fd(fd as RawFd) })
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    struct ExecutableStamp {
        dev: u64,
        ino: u64,
        len: u64,
        mode: u32,
        mtime: i64,
        mtime_nsec: i64,
    }

    impl ExecutableStamp {
        fn read(metadata: &std::fs::Metadata) -> Self {
            Self {
                dev: metadata.dev(),
                ino: metadata.ino(),
                len: metadata.len(),
                mode: metadata.mode(),
                mtime: metadata.mtime(),
                mtime_nsec: metadata.mtime_nsec(),
            }
        }
    }

    struct ResolvedExecutable {
        path: PathBuf,
        stamp: ExecutableStamp,
        _file: std::fs::File,
        node_fault: bool,
    }

    impl ResolvedExecutable {
        fn from_file(
            file: std::fs::File,
            workdir: Option<&Path>,
            node_fault: bool,
        ) -> std::io::Result<Self> {
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "resolved executable is not a regular file",
                ));
            }
            if metadata.mode() & 0o111 == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "resolved executable has no execute permission",
                ));
            }
            let fd_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
            let path = std::fs::read_link(&fd_path)?;
            if let Some(workdir) = workdir {
                if !path.starts_with(workdir) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "resolved executable {} escapes runtime workdir {}",
                            path.display(),
                            workdir.display()
                        ),
                    ));
                }
            }
            Ok(Self {
                path,
                stamp: ExecutableStamp::read(&metadata),
                _file: file,
                node_fault,
            })
        }

        fn revalidate(&self) -> std::io::Result<()> {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&self.path)?;
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.mode() & 0o111 == 0
                || ExecutableStamp::read(&metadata) != self.stamp
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "validated executable {} changed before spawn",
                        self.path.display()
                    ),
                ));
            }
            Ok(())
        }

        fn error(&self, error: std::io::Error) -> std::io::Error {
            executable_error(error.kind(), error, self.node_fault)
        }
    }

    fn executable_error(
        kind: std::io::ErrorKind,
        message: impl std::fmt::Display,
        node_fault: bool,
    ) -> std::io::Error {
        if node_fault {
            std::io::Error::new(
                kind,
                format!("{message} ({})", hive_core::fault::NODE_RUNTIME_MISSING),
            )
        } else {
            std::io::Error::new(kind, message.to_string())
        }
    }

    fn platform_runtime_program(launch: &FunctionLaunch, program: &str) -> bool {
        let basename = Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(program);
        match launch.runtime {
            hive_core::Runtime::Node => {
                matches!(basename, "node" | "npm" | "npx" | "pnpm" | "yarn")
            }
            hive_core::Runtime::Bun => matches!(basename, "bun" | "bunx"),
            hive_core::Runtime::Python => matches!(basename, "python" | "python3"),
            hive_core::Runtime::Wasmer => basename == "wasmer",
            hive_core::Runtime::Container | hive_core::Runtime::Command => false,
        }
    }

    fn normalize_relative(path: &Path, allow_empty: bool) -> std::io::Result<PathBuf> {
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(name) => normalized.push(name),
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "executable path {} contains traversal or an absolute prefix",
                            path.display()
                        ),
                    ));
                }
            }
        }
        if !allow_empty && normalized.as_os_str().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "executable path resolves to the runtime workdir itself",
            ));
        }
        Ok(normalized)
    }

    fn system_path_entry(path: &Path) -> bool {
        GUEST_PATH.split(':').any(|entry| Path::new(entry) == path)
    }

    enum SearchDir {
        Artifact(PathBuf),
        System(PathBuf),
    }

    fn parse_search_path(
        path: &str,
        workdir: &ValidatedWorkdir,
    ) -> std::io::Result<Vec<SearchDir>> {
        let mut entries = Vec::new();
        for entry in path.split(':') {
            if entry.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "function PATH contains an empty entry; use '.' explicitly for the workdir",
                ));
            }
            let entry = Path::new(entry);
            if entry.is_absolute() {
                if system_path_entry(entry) {
                    entries.push(SearchDir::System(entry.to_path_buf()));
                    continue;
                }
                let relative = entry.strip_prefix(&workdir.path).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "function PATH entry {} is outside the runtime workdir and platform PATH",
                            entry.display()
                        ),
                    )
                })?;
                entries.push(SearchDir::Artifact(normalize_relative(relative, true)?));
            } else {
                entries.push(SearchDir::Artifact(normalize_relative(entry, true)?));
            }
        }
        Ok(entries)
    }

    fn open_artifact_executable(
        workdir: &ValidatedWorkdir,
        relative: &Path,
        node_fault: bool,
    ) -> std::io::Result<ResolvedExecutable> {
        let file = open_at(workdir.dir.as_raw_fd(), relative, true)?;
        ResolvedExecutable::from_file(file, Some(&workdir.path), node_fault)
    }

    fn open_system_executable(
        path: &Path,
        node_fault: bool,
    ) -> std::io::Result<ResolvedExecutable> {
        let file = open_at(libc::AT_FDCWD, path, false)?;
        ResolvedExecutable::from_file(file, None, node_fault)
    }

    fn resolve_function_executable(
        launch: &FunctionLaunch,
        workdir: &ValidatedWorkdir,
        effective_path: &str,
    ) -> std::io::Result<ResolvedExecutable> {
        let program = launch.start_cmd.first().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty start_cmd")
        })?;
        if program.as_bytes().contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "function executable contains a NUL byte",
            ));
        }
        let platform_program = platform_runtime_program(launch, program);
        if program.contains('/') {
            let path = Path::new(program);
            if path.is_absolute() {
                if let Ok(relative) = path.strip_prefix(&workdir.path) {
                    return open_artifact_executable(
                        workdir,
                        &normalize_relative(relative, false)?,
                        false,
                    );
                }
                if path
                    .components()
                    .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
                    || !GUEST_PATH
                        .split(':')
                        .any(|entry| path.starts_with(Path::new(entry)))
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "absolute function executable {} is outside the runtime workdir and platform PATH",
                            path.display()
                        ),
                    ));
                }
                return open_system_executable(path, platform_program)
                    .map_err(|error| executable_error(error.kind(), error, platform_program));
            }
            return open_artifact_executable(workdir, &normalize_relative(path, false)?, false);
        }

        let mut searched_platform_path = false;
        for entry in parse_search_path(effective_path, workdir)? {
            let (result, node_fault) = match entry {
                SearchDir::Artifact(directory) => (
                    open_artifact_executable(workdir, &directory.join(program), false),
                    false,
                ),
                SearchDir::System(directory) => {
                    searched_platform_path = true;
                    (
                        open_system_executable(&directory.join(program), platform_program),
                        platform_program,
                    )
                }
            };
            match result {
                Ok(executable) => return Ok(executable),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => {
                    return Err(executable_error(error.kind(), error, node_fault));
                }
            }
        }
        let node_fault = platform_program && searched_platform_path;
        Err(executable_error(
            std::io::ErrorKind::NotFound,
            format!(
                "function executable `{program}` was not found in effective PATH `{effective_path}`"
            ),
            node_fault,
        ))
    }

    #[derive(Clone, Copy)]
    enum BridgeMode {
        Http,
        Raw,
    }

    async fn serve_function_connection<S>(
        stream: S,
        local: String,
        max_concurrency: u32,
        mode: BridgeMode,
    ) where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        match mode {
            BridgeMode::Http => {
                fluid_tunnel::TunnelServer::serve(stream, local, max_concurrency).await;
            }
            BridgeMode::Raw => {
                fluid_tunnel::TunnelServer::serve_raw(stream, local).await;
            }
        }
    }

    fn run_function_bridge(
        startup: &std::sync::mpsc::SyncSender<Result<(), String>>,
        ready: &std::sync::atomic::AtomicBool,
        function_port: u16,
        max_concurrency: u32,
        mode: BridgeMode,
    ) -> std::io::Result<()> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("function bridge Tokio runtime creation failed: {error}"),
                )
            })?;
        let address = tokio_vsock::VsockAddr::new(libc::VMADDR_CID_ANY, CELL_FUNCTION_PORT);
        let local = format!("127.0.0.1:{function_port}");
        runtime.block_on(async move {
            let mut listener = tokio_vsock::VsockListener::bind(address).map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!(
                        "function bridge bind on vsock port {CELL_FUNCTION_PORT} failed: {error}"
                    ),
                )
            })?;
            ready.store(true, std::sync::atomic::Ordering::SeqCst);
            if startup.send(Ok(())).is_err() {
                ready.store(false, std::sync::atomic::Ordering::SeqCst);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "function bridge startup receiver disappeared",
                ));
            }
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let local = local.clone();
                        tokio::spawn(serve_function_connection(
                            stream,
                            local,
                            max_concurrency,
                            mode,
                        ));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
        })
    }

    fn bridge_fatal(message: &str) -> ! {
        eprintln!("function bridge terminated after readiness: {message}; powering off cell");
        unsafe {
            libc::sync();
            if std::process::id() == 1 {
                libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
            }
            libc::_exit(1);
        }
    }

    fn start_function_bridge(
        function_port: u16,
        max_concurrency: u32,
        raw_proxy: bool,
    ) -> std::io::Result<()> {
        let mode = if raw_proxy {
            BridgeMode::Raw
        } else {
            BridgeMode::Http
        };
        let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel(0);
        let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_ready = ready.clone();
        let mut slot = function_bridge()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slot.is_some() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "function bridge already exists in this cell ({})",
                    hive_core::fault::NODE_RUNTIME_MISSING
                ),
            ));
        }
        let handle = std::thread::Builder::new()
            .name("hive-function-bridge".to_string())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_function_bridge(
                        &startup_tx,
                        &thread_ready,
                        function_port,
                        max_concurrency,
                        mode,
                    )
                }));
                let message = match outcome {
                    Ok(Ok(())) => "bridge serve task returned unexpectedly".to_string(),
                    Ok(Err(error)) => error.to_string(),
                    Err(_) => "bridge serve task panicked".to_string(),
                };
                if thread_ready.load(std::sync::atomic::Ordering::SeqCst) {
                    bridge_fatal(&message);
                }
                let _ = startup_tx.send(Err(message));
            })
            .map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!(
                        "function bridge thread creation failed: {error} ({})",
                        hive_core::fault::NODE_RUNTIME_MISSING
                    ),
                )
            })?;
        *slot = Some(handle);
        drop(slot);

        match startup_rx.recv() {
            Ok(Ok(())) => {
                let finished = function_bridge()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                    .map(|handle| handle.is_finished())
                    .unwrap_or(true);
                if finished {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        format!(
                            "function bridge ended during startup ({})",
                            hive_core::fault::NODE_RUNTIME_MISSING
                        ),
                    ));
                }
                Ok(())
            }
            Ok(Err(message)) => {
                let handle = function_bridge()
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(handle) = handle {
                    let _ = handle.join();
                }
                Err(std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    format!(
                        "function bridge failed before readiness: {message} ({})",
                        hive_core::fault::NODE_RUNTIME_MISSING
                    ),
                ))
            }
            Err(error) => {
                let handle = function_bridge()
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .take();
                if let Some(handle) = handle {
                    let _ = handle.join();
                }
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    format!(
                        "function bridge startup failed without a result: {error} ({})",
                        hive_core::fault::NODE_RUNTIME_MISSING
                    ),
                ))
            }
        }
    }

    fn stderr_tail_text(
        tail: &std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<u8>>>,
    ) -> String {
        let bytes: Vec<u8> = tail
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .copied()
            .collect();
        String::from_utf8_lossy(&bytes).trim().to_string()
    }

    /// Launch the function server and bridge `CELL_FUNCTION_PORT` (vsock) to it.
    fn start_function(launch: &FunctionLaunch, legacy_unverified: bool) -> std::io::Result<()> {
        if launch.start_cmd.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty start_cmd",
            ));
        }
        let workdir = validate_function_workdir(launch, legacy_unverified)?;
        let effective_path = launch
            .env
            .get("PATH")
            .map(String::as_str)
            .unwrap_or(GUEST_PATH);
        let executable = resolve_function_executable(launch, &workdir, effective_path)?;

        let mut cmd = Command::new(&executable.path);
        cmd.arg0(&launch.start_cmd[0])
            .args(&launch.start_cmd[1..])
            .current_dir(format!("/proc/self/fd/{}", workdir.dir.as_raw_fd()))
            .env_clear()
            .envs(launch.env.iter())
            // Platform-owned constraints are applied last, after launch env.
            .env("PATH", effective_path)
            .env("HOME", "/root")
            .env("PORT", launch.port.to_string())
            .stdin(Stdio::null());
        if let Ok(con) = std::fs::OpenOptions::new()
            .append(true)
            .open("/dev/console")
        {
            if let Ok(con2) = con.try_clone() {
                cmd.stdout(Stdio::from(con));
                cmd.stderr(Stdio::from(con2));
            }
        }
        let cc_off = launch
            .env
            .get("HIVE_COMPILE_CACHE")
            .map(|value| value == "0" || value == "false")
            .unwrap_or(false);
        if !cc_off && launch.runtime.uses_v8_compile_cache() {
            let cache_dir = workdir.path.join(".hive-compile-cache");
            if std::fs::create_dir_all(&cache_dir).is_ok() {
                cmd.env("NODE_COMPILE_CACHE", cache_dir);
            }
        }
        cmd.stderr(std::process::Stdio::piped());
        executable
            .revalidate()
            .map_err(|error| executable.error(error))?;
        let mut child = cmd.spawn().map_err(|error| {
            executable.error(std::io::Error::new(
                error.kind(),
                format!(
                    "spawn of validated executable {} failed: {error}",
                    executable.path.display()
                ),
            ))
        })?;
        let stderr_tail: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<u8>>> =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        if let Some(err_pipe) = child.stderr.take() {
            let tail = stderr_tail.clone();
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(err_pipe);
                let mut buffer = [0u8; 1024];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let mut bytes = tail.lock().unwrap_or_else(|error| error.into_inner());
                            bytes.extend(&buffer[..read]);
                            while bytes.len() > 4096 {
                                bytes.pop_front();
                            }
                        }
                    }
                }
            });
        }

        let ready_secs = match launch.runtime {
            hive_core::Runtime::Wasmer => 60,
            _ => 30,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(ready_secs);
        loop {
            if TcpStream::connect(("127.0.0.1", launch.port)).is_ok() {
                break;
            }
            if let Some(status) = child.try_wait()? {
                let tail = stderr_tail_text(&stderr_tail);
                let detail = if tail.is_empty() {
                    "process wrote nothing to stderr".to_string()
                } else {
                    format!("stderr (last 4KiB): {tail}")
                };
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    format!("function exited before binding its port ({status}); {detail}"),
                ));
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                let tail = stderr_tail_text(&stderr_tail);
                let message = if tail.is_empty() {
                    "function did not bind its port (process wrote nothing to stderr)".to_string()
                } else {
                    format!("function did not bind its port. Its stderr (last 4KiB): {tail}")
                };
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, message));
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        if let Err(error) =
            start_function_bridge(launch.port, launch.max_concurrency.max(1), launch.raw_proxy)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        drop(child);
        Ok(())
    }

    fn run_build(stream: &mut UnixStream, job: &BuildJob) -> std::io::Result<BuildResult> {
        let started_at_ms = now_ms();
        sys(stream, format!("cell agent running build {}", job.id))?;

        let mut steps: Vec<String> = Vec::new();
        if !job.repo.is_empty() {
            let branch = if job.git_ref.is_empty() || job.git_ref == "HEAD" {
                String::new()
            } else {
                format!("--branch {}", job.git_ref)
            };
            steps.push(format!("git clone --depth 1 {} {} .", branch, job.repo));
        }
        steps.extend(job.commands.iter().cloned());

        let _ = std::fs::create_dir_all("/build");

        // Build cache: restore before the build.
        if let Some(key) = &job.cache_key {
            if !job.cache_paths.is_empty() {
                cache_restore(stream, key, &job.cache_paths)?;
            }
        }

        let mut exit_code = 0i32;
        for step in &steps {
            sys(stream, format!("$ {step}"))?;
            exit_code = run_step(stream, job, step)?;
            if exit_code != 0 {
                break;
            }
        }

        // Build cache: save after a successful build.
        if exit_code == 0 {
            if let Some(key) = &job.cache_key {
                if !job.cache_paths.is_empty() {
                    cache_save(stream, key, &job.cache_paths)?;
                }
            }
        }

        Ok(BuildResult {
            job_id: job.id.clone(),
            exit_code,
            timed_out: false,
            started_at_ms,
            finished_at_ms: now_ms(),
        })
    }

    /// Ask the box daemon for the cache tarball and unpack it into /build.
    fn cache_restore(stream: &mut UnixStream, key: &str, paths: &[String]) -> std::io::Result<()> {
        send(
            stream,
            &AgentEvent::CacheGet {
                key: key.to_string(),
                paths: paths.to_vec(),
            },
        )?;
        let frame = read_frame(stream)?;
        if let Ok(AgentRequest::CacheData { tar }) = serde_json::from_slice::<AgentRequest>(&frame)
        {
            if !tar.is_empty() {
                std::fs::write("/tmp/cache_in.tgz", &tar)?;
                let _ = Command::new("tar")
                    .args(["xzf", "/tmp/cache_in.tgz", "-C", "/build"])
                    .status();
                sys(
                    stream,
                    format!("build cache restored [{key}] ({} bytes)", tar.len()),
                )?;
            } else {
                sys(stream, format!("build cache miss [{key}]"))?;
            }
        }
        Ok(())
    }

    /// Tar the (existing) cache paths and send them to the box daemon to persist.
    fn cache_save(stream: &mut UnixStream, key: &str, paths: &[String]) -> std::io::Result<()> {
        let existing: Vec<String> = paths
            .iter()
            .filter(|p| std::path::Path::new(&format!("/build/{p}")).exists())
            .cloned()
            .collect();
        if existing.is_empty() {
            return Ok(());
        }
        let mut args = vec![
            "czf".to_string(),
            "/tmp/cache_out.tgz".to_string(),
            "-C".to_string(),
            "/build".to_string(),
        ];
        args.extend(existing);
        let ok = Command::new("tar")
            .args(&args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            if let Ok(tar) = std::fs::read("/tmp/cache_out.tgz") {
                let n = tar.len();
                send(
                    stream,
                    &AgentEvent::CachePut {
                        key: key.to_string(),
                        tar,
                    },
                )?;
                sys(stream, format!("build cache saved [{key}] ({n} bytes)"))?;
            }
        }
        Ok(())
    }

    /// Run one shell step, merging stderr into stdout and streaming each line.
    fn run_step(stream: &mut UnixStream, job: &BuildJob, step: &str) -> std::io::Result<i32> {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(format!("{step} 2>&1"))
            .current_dir("/build")
            .env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("HOME", "/root")
            .envs(job.env.iter())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Ensure the work dir exists.
        let _ = std::fs::create_dir_all("/build");

        let mut child = cmd.spawn()?;
        if let Some(out) = child.stdout.take() {
            // BOUNDED capture — see `hive_core::logcap`.
            let mut reader = BufReader::new(out);
            while let Ok(Some(l)) = hive_core::logcap::read_capped_line_blocking(
                &mut reader,
                hive_core::MAX_LOG_LINE_BYTES,
            ) {
                send(
                    stream,
                    &AgentEvent::Log(LogLine {
                        ts_ms: now_ms(),
                        stream: LogStream::Stdout,
                        line: l.text,
                    }),
                )?;
            }
        }
        let status = child.wait()?;
        Ok(status.code().unwrap_or(-1))
    }

    fn sys(stream: &mut UnixStream, line: String) -> std::io::Result<()> {
        send(
            stream,
            &AgentEvent::Log(LogLine {
                ts_ms: now_ms(),
                stream: LogStream::System,
                line,
            }),
        )
    }

    fn send(stream: &mut UnixStream, ev: &AgentEvent) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(ev)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        write_frame(stream, &bytes)
    }

    // ---- vsock + framing (sync) ------------------------------------------

    fn vsock_listen(port: u32) -> std::io::Result<i32> {
        unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut addr: libc::sockaddr_vm = std::mem::zeroed();
            addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
            addr.svm_cid = libc::VMADDR_CID_ANY;
            addr.svm_port = port;
            let ret = libc::bind(
                fd,
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            );
            if ret < 0 {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }
            if libc::listen(fd, 8) < 0 {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }
            Ok(fd)
        }
    }

    fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> std::io::Result<()> {
        w.write_all(&(payload.len() as u32).to_be_bytes())?;
        w.write_all(payload)?;
        w.flush()
    }

    fn read_frame<R: Read>(r: &mut R) -> std::io::Result<Vec<u8>> {
        let mut len = [0u8; 4];
        r.read_exact(&mut len)?;
        let n = u32::from_be_bytes(len) as usize;
        if n > 64 * 1024 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        let mut buf = vec![0u8; n];
        r.read_exact(&mut buf)?;
        Ok(buf)
    }
}
