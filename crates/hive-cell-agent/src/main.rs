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
        now_ms, AgentEvent, AgentRequest, BuildJob, BuildResult, FunctionLaunch, LogLine,
        LogStream, CELL_AGENT_PORT, CELL_FUNCTION_PORT, CELL_GUEST_CID,
    };
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::os::fd::FromRawFd;
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};
    use std::time::Duration;

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
            let conn_fd = unsafe { libc::accept(listen_fd, std::ptr::null_mut(), std::ptr::null_mut()) };
            if conn_fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: conn_fd is a valid connected SOCK_STREAM fd; UnixStream is
            // just a typed wrapper around read()/write() on it.
            let mut stream = unsafe { UnixStream::from_raw_fd(conn_fd) };
            match handle_conn(&mut stream) {
                Ok(true) => return Ok(()), // a build ran; cell is single-use
                Ok(false) => continue,     // ping/keepalive; keep listening
                Err(e) => eprintln!("connection error: {e}"),
            }
        }
    }

    /// Returns Ok(true) if a build was executed (caller should stop serving).
    fn handle_conn(stream: &mut UnixStream) -> std::io::Result<bool> {
        let frame = read_frame(stream)?;
        let req: AgentRequest = serde_json::from_slice(&frame)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        match req {
            AgentRequest::Ping => {
                send(stream, &AgentEvent::Pong)?;
                Ok(false)
            }
            AgentRequest::Run(job) => {
                let result = run_build(stream, &job)?;
                send(stream, &AgentEvent::Done(result))?;
                let _ = stream.flush();
                Ok(true)
            }
            AgentRequest::StartFunction(launch) => {
                match start_function(&launch) {
                    Ok(()) => send(stream, &AgentEvent::FunctionReady)?,
                    Err(e) => send(stream, &AgentEvent::FunctionError(e.to_string()))?,
                }
                let _ = stream.flush();
                // Keep serving control conns; the function bridge runs in threads.
                Ok(false)
            }
            // Only valid as a reply during a build (handled in cache_restore).
            AgentRequest::CacheData { .. } => Ok(false),
        }
    }

    /// Launch the function server and bridge `CELL_FUNCTION_PORT` (vsock) to it.
    fn start_function(launch: &FunctionLaunch) -> std::io::Result<()> {
        if launch.start_cmd.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty start_cmd",
            ));
        }
        let workdir = launch.workdir.clone().unwrap_or_else(|| "/build".to_string());
        let _ = std::fs::create_dir_all(&workdir);

        let mut cmd = Command::new(&launch.start_cmd[0]);
        cmd.args(&launch.start_cmd[1..])
            .current_dir(&workdir)
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
            .env("HOME", "/root")
            .env("PORT", launch.port.to_string())
            .envs(launch.env.iter())
            .stdin(Stdio::null());
        // Detach: the function lives until the cell is torn down. Dropping the
        // std Child does not kill it.
        let _child = cmd.spawn()?;

        // Wait for the function to bind its port.
        let fport = launch.port;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            if TcpStream::connect(("127.0.0.1", fport)).is_ok() {
                break;
            }
            if std::time::Instant::now() > deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "function did not bind its port",
                ));
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
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
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
        if let Ok(AgentRequest::CacheData { tar }) = serde_json::from_slice::<AgentRequest>(&frame) {
            if !tar.is_empty() {
                std::fs::write("/tmp/cache_in.tgz", &tar)?;
                let _ = Command::new("tar")
                    .args(["xzf", "/tmp/cache_in.tgz", "-C", "/build"])
                    .status();
                sys(stream, format!("build cache restored [{key}] ({} bytes)", tar.len()))?;
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
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
            .env("HOME", "/root")
            .envs(job.env.iter())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Ensure the work dir exists.
        let _ = std::fs::create_dir_all("/build");

        let mut child = cmd.spawn()?;
        if let Some(out) = child.stdout.take() {
            let reader = BufReader::new(out);
            for line in reader.lines() {
                let line = line.unwrap_or_default();
                send(
                    stream,
                    &AgentEvent::Log(LogLine {
                        ts_ms: now_ms(),
                        stream: LogStream::Stdout,
                        line,
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
