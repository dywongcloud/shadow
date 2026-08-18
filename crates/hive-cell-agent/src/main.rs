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

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "hive-cell-agent runs inside a Linux microVM (needs AF_VSOCK). \
         Build it for the guest with: cargo build --release -p hive-cell-agent (on Linux/aarch64)."
    );
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod linux {
    use hive_core::{
        now_ms, AgentEvent, AgentRequest, BuildJob, BuildResult, ExecRequest, FunctionLaunch,
        LogLine, LogStream, CELL_AGENT_PORT, CELL_FUNCTION_PORT, CELL_GUEST_CID,
    };
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::os::fd::FromRawFd;
    use std::os::unix::net::UnixStream;
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
        // /build so the function server runs against the deployment's artifacts.
        // Best-effort: build cells have no second drive and this simply no-ops.
        if std::path::Path::new("/dev/vdb").exists() {
            mount("/dev/vdb", "/build", "ext4");
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

    fn mount(src: &str, target: &str, fstype: &str) {
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
            );
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

    /// Returns Ok(true) if a build was executed (caller should stop serving).
    /// Takes the stream BY VALUE (not `&mut`): the `Exec` branch moves it into a
    /// dedicated thread that outlives this call, so the accept loop can serve
    /// the next connection without waiting for the command to finish.
    fn handle_conn(mut stream: UnixStream) -> std::io::Result<bool> {
        let frame = read_frame(&mut stream)?;
        let req: AgentRequest = serde_json::from_slice(&frame)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        match req {
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
            AgentRequest::StartFunction(launch) => {
                match start_function(&launch) {
                    Ok(()) => send(&mut stream, &AgentEvent::FunctionReady)?,
                    Err(e) => send(&mut stream, &AgentEvent::FunctionError(e.to_string()))?,
                }
                let _ = stream.flush();
                // Keep serving control conns; the function bridge runs in threads.
                Ok(false)
            }
            // Only valid as a reply during a build (handled in cache_restore).
            AgentRequest::CacheData { .. } => Ok(false),
            AgentRequest::Exec(req) => {
                // Hand the connection to a thread so the accept loop can keep
                // serving other connections immediately — unlike `Run`, a
                // sandbox exec must not block new exec/kill requests, and a
                // sandbox cell never self-destructs after one command.
                std::thread::spawn(move || {
                    let mut stream = stream;
                    run_exec(&mut stream, req);
                });
                Ok(false)
            }
            AgentRequest::KillExec { id } => {
                // SIGKILL delivered here; the ORIGINAL exec's own connection/
                // thread observes the process die and sends the authoritative
                // `ExecDone{exit_code: None}` — this ack just confirms the
                // signal was (or wasn't, if already gone) delivered.
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

    /// The PATH this agent hands a FUNCTION process. Named once so the
    /// pre-flight existence check below and the spawn itself can never drift
    /// apart — a check against a different PATH than the exec uses is worse
    /// than no check. (The build-process spawns elsewhere in this module set
    /// the same value inline; they are deliberately left alone here, since
    /// nothing pre-flights against them.)
    const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

    /// Is `prog` executable inside this guest via `path`? A program containing a
    /// separator is a path, not a PATH lookup (`execvp` semantics).
    fn guest_bin_exists(prog: &str, path: &str) -> bool {
        if prog.contains('/') {
            return std::path::Path::new(prog).is_file();
        }
        path.split(':')
            .filter(|d| !d.is_empty())
            .any(|d| std::path::Path::new(d).join(prog).is_file())
    }

    /// Launch the function server and bridge `CELL_FUNCTION_PORT` (vsock) to it.
    fn start_function(launch: &FunctionLaunch) -> std::io::Result<()> {
        if launch.start_cmd.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty start_cmd",
            ));
        }
        let workdir = launch
            .workdir
            .clone()
            .unwrap_or_else(|| "/build".to_string());
        let _ = std::fs::create_dir_all(&workdir);

        // The interpreter must exist INSIDE THE GUEST. This is the single most
        // important thing about running a non-Node runtime on Firecracker and
        // the exact bug the first cut of Wasmer support shipped: wasmer was
        // installed on the HOST, but this agent is PID1 inside the microVM and
        // execs against the GUEST rootfs, so the host copy is invisible here.
        // Checked before spawning so the failure names the node fault and its
        // remedy (bake it into the rootfs image — see scripts/build-rootfs.sh)
        // instead of surfacing as a bare ENOENT that the gateway cannot
        // distinguish from the deployment's own entrypoint being wrong.
        let prog = &launch.start_cmd[0];
        if !guest_bin_exists(prog, GUEST_PATH) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "{}: `{prog}` is not installed in this cell's guest image — \
                     the deployment's runtime must be baked into the rootfs \
                     (operator remedy; not an application fault)",
                    hive_core::fault::NODE_RUNTIME_MISSING,
                ),
            ));
        }

        let mut cmd = Command::new(&launch.start_cmd[0]);
        cmd.args(&launch.start_cmd[1..])
            .current_dir(&workdir)
            .env("PATH", GUEST_PATH)
            .env("HOME", "/root")
            .env("PORT", launch.port.to_string())
            .envs(launch.env.iter())
            .stdin(Stdio::null());
        // Wire the function's stdout/stderr to the VM serial console. This
        // agent runs as PID1 with NO open fds (the kernel gives init none), so
        // an inherited-stdio child wrote its output into the void — every app
        // crash/console.error was unobservable from the host, which turned
        // real production failures (e.g. an uncaught throw in a route handler)
        // into blind 500s. /dev/console lands in the host-side per-cell
        // console.log next to the kernel boot lines. Best-effort: a rootfs
        // without /dev/console just keeps the old (silent) behavior.
        if let Ok(con) = std::fs::OpenOptions::new()
            .append(true)
            .open("/dev/console")
        {
            if let Ok(con2) = con.try_clone() {
                cmd.stdout(Stdio::from(con));
                cmd.stderr(Stdio::from(con2));
            }
        }
        // V8 compile-cache (Node cold-start): point Node at the artifact-seeded,
        // WRITABLE cache dir under the workdir. The build shipped precompiled bytecode
        // there; Node >=22.1 picks it up automatically (skips parse/compile on a cold
        // start) and appends entries for modules the build didn't pre-cache. Genuinely
        // Node/V8-only (`launch.runtime` is the single explicit signal, set by the
        // scheduler at cold-start — Bun uses JavaScriptCore and never reads this env
        // var, so gating on it instead of re-sniffing argv fixes a latent bug where a
        // Bun process used to get NODE_COMPILE_CACHE set for no effect). Bun's own
        // bytecode cache (a `.jsc` sidecar produced by `bun build --bytecode` at build
        // time) needs NO runtime env var at all — `bun run <entry>` auto-loads it, so
        // the Bun path here correctly does nothing. Opt-out via HIVE_COMPILE_CACHE=0.
        // Never fails boot: a missing/invalid/unwritable cache just means recompiling.
        let cc_off = launch
            .env
            .get("HIVE_COMPILE_CACHE")
            .map(|v| v == "0" || v == "false")
            .unwrap_or(false);
        if !cc_off && launch.runtime.uses_v8_compile_cache() {
            let cache_dir = format!("{}/.hive-compile-cache", workdir.trim_end_matches('/'));
            if std::fs::create_dir_all(&cache_dir).is_ok() {
                cmd.env("NODE_COMPILE_CACHE", &cache_dir);
            }
        }
        // Detach: the function lives until the cell is torn down. Dropping the
        // std Child does not kill it. STDERR is piped into a bounded capture
        // (last 4 KiB) so a process that dies before binding its port can be
        // DIAGNOSED: the bare "did not bind its port" error told tenants to
        // "check the deployment's logs" while the guest threw those logs away
        // — the one artifact that says WHY (missing env, unreachable database,
        // a stack trace) never left the microVM.
        cmd.stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn()?;
        let stderr_tail: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<u8>>> =
            std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        if let Some(err_pipe) = child.stderr.take() {
            let tail = stderr_tail.clone();
            std::thread::spawn(move || {
                use std::io::Read;
                let mut r = std::io::BufReader::new(err_pipe);
                let mut buf = [0u8; 1024];
                loop {
                    match r.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                            t.extend(&buf[..n]);
                            while t.len() > 4096 {
                                t.pop_front();
                            }
                        }
                    }
                }
            });
        }
        let _child = child;

        // Wait for the function to bind its port.
        //
        // RUNTIME-AWARE, because 30s is not a neutral number for every runtime.
        // A Wasmer guest compiles its whole module ahead-of-time (Cranelift)
        // before it can listen, and on THIS backend that is always a cache MISS:
        // every provision copies a fresh per-cell overlay from the base image
        // and terminate discards it, so wasmer's on-disk artifact cache never
        // survives to a second start. The ~40ms cold start measured for this
        // runtime elsewhere was a cache HIT and says nothing about the first
        // compile of a real module. The mock backend already gives wasm the
        // longer budget for exactly this reason; the guest had no such branch,
        // so a slow compile timed out, the agent reported FunctionError, and the
        // gateway published DEPLOYMENT_START_FAILED — telling the tenant to
        // debug an app that was merely still compiling, while the pool's
        // crash-loop circuit opened against it.
        let fport = launch.port;
        let ready_secs = match launch.runtime {
            hive_core::Runtime::Wasmer => 60,
            _ => 30,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(ready_secs);
        loop {
            if TcpStream::connect(("127.0.0.1", fport)).is_ok() {
                break;
            }
            if std::time::Instant::now() > deadline {
                // Attach the process's own last words — the single most
                // diagnostic artifact for a bind failure, previously discarded.
                let tail: Vec<u8> = stderr_tail
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .copied()
                    .collect();
                let tail = String::from_utf8_lossy(&tail);
                let tail = tail.trim();
                let msg = if tail.is_empty() {
                    "function did not bind its port (process wrote nothing to stderr)".to_string()
                } else {
                    format!("function did not bind its port. Its stderr (last 4KiB): {tail}")
                };
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, msg));
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // Front the function with the SAME multiplexed tunnel protocol the
        // gateway speaks to instances (the mock backend fronts its functions with
        // `TunnelServer` too) — served over an async vsock listener on
        // CELL_FUNCTION_PORT. A dedicated current-thread tokio runtime owns this;
        // the agent's control channel above stays synchronous. Each accepted
        // tunnel connection multiplexes many requests onto 127.0.0.1:<fport>.
        let max_conc = launch.max_concurrency.max(1);
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("tunnel runtime build failed: {e}");
                    return;
                }
            };
            rt.block_on(async move {
                let addr = tokio_vsock::VsockAddr::new(libc::VMADDR_CID_ANY, CELL_FUNCTION_PORT);
                let mut listener = match tokio_vsock::VsockListener::bind(addr) {
                    Ok(l) => l,
                    Err(e) => {
                        eprintln!("vsock listen on {CELL_FUNCTION_PORT} failed: {e}");
                        return;
                    }
                };
                let local = format!("127.0.0.1:{fport}");
                loop {
                    match listener.accept().await {
                        Ok((stream, _)) => {
                            let local = local.clone();
                            tokio::spawn(async move {
                                fluid_tunnel::TunnelServer::serve(stream, local, max_conc).await;
                            });
                        }
                        Err(_) => break,
                    }
                }
            });
        });
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
