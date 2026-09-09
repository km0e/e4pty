//! Windows backend: ConPTY (`CreatePseudoConsole`) via the `windows` crate.
//!
//! I/O model: ConPTY pipes only support blocking I/O, so two dedicated
//! threads own the near ends of the pipes:
//!
//! - a **reader thread** pumps `ReadFile(conout)` chunks into a tokio
//!   mpsc channel; the async reader just drains that channel. When the
//!   console closes, `ReadFile` fails/returns 0 and the thread exits.
//! - a **writer thread** consumes a tokio mpsc channel of messages
//!   (data / resize / eof) and performs the blocking `WriteFile`s and
//!   `ResizePseudoConsole` calls. An `Eof` message breaks the loop; the
//!   thread then drops the conin handle, which is what signals EOF to
//!   the child's stdin.
//! - a **waiter thread** parks on the child process handle and publishes
//!   the exit code for `wait` (keeps `INFINITE` waits off the tokio
//!   blocking pool, which a session-per-thread would exhaust).
//!
//! Because pipe/ConPTY access is confined to those threads and the
//! ConPTY handle lives behind an `Arc<Mutex<Option<HPCON>>>` that is
//! taken by `ClosePseudoConsole` on ctl drop, drops of the three handles
//! are trivially safe in any order — no spin-waits needed.
//!
//! All kernel handles are closed with `CloseHandle` (via `SafeHandle`),
//! the ConPTY handle with `ClosePseudoConsole`, and the auxiliary thread
//! handle from `CreateProcessW` right after spawn.

use std::ffi::OsStr;
use std::io::{Error, ErrorKind};
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, Notify};
use tracing::debug;

use windows::Win32::Foundation::{CloseHandle, HANDLE, MAX_PATH};
use windows::Win32::Storage::FileSystem::{ReadFile, SearchPathW, WriteFile};
use windows::Win32::System::Console::*;
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::*;
use windows::core::*;

use crate::error::Result;
use crate::pty::{Pty, PtyCtl, PtyReader, PtyWriter, WindowSize};
use crate::spawn::SpawnSpec;

/// Messages flowing from the async [`PtyWriter`] to the writer thread.
enum WriterMsg {
    /// Input bytes for the child's stdin.
    Data(Vec<u8>),
    /// Terminal resize (`ResizePseudoConsole`).
    Resize { cols: u16, rows: u16 },
    /// Close the input pipe (EOF on the child's stdin) and stop.
    Eof,
}

/// The shared ConPTY handle. `None` after [`ConptyCore::close`], so a
/// resize racing with teardown is a no-op instead of a use-after-close.
struct ConptyCore {
    hpcon: Mutex<Option<HPCON>>,
}

impl ConptyCore {
    fn resize(&self, size: WindowSize) -> windows::core::Result<()> {
        let guard = self.hpcon.lock().unwrap();
        match *guard {
            Some(h) => unsafe { ResizePseudoConsole(h, COORD::from(size)) },
            None => Ok(()),
        }
    }

    fn close(&self) {
        if let Some(h) = self.hpcon.lock().unwrap().take() {
            unsafe { ClosePseudoConsole(h) };
        }
    }
}

/// RAII wrapper closing kernel handles with `CloseHandle` on drop.
#[derive(Debug, Default)]
struct SafeHandle(HANDLE);

impl From<HANDLE> for SafeHandle {
    fn from(handle: HANDLE) -> Self {
        SafeHandle(handle)
    }
}

impl SafeHandle {
    /// Borrow the raw handle (for Win32 calls).
    fn get(&self) -> HANDLE {
        self.0
    }
}

impl Drop for SafeHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

// Raw HANDLE values are just integers; passing them across threads is fine.
unsafe impl Send for SafeHandle {}
unsafe impl Sync for SafeHandle {}

/// Shared exit status of the spawned child, produced by the dedicated
/// waiter thread and consumed by [`WinCtl::wait`].
struct ExitState {
    code: Mutex<Option<i32>>,
    notify: Notify,
}

impl ExitState {
    fn new() -> Self {
        Self {
            code: Mutex::new(None),
            notify: Notify::new(),
        }
    }

    fn set(&self, code: i32) {
        *self.code.lock().unwrap() = Some(code);
        self.notify.notify_waiters();
    }

    fn has_code(&self) -> bool {
        self.code.lock().unwrap().is_some()
    }

    /// The cached exit code, or wait for the waiter thread's
    /// notification. The future is registered (`enable`) before the
    /// check, so a concurrent `set` cannot be missed.
    async fn get(&self) -> i32 {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(code) = *self.code.lock().unwrap() {
            return code;
        }
        notified.await;
        self.code.lock().unwrap().expect("set before notify_waiters")
    }
}

/// Blocking wait loop: parks a dedicated thread on the process handle
/// (the same one-thread-per-session model as the ConPTY I/O threads) and
/// publishes the exit code. A tokio blocking-pool thread is deliberately
/// *not* used: one `INFINITE` wait per session would exhaust the pool's
/// default cap (512 threads) on long-lived sessions.
fn wait_thread(process: SafeHandle, exit: Arc<ExitState>) {
    unsafe { WaitForSingleObject(process.get(), INFINITE) };
    let mut raw = 0u32;
    let code = match unsafe { GetExitCodeProcess(process.get(), &mut raw as *mut u32) } {
        Ok(()) => raw as i32,
        Err(e) => {
            debug!("GetExitCodeProcess failed: {e}; reporting 1");
            1
        }
    };
    debug!("exit code: {}", code);
    exit.set(code);
}

/// Control half: owns the shared ConPTY state and the child's exit
/// state.
///
/// The process handle itself is owned by the dedicated waiter thread
/// (see [`wait_thread`]); this half only tracks the pid and the exit
/// code. Dropping it closes the pseudoconsole. On ConPTY semantics this
/// also signals the attached client processes, which unblocks the
/// waiter thread even when `wait` was never called — no explicit
/// `TerminateProcess` is needed for teardown.
struct WinCtl {
    pid: u32,
    conpty: Arc<ConptyCore>,
    exit: Arc<ExitState>,
}

impl Drop for WinCtl {
    fn drop(&mut self) {
        self.conpty.close();
    }
}

#[async_trait]
impl PtyCtl for WinCtl {
    async fn wait(&mut self) -> Result<i32> {
        Ok(self.exit.get().await)
    }

    fn pid(&self) -> Option<u32> {
        Some(self.pid)
    }

    async fn kill(&mut self) -> Result<()> {
        // Idempotent: once the waiter thread published the code, there is
        // nothing left to terminate.
        if self.exit.has_code() {
            return Ok(());
        }
        // Reopen by pid: the only long-lived handle belongs to the waiter
        // thread, so `WinCtl` stays lightweight and `Drop`-simple.
        let process = unsafe { OpenProcess(PROCESS_TERMINATE, false, self.pid) }
            .map_err(crate::Error::from)?;
        let process = SafeHandle::from(process);
        match unsafe { TerminateProcess(process.get(), 1) } {
            Ok(()) => Ok(()),
            // Raced with a concurrent exit: the code is in, that's fine.
            Err(_) if self.exit.has_code() => Ok(()),
            Err(e) => Err(crate::Error::from(e)),
        }
    }
}

/// Write half: an async front that feeds the writer thread over an
/// unbounded channel.
///
/// Input to a terminal is naturally small (keystrokes, small pastes), so
/// the unbounded queue is a deliberate trade-off: it lets `poll_write`
/// stay fully synchronous (tokio's bounded mpsc has no poll-based send)
/// and keeps `WinWriter` trivially `Send + Sync`. A pathological writer
/// could grow memory while the child stops consuming input.
struct WinWriter {
    tx: mpsc::UnboundedSender<WriterMsg>,
}

impl WinWriter {
    /// Queue a message for the writer thread.
    fn send_msg(&self, msg: WriterMsg) -> std::io::Result<()> {
        self.tx.send(msg).map_err(|_| broken_pipe())
    }
}

impl AsyncWrite for WinWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        // The writer thread retries partial `WriteFile`s; from here the
        // whole buffer is accepted at once.
        match self.send_msg(WriterMsg::Data(buf.to_vec())) {
            Ok(()) => std::task::Poll::Ready(Ok(buf.len())),
            Err(e) => std::task::Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // The writer thread issues `WriteFile`s as fast as messages arrive;
        // there is no userspace buffer to flush.
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // No half-close for a ConPTY input pipe; a real EOF is available
        // through `PtyWriter::eof`.
        std::task::Poll::Ready(Ok(()))
    }

    fn is_write_vectored(&self) -> bool {
        false
    }
}

#[async_trait]
impl PtyWriter for WinWriter {
    async fn window_change(&self, width: u16, height: u16) -> Result<()> {
        self.send_msg(WriterMsg::Resize {
            cols: width,
            rows: height,
        })?;
        Ok(())
    }
    async fn eof(&self) -> Result<()> {
        // The writer thread stops and drops the conin handle; ConPTY then
        // reports EOF to the child's stdin.
        self.send_msg(WriterMsg::Eof)?;
        Ok(())
    }
}

/// Read half: an async front draining the reader thread's channel.
/// Oversized chunks are buffered in `pending` so no bytes are ever lost
/// when the caller's read buffer is smaller than a chunk.
struct WinReader {
    rx: mpsc::Receiver<Vec<u8>>,
    pending: Option<Vec<u8>>,
}

impl AsyncRead for WinReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return std::task::Poll::Ready(Ok(()));
        }
        loop {
            if this.pending.is_none() {
                match this.rx.poll_recv(cx) {
                    std::task::Poll::Pending => return std::task::Poll::Pending,
                    std::task::Poll::Ready(Some(chunk)) => this.pending = Some(chunk),
                    // Channel closed: the reader thread saw the console
                    // close — report a clean EOF.
                    std::task::Poll::Ready(None) => {
                        return std::task::Poll::Ready(Ok(()));
                    }
                }
            }
            let chunk = this.pending.as_mut().unwrap();
            let unfilled = buf.initialize_unfilled();
            let n = chunk.len().min(unfilled.len());
            unfilled[..n].copy_from_slice(&chunk[..n]);
            buf.advance(n);
            if n < chunk.len() {
                chunk.drain(..n);
                // The caller's buffer is full; keep the remainder.
            } else {
                this.pending = None;
            }
            return std::task::Poll::Ready(Ok(()));
        }
    }
}

impl PtyReader for WinReader {}

fn broken_pipe() -> Error {
    Error::new(ErrorKind::BrokenPipe, "pty session closed")
}

impl From<WindowSize> for COORD {
    fn from(size: WindowSize) -> Self {
        COORD {
            X: size.cols as i16,
            Y: size.rows as i16,
        }
    }
}

/// Spawn `spec.script` attached to a fresh ConPTY pseudoconsole (Windows backend).
///
/// Unlike the Unix backend this assembles the command line itself
/// (wide-char `CreateProcessW` semantics) instead of using
/// [`std::process::Command`]. Arguments are quoted with the standard
/// Windows argv escaping rules; the working directory and environment
/// are passed via `lpCurrentDirectory` / `lpEnvironment`.
pub(crate) fn openpty(spec: SpawnSpec) -> Result<Pty> {
    let resolved = spec.script.materialize()?;
    let window_size = spec.window_size;

    // conout: the parent reads child output. CreatePipe yields
    // (read end `conout` we keep, write end `conout_pty` for ConPTY).
    let mut conout = SafeHandle::default();
    let mut conout_pty = SafeHandle::default();
    unsafe { CreatePipe(&mut conout.0, &mut conout_pty.0, None, 0) }?;

    // conin: the parent writes child input (directions mirrored).
    let mut conin_pty = SafeHandle::default();
    let mut conin = SafeHandle::default();
    unsafe { CreatePipe(&mut conin_pty.0, &mut conin.0, None, 0) }?;

    let pty_handle = unsafe {
        CreatePseudoConsole(
            COORD::from(window_size.clone()),
            conin_pty.get(),
            conout_pty.get(),
            0,
        )
    }?;
    debug!("Pseudoconsole created with handle {:?}", pty_handle);

    let mut startup_info_ex = STARTUPINFOEXW::default();
    startup_info_ex.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
    startup_info_ex.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;

    // Two-call pattern: first call with `None` reports the required
    // attribute-list size.
    let mut size: usize = 0;
    let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut size) };
    debug!("Attribute list size: {}", size);
    let mut attr_list: Box<[u8]> = vec![0; size].into_boxed_slice();
    startup_info_ex.lpAttributeList =
        LPPROC_THREAD_ATTRIBUTE_LIST(attr_list.as_mut_ptr() as *mut _);

    unsafe {
        InitializeProcThreadAttributeList(
            Some(startup_info_ex.lpAttributeList),
            1,
            None,
            &mut size as *mut usize,
        )
    }?;

    // Set thread attribute list's Pseudo Console to the specified ConPTY.
    unsafe {
        UpdateProcThreadAttribute(
            startup_info_ex.lpAttributeList,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            Some(pty_handle.0 as *mut _),
            mem::size_of::<HPCON>(),
            None,
            None,
        )
    }?;

    // `CreateProcessW` may mutate the command line buffer, hence the
    // owned, NUL-terminated UTF-16 allocation (not a borrowed &str).
    let mut cmdline = resolve_program(&resolved.program)?;
    for arg in &resolved.args {
        cmdline.push(' ' as u16);
        cmdline.extend(quote_arg(arg));
    }
    cmdline.push(0);
    debug!("command line: {}", String::from_utf16_lossy(&cmdline));

    // Working directory (None → inherit) and environment (None → inherit;
    // otherwise a NUL-separated UTF-16 "NAME=VALUE" block, double-NUL
    // terminated). The buffers must outlive the `CreateProcessW` call.
    let cwd_wide: Option<Vec<u16>> = spec.current_dir.as_ref().map(|dir| {
        dir.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    });
    let env_block: Option<Vec<u16>> = spec.env.map(build_env_block);

    let creation_flags = EXTENDED_STARTUPINFO_PRESENT;
    let mut proc_info = PROCESS_INFORMATION::default();

    unsafe {
        CreateProcessW(
            None,
            Some(PWSTR::from_raw(cmdline.as_mut_ptr())),
            None,
            None,
            false,
            creation_flags,
            env_block
                .as_ref()
                .map(|block| block.as_ptr() as *const core::ffi::c_void),
            cwd_wide
                .as_ref()
                .map(|dir| PCWSTR::from_raw(dir.as_ptr()))
                .unwrap_or_default(),
            &mut startup_info_ex.StartupInfo as *mut STARTUPINFOW,
            &mut proc_info as *mut PROCESS_INFORMATION,
        )
    }?;

    unsafe {
        DeleteProcThreadAttributeList(startup_info_ex.lpAttributeList);
    }

    // The ConPTY owns its copies of the pipe far ends now; close ours
    // (SafeHandle drop → CloseHandle). The primary thread handle of the
    // child is likewise never needed.
    drop(conout_pty);
    drop(conin_pty);
    let thread = SafeHandle::from(proc_info.hThread);
    drop(thread);

    let pid = unsafe { GetProcessId(proc_info.hProcess) };
    let exit = Arc::new(ExitState::new());
    // Dedicated waiter thread: takes ownership of the only process
    // handle, parks until the child exits and publishes the code for
    // `WinCtl::wait` (see the module docs for why not `spawn_blocking`).
    // (`SafeHandle` first: raw `HANDLE` is `!Send` and cannot cross into
    // the closure.)
    let process = SafeHandle::from(proc_info.hProcess);
    let exit_for_thread = Arc::clone(&exit);
    std::thread::spawn(move || wait_thread(process, exit_for_thread));

    let conpty = Arc::new(ConptyCore {
        hpcon: Mutex::new(Some(pty_handle)),
    });

    // Channels between the async fronts and the blocking I/O threads.
    // Writer side is unbounded (see `WinWriter` docs); reader side is
    // bounded so a stalled consumer applies backpressure to the thread.
    let (in_tx, in_rx) = mpsc::unbounded_channel::<WriterMsg>();
    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(32);

    // Writer thread: owns the near end of conin. Exits on `Eof` or when
    // the sender side is dropped; dropping `conin` then signals EOF.
    let conpty_writer = Arc::clone(&conpty);
    std::thread::spawn(move || writer_thread(conin, conpty_writer, in_rx));
    // Reader thread: owns the near end of conout. Exits when the console
    // closes (ReadFile fails / returns 0) or the receiver is dropped.
    std::thread::spawn(move || reader_thread(conout, out_tx));

    Ok(Pty::new(
        WinCtl {
            pid,
            conpty,
            exit,
        },
        WinWriter { tx: in_tx },
        WinReader {
            rx: out_rx,
            pending: None,
        },
    ))
}

/// Blocking writer loop: drains the channel and performs the actual
/// `WriteFile` / `ResizePseudoConsole` calls (partial writes are retried).
fn writer_thread(
    pipe: SafeHandle,
    conpty: Arc<ConptyCore>,
    mut rx: mpsc::UnboundedReceiver<WriterMsg>,
) {
    while let Some(msg) = rx.blocking_recv() {
        match msg {
            WriterMsg::Data(data) => {
                let mut offset = 0;
                while offset < data.len() {
                    let mut written = 0u32;
                    let res = unsafe {
                        WriteFile(pipe.get(), Some(&data[offset..]), Some(&mut written), None)
                    };
                    match res {
                        Ok(()) if written > 0 => offset += written as usize,
                        _ => return, // pipe broken: console is gone
                    }
                }
            }
            WriterMsg::Resize { cols, rows } => {
                let _ = conpty.resize(WindowSize { rows, cols });
            }
            WriterMsg::Eof => break,
        }
    }
    // Dropping `pipe` (CloseHandle) is what delivers EOF to the child.
}

/// Blocking reader loop: pumps chunks from the console output pipe into
/// the async channel until EOF or the receiver is dropped.
fn reader_thread(pipe: SafeHandle, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 8192];
    loop {
        let mut bytes = 0u32;
        let res = unsafe { ReadFile(pipe.get(), Some(&mut buf), Some(&mut bytes), None) };
        match res {
            Ok(()) if bytes > 0 => {
                debug!("read {} bytes", bytes);
                if tx.blocking_send(buf[..bytes as usize].to_vec()).is_err() {
                    // Receiver dropped: nothing left to report to.
                    break;
                }
            }
            _ => break, // 0 bytes or error: console closed → EOF
        }
    }
}

/// Build a `NAME=VALUE\0…\0` UTF-16 environment block for
/// `CreateProcessW`.
///
/// Windows environment variable names are case-insensitive, so entries
/// that collide case-insensitively are deduplicated with the last value
/// winning (the parent env may hold `Path` while the caller overrides
/// `PATH`, for example).
fn build_env_block(vars: Vec<(std::ffi::OsString, std::ffi::OsString)>) -> Vec<u16> {
    fn upper16(c: u16) -> u16 {
        if (b'a' as u16..=b'z' as u16).contains(&c) {
            c - (b'a' as u16 - b'A' as u16)
        } else {
            c
        }
    }

    let mut entries: Vec<(Vec<u16>, Vec<u16>)> = Vec::with_capacity(vars.len());
    for (key, value) in vars {
        let key16: Vec<u16> = key.encode_wide().collect();
        match entries.iter_mut().find(|(name, _)| {
            name.len() == key16.len()
                && name
                    .iter()
                    .zip(&key16)
                    .all(|(a, b)| upper16(*a) == upper16(*b))
        }) {
            Some(slot) => slot.1 = value.encode_wide().collect(),
            None => entries.push((key16, value.encode_wide().collect())),
        }
    }

    let mut block = Vec::new();
    for (name, value) in entries {
        block.extend(name);
        block.push('=' as u16);
        block.extend(value);
        block.push(0);
    }
    block.push(0); // terminating double NUL
    block
}

/// Resolve a program name to an absolute, NUL-terminated UTF-16 path via
/// `SearchPathW` (appends `.exe` when the name has no extension).
fn resolve_program(program: &OsStr) -> std::io::Result<Vec<u16>> {
    debug!("searching for {}", program.to_string_lossy());
    let mut filename = program
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();
    let mut buf = vec![0u16; MAX_PATH as usize];
    let len = unsafe {
        SearchPathW(
            None,
            PCWSTR::from_raw(filename.as_mut_ptr()),
            w!(".exe"),
            Some(buf.as_mut_slice()),
            None,
        )
    };
    if len == 0 {
        return Err(Error::last_os_error());
    }
    // `SearchPathW` wrote the resolved absolute path into `buf`.
    buf.truncate(len as usize);
    Ok(buf)
}

/// Quote one argument with the standard Windows argv escaping rules
/// (backslashes before quotes are doubled, embedded quotes are escaped,
/// arguments containing whitespace/quotes get wrapped in double quotes).
fn quote_arg(arg: &OsStr) -> Vec<u16> {
    let s: Vec<u16> = arg.encode_wide().collect();
    let needs_quotes = s.is_empty()
        || s.iter()
            .any(|&c| c == ' ' as u16 || c == '\t' as u16 || c == '"' as u16);
    if !needs_quotes {
        return s;
    }
    let mut out = Vec::with_capacity(s.len() + 2);
    out.push('"' as u16);
    let mut backslashes = 0usize;
    for &c in &s {
        if c == '\\' as u16 {
            backslashes += 1;
            out.push(c);
        } else if c == '"' as u16 {
            // 2n+1 backslashes before an embedded quote.
            for _ in 0..(backslashes * 2 + 1) {
                out.push('\\' as u16);
            }
            backslashes = 0;
            out.push(c);
        } else {
            backslashes = 0;
            out.push(c);
        }
    }
    // Double the trailing backslashes before the closing quote.
    for _ in 0..backslashes {
        out.push('\\' as u16);
    }
    out.push('"' as u16);
    out
}
