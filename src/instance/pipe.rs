//! The single-instance named pipe: the identity of this process, the
//! receiving end (`create_pipe`, `PipeHandle::serve`, `Server::stop_serving`, `shutdown`) and the sending end
//! (`forward`). The exchange is four steps: the sender writes the request, the receiver answers `ready` or `refused`,
//! the sender writes `commit`, the receiver acts and answers `done`.
//!
//! Everything is overlapped I/O with a timeout, on both ends, for two reasons: a client that connects and then says
//! nothing must not block the receiver, and a thread blocked in a synchronous `ReadFile` cannot be stopped (closing
//! a synchronous handle from another thread waits for the pending read), so `stop_serving` could not free the name.
//! On a timeout or a stop the operation is cancelled (`CancelIoEx`) and its completion waited for before the
//! `OVERLAPPED` and the buffer go out of scope, since the kernel writes into both until then.
//!
//! Nothing here may panic on what a client sends (release builds abort on panic and would take the user's window
//! with them): no indexing, `unwrap` or `expect` on received bytes or on Win32 results.

use super::{decode_request, encode_request, exe_key, pipe_name, wide, Request, MAX_MESSAGE};
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    GetLastError, LocalFree, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_MORE_DATA, ERROR_NO_DATA, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT};
use windows_sys::Win32::Security::{
    GetAce, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, IsValidSid, TokenIntegrityLevel, TokenIsAppContainer, TokenSessionId, TokenStatistics, TokenUser,
    ACE_HEADER, ACL, LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SYSTEM_MANDATORY_LABEL_ACE, TOKEN_INFORMATION_CLASS,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_STATISTICS, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, READ_CONTROL, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResultEx, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId, WaitNamedPipeW, PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcess, OpenProcessToken, SetEvent, WaitForMultipleObjects, WaitForSingleObject, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

/// The one-byte replies and the commit. Any other byte, or more than one, is a protocol error.
const READY: u8 = b'y';
const REFUSED: u8 = b'n';
const COMMIT: u8 = b'c';
const DONE: u8 = b'd';

/// How long a connected client has to deliver its request. A client that connects and says nothing is dropped after
/// this, so it cannot hold the only pipe instance and block every later launch.
const REQUEST_MS: u32 = 2000;
/// How long the receiver waits for `commit` after `ready`. Shorter than the sender's `DONE_MS`, so a sender that
/// sees no `done` can assume the receiver acted (see `forward`).
const COMMIT_MS: u32 = 1000;
/// How long the receiver waits for the sender to close after the last reply: `DisconnectNamedPipe` would discard a
/// reply the sender has not read yet.
const CLOSE_MS: u32 = 1000;
/// Cap on writing one reply byte. A message-mode write of one byte into an empty pipe completes at once; this only
/// guards against a pipe that is somehow full.
const WRITE_MS: u32 = 1000;
/// How long the sender waits for `done` after `commit`; no `done` within it counts as delivered.
const DONE_MS: u32 = 2000;
/// How long `stop_serving` and `shutdown` wait for a server thread to cancel its I/O and close the handle.
const STOP_MS: u32 = 200;

/// Integrity RIDs: `SECURITY_MANDATORY_MEDIUM_RID` (the windows-sys constant needs the large `SystemServices`
/// feature for one number).
const MEDIUM_RID: u32 = 0x2000;
/// `SYSTEM_MANDATORY_LABEL_ACE_TYPE`, for the same reason.
const MANDATORY_LABEL_ACE: u8 = 0x11;

/// Set once by `shutdown`: from then on every server refuses and no pipe is created. Process-wide on purpose (the
/// process is ending), unlike `Server::stop_serving`, after which the window may claim the name again.
static CLOSING: AtomicBool = AtomicBool::new(false);

// ---- identity ----

/// Who a process runs as, for the pipe name and for the check of the other end: the token user's SID string, the
/// logon session (`TokenStatistics.AuthenticationId`, so a `runas` to oneself or the elevated half of a split token
/// gets its own name; drive letters and `subst` are per logon), the integrity RID and the Terminal Services session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub sid: String,
    pub logon: u64,
    pub level: u32,
    pub session: u32,
}

/// This process's identity. Low integrity and AppContainer processes (browser sandboxes, packaged apps) get an error:
/// they neither serve nor forward, since a pipe labelled at their level could be opened by other sandboxed
/// processes of the same user.
pub fn own_identity() -> io::Result<Identity> {
    let mut token: HANDLE = null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo handle that needs no closing; `token` is written on success.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `token` is a valid handle that nothing else owns.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let app_container = read_token::<u32, _>(&token, TokenIsAppContainer, |&v| Ok(v != 0))?;
    let id = token_identity(&token)?;
    if !takes_part(app_container, id.level) {
        return Err(io::Error::other(format!("no part in single-window mode (AppContainer: {app_container}, integrity level {:#x})", id.level)));
    }
    Ok(id)
}

/// Only medium and higher integrity outside an AppContainer serve or forward. Medium plus (0x2100, a
/// UIAccess process) and high (elevated) take part, each paired with its own level through the pipe name.
fn takes_part(app_container: bool, level: u32) -> bool {
    !app_container && level >= MEDIUM_RID
}

/// The pipe name for this process and this exe: one receiver per user, logon, integrity level and exe.
pub fn name_for_this_exe() -> io::Result<String> {
    let id = own_identity()?;
    let exe = std::env::current_exe()?;
    Ok(pipe_name(&id.sid, id.logon, id.level, &exe_key(&exe)))
}

/// Whether the process at the other end of the pipe is the same user, logon, integrity level and session as this one.
/// All four, not only the SID: the same user in another logon sees other drive letters, and a normal launch must
/// never steer an elevated window.
pub fn identity_matches(own: &Identity, theirs: &Identity) -> bool {
    own.sid == theirs.sid && own.logon == theirs.logon && own.level == theirs.level && own.session == theirs.session
}

/// The identity recorded in `token` (no level check: this is also used for the other end, where the comparison does it).
fn token_identity(token: &OwnedHandle) -> io::Result<Identity> {
    let sid = read_token::<TOKEN_USER, _>(token, TokenUser, |u| sid_string(u.User.Sid))?;
    let logon = read_token::<TOKEN_STATISTICS, _>(token, TokenStatistics, |s| {
        let a = s.AuthenticationId;
        Ok((u64::from(a.HighPart as u32) << 32) | u64::from(a.LowPart))
    })?;
    let level = read_token::<TOKEN_MANDATORY_LABEL, _>(token, TokenIntegrityLevel, |l| sid_rid(l.Label.Sid).ok_or_else(|| io::Error::other("integrity SID has no RID")))?;
    let session = read_token::<u32, _>(token, TokenSessionId, |&s| Ok(s))?;
    Ok(Identity { sid, logon, level, session })
}

/// One `GetTokenInformation` class, read into a buffer of the size the first call reports and handed to `f` as `T`.
/// The buffer lives until `f` returns, because structures such as `TOKEN_USER` point into it.
fn read_token<T, R>(token: &OwnedHandle, class: TOKEN_INFORMATION_CLASS, f: impl FnOnce(&T) -> io::Result<R>) -> io::Result<R> {
    let h = token.as_raw_handle() as HANDLE;
    let mut len = 0u32;
    // SAFETY: a null buffer of length 0 only asks for the size, which is written to `len`.
    unsafe { GetTokenInformation(h, class, null_mut(), 0, &mut len) };
    if (len as usize) < size_of::<T>() {
        return Err(io::Error::last_os_error());
    }
    // u64 elements: 8-byte aligned, enough for every token structure (they hold pointers at most).
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` is writable for `len` bytes.
    if unsafe { GetTokenInformation(h, class, buf.as_mut_ptr().cast(), len, &mut len) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call filled at least `size_of::<T>()` bytes (checked above) of an aligned buffer with a `T`.
    f(unsafe { &*buf.as_ptr().cast::<T>() })
}

/// `S-1-5-...` for a SID.
fn sid_string(sid: PSID) -> io::Result<String> {
    // SAFETY: `IsValidSid` only reads the SID.
    if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
        return Err(io::Error::other("invalid SID"));
    }
    let mut p = null_mut();
    // SAFETY: `sid` is valid; on success `p` is a NUL-terminated string to be freed with `LocalFree`.
    if unsafe { ConvertSidToStringSidW(sid, &mut p) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `p` is NUL-terminated (a SID string is at most a few hundred characters; the bound is a guard only).
    let units: Vec<u16> = (0..1024).map(|i| unsafe { *p.add(i) }).take_while(|&u| u != 0).collect();
    // SAFETY: `p` came from `ConvertSidToStringSidW`.
    unsafe { LocalFree(p.cast()) };
    String::from_utf16(&units).map_err(|_| io::Error::other("SID string is not UTF-16"))
}

/// The last sub-authority of a SID: for a mandatory label SID (`S-1-16-<rid>`), the integrity level.
fn sid_rid(sid: PSID) -> Option<u32> {
    // SAFETY: `IsValidSid` only reads the SID; the sub-authority functions return pointers into a valid SID and the
    // index is below its count.
    unsafe {
        if sid.is_null() || IsValidSid(sid) == 0 {
            return None;
        }
        let count = *GetSidSubAuthorityCount(sid);
        let last = u32::from(count).checked_sub(1)?;
        Some(*GetSidSubAuthority(sid, last))
    }
}

// ---- events and overlapped I/O ----

/// A manual-reset event, used as an `OVERLAPPED` event and as the stop signal.
struct Event(OwnedHandle);

impl Event {
    fn new() -> io::Result<Event> {
        // SAFETY: no security attributes (so not inheritable), manual reset, initially clear, no name.
        let h = unsafe { CreateEventW(null(), 1, 0, null()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `h` is a valid handle that nothing else owns.
        Ok(Event(unsafe { OwnedHandle::from_raw_handle(h) }))
    }

    fn raw(&self) -> HANDLE {
        self.0.as_raw_handle() as HANDLE
    }

    fn set(&self) {
        // SAFETY: the handle is a valid event.
        unsafe { SetEvent(self.raw()) };
    }

    fn is_set(&self) -> bool {
        // SAFETY: the handle is a valid event; a zero timeout only polls.
        unsafe { WaitForSingleObject(self.raw(), 0) == WAIT_OBJECT_0 }
    }

    /// Waits up to `ms` for the event; true if it was set.
    fn wait(&self, ms: u32) -> bool {
        // SAFETY: the handle is a valid event.
        unsafe { WaitForSingleObject(self.raw(), ms) == WAIT_OBJECT_0 }
    }
}

/// How one overlapped operation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Io {
    /// Completed; the byte count.
    Done(u32),
    /// Failed with this Win32 error (`ERROR_MORE_DATA` included: a message longer than the buffer).
    Failed(u32),
    /// Not complete within the timeout; cancelled.
    TimedOut,
    /// The stop event was set first; cancelled.
    Stopped,
}

/// Starts one overlapped operation through `start` and waits for it for at most `ms`, or until `stop` is set.
///
/// `start` gets a fresh `OVERLAPPED` (with `ev` as its event) and must issue exactly one operation on `h` with it,
/// using a buffer that outlives this call. This function does not return while the operation can still touch either:
/// on a timeout or stop it cancels with `CancelIoEx` and then waits, without a limit, for the cancellation to complete.
/// That wait is short (cancelling a pipe read, write or connect completes at once); leaving it out would let the
/// kernel write into a freed stack frame.
///
/// The wait itself is `WaitForMultipleObjects` on the I/O event and the stop event, so a stop wakes it at once;
/// `GetOverlappedResultEx` then collects the result without waiting (or waits for the cancellation).
fn overlapped(h: HANDLE, ev: &Event, stop: Option<&Event>, ms: u32, start: impl FnOnce(*mut OVERLAPPED) -> i32) -> Io {
    // SAFETY: an all-zero OVERLAPPED is the documented starting state; only the event is set.
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = ev.raw();
    if start(&mut ov) == 0 {
        // SAFETY: trivially safe.
        let e = unsafe { GetLastError() };
        if e != ERROR_IO_PENDING {
            // Finished synchronously with an error (ERROR_PIPE_CONNECTED, ERROR_NO_DATA, ERROR_MORE_DATA, a broken
            // pipe): nothing is pending, so returning is safe.
            return Io::Failed(e);
        }
    }
    // Pending, or done synchronously (then the event is already set and the wait returns at once).
    let handles = [ev.raw(), stop.map_or(null_mut(), Event::raw)];
    let count = if stop.is_some() { 2 } else { 1 };
    // SAFETY: `handles` holds `count` valid handles. The I/O event comes first, so a completion wins over a stop
    // that arrives at the same moment.
    let woke = unsafe { WaitForMultipleObjects(count, handles.as_ptr(), 0, ms) };
    let mut n = 0u32;
    if woke == WAIT_OBJECT_0 {
        // SAFETY: the operation has completed; `ov` is the one it was started with.
        return if unsafe { GetOverlappedResultEx(h, &ov, &mut n, 0, 0) } != 0 { Io::Done(n) } else { Io::Failed(unsafe { GetLastError() }) };
    }
    // Timed out, stopped, or the wait failed: cancel and wait for the operation to let go of `ov` and the buffer.
    // SAFETY: `ov` is the pending operation's; `CancelIoEx` failing (ERROR_NOT_FOUND) means it completed meanwhile,
    // and the INFINITE wait then returns at once.
    unsafe {
        CancelIoEx(h, &ov);
        GetOverlappedResultEx(h, &ov, &mut n, INFINITE, 0);
    }
    if woke == WAIT_OBJECT_0 + 1 { Io::Stopped } else { Io::TimedOut }
}

fn read_into(h: HANDLE, ev: &Event, stop: Option<&Event>, ms: u32, buf: &mut [u8]) -> Io {
    let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    let p = buf.as_mut_ptr();
    // SAFETY: `buf` is writable for `len` bytes and outlives the operation (`overlapped` waits for it).
    overlapped(h, ev, stop, ms, |ov| unsafe { ReadFile(h, p, len, null_mut(), ov) })
}

fn write_all(h: HANDLE, ev: &Event, stop: Option<&Event>, ms: u32, bytes: &[u8]) -> Io {
    let Ok(len) = u32::try_from(bytes.len()) else { return Io::Failed(ERROR_MORE_DATA) };
    // SAFETY: `bytes` is readable for `len` bytes and outlives the operation (`overlapped` waits for it).
    match overlapped(h, ev, stop, ms, |ov| unsafe { WriteFile(h, bytes.as_ptr(), len, null_mut(), ov) }) {
        // A message-mode write is all or nothing; a short count would be a broken pipe.
        Io::Done(n) if n != len => Io::Failed(ERROR_BROKEN_PIPE),
        other => other,
    }
}

/// Reads one message that must be exactly one byte.
fn read_byte(h: HANDLE, ev: &Event, stop: Option<&Event>, ms: u32) -> Result<u8, Io> {
    let mut b = [0u8; 1];
    match read_into(h, ev, stop, ms, &mut b) {
        Io::Done(1) => {
            let [byte] = b;
            Ok(byte)
        }
        // A zero-length message is not a reply.
        Io::Done(_) => Err(Io::Failed(ERROR_MORE_DATA)),
        other => Err(other),
    }
}

/// The other end has gone (closed its handle, or the server disconnected).
fn is_broken(e: u32) -> bool {
    matches!(e, ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED)
}

// ---- the receiving end ----

/// The server end of the pipe, created but not yet served. It owns the handle through the process-wide registry, so
/// `shutdown` can close a pipe that was never served (a glow panic before `App::new` runs, which relaunches with wgpu).
/// Dropping it closes the handle and frees the name.
pub struct PipeHandle {
    id: u64,
}

/// Why `create_pipe` did not create the pipe.
#[derive(Debug)]
pub enum CreateError {
    /// The name exists: another process receives, so this launch should forward.
    Taken,
    /// Anything else (also: the process is shutting down). The launch opens its own window without the pipe.
    Other(io::Error),
}

/// What the registry holds per pipe: the handle until `serve` moves it to its thread, then the server's signals.
enum Slot {
    Unserved(OwnedHandle),
    Serving(Arc<ServerShared>),
}

/// Every pipe of this process, so `shutdown` can reach them from the panic hook without the `PipeHandle` or `Server`.
static REGISTRY: Mutex<Vec<(u64, Slot)>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The registry, poisoned or not (nothing under the lock can panic, but a poisoned lock must not stop a shutdown).
fn registry() -> MutexGuard<'static, Vec<(u64, Slot)>> {
    REGISTRY.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Creates the receiving pipe `name` for `id`. Exactly one process can: `ERROR_ACCESS_DENIED`
/// (the name exists, `FILE_FLAG_FIRST_PIPE_INSTANCE`) and `ERROR_PIPE_BUSY` (its one instance is in use) both mean
/// another process got there first.
pub fn create_pipe(name: &str, id: &Identity) -> Result<PipeHandle, CreateError> {
    // Owner: the user. DACL, protected (no inherited entries): generic all for the user and nobody else, so no other
    // user (Administrators, SYSTEM included) can open it. SACL: a mandatory label at the process's own level with
    // no-write-up and no-read-up, so lower-integrity processes of the same user (browser sandboxes) cannot open it at
    // all, not even to read. The level is written as the label SID `S-1-16-<rid>`, which SDDL accepts like `ME` or
    // `HI` and which also covers in-between levels such as medium plus (0x2100). A process may label an object at its
    // own level without a privilege.
    let sddl = format!("O:{sid}D:P(A;;GA;;;{sid})S:(ML;;NWNR;;;S-1-16-{level})", sid = id.sid, level = id.level);
    create_pipe_sddl(name, &sddl)
}

fn create_pipe_sddl(name: &str, sddl: &str) -> Result<PipeHandle, CreateError> {
    if CLOSING.load(Ordering::SeqCst) {
        return Err(CreateError::Other(io::Error::other("shutting down")));
    }
    let sddl_w = wide(sddl);
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `sddl_w` is NUL-terminated; on success `sd` is freed with `LocalFree` below.
    if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl_w.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) } == 0 {
        return Err(CreateError::Other(io::Error::last_os_error()));
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        // Not inheritable: `relaunch_with_wgpu` (win.rs) starts a child with `std::process::Command`, which inherits
        // every inheritable handle, and a child holding the server end would keep the name taken without serving it.
        bInheritHandle: 0,
    };
    let name_w = wide(name);
    // SAFETY: `name_w` is NUL-terminated and `sa` (with its descriptor) is valid for the call.
    let h = unsafe {
        CreateNamedPipeW(
            name_w.as_ptr(),
            // Overlapped, so every wait has a timeout and a stop. First instance: fail if the name exists, so another
            // process (another user's included) cannot have created it first and be joined unknowingly.
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
            // Message mode: a request is one message, so its end is known without a length prefix. Remote clients
            // are refused: the pipe is for this machine only.
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            // One instance, reused for every client, so the name never disappears while the receiver runs.
            1,
            // Out: replies are one byte. In: room for one whole request.
            64,
            MAX_MESSAGE as u32,
            // Default for `WaitNamedPipeW(NMPWAIT_USE_DEFAULT_WAIT)`; `forward` always passes its own time.
            0,
            &sa,
        )
    };
    // SAFETY: trivially safe; read before `LocalFree` can change it.
    let err = unsafe { GetLastError() };
    // SAFETY: `sd` came from `ConvertStringSecurityDescriptorToSecurityDescriptorW`; the pipe keeps its own copy.
    unsafe { LocalFree(sd) };
    if h == INVALID_HANDLE_VALUE {
        return Err(match err {
            ERROR_ACCESS_DENIED | ERROR_PIPE_BUSY => CreateError::Taken,
            e => CreateError::Other(io::Error::from_raw_os_error(e as i32)),
        });
    }
    // SAFETY: `h` is a valid handle that nothing else owns.
    let handle = unsafe { OwnedHandle::from_raw_handle(h) };
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut reg = registry();
    // Checked again under the lock: a `shutdown` that ran since the first check has already emptied the registry and
    // would not see this handle.
    if CLOSING.load(Ordering::SeqCst) {
        return Err(CreateError::Other(io::Error::other("shutting down")));
    }
    reg.push((id, Slot::Unserved(handle)));
    Ok(PipeHandle { id })
}

impl Drop for PipeHandle {
    fn drop(&mut self) {
        // Only a handle still unserved; once served, the entry belongs to the server thread.
        let mut reg = registry();
        if let Some(pos) = reg.iter().position(|(i, s)| *i == self.id && matches!(s, Slot::Unserved(_))) {
            reg.swap_remove(pos);
        }
    }
}

/// What a server thread and its `Server` share: the stop signal and the thread's "handle closed" signal.
struct ServerShared {
    stop: Event,
    done: Event,
}

/// A serving pipe: its thread answers senders until `stop_serving` or `shutdown`. Dropping a `Server` does not stop
/// it (the thread then serves until `shutdown`); keep it to stop it or to ask `is_finished`.
pub struct Server {
    /// None for a server that never ran (its pipe was already closed, or its events could not be made).
    shared: Option<Arc<ServerShared>>,
}

type Accept = Arc<dyn Fn(&Request) -> bool + Send + Sync>;
type Commit = Arc<dyn Fn(Request) + Send + Sync>;

impl PipeHandle {
    /// Starts the server thread. For each sender: it gets `REQUEST_MS` to deliver its request; the reply
    /// is `ready` if the request decodes, the process is not closing and `accept` agrees, else `refused`; after
    /// `ready` it gets `COMMIT_MS` to write `commit`, and only then is `commit(request)` called, on the server thread;
    /// then `done`, a wait of up to `CLOSE_MS` for the sender to close, and a disconnect.
    ///
    /// `accept` and `commit` run on this one pipe thread while their sender holds the only pipe instance, so every
    /// other sender queues behind them. They must:
    /// - return well within `DONE_MS` (2 s): a sender that gets no `done` by then counts the file as delivered;
    /// - never wait on the UI thread, or on a lock the UI thread may hold while it calls `stop_serving` or `shutdown`
    ///   (those wait for this thread, up to `STOP_MS`, and the exchange would stall until the sender gives up);
    /// - never panic: release builds abort, taking the window with them.
    ///
    /// So `commit` should only hand the request to a channel or worker and do the non-blocking restore and focus of
    /// the window (`ShowWindowAsync`, `SetForegroundWindow`), and `accept` should only read cheap state.
    ///
    /// If the pipe was already closed by `shutdown`, or the thread or its events cannot be made, the result is a
    /// finished `Server` and the name is freed.
    pub fn serve(self, accept: Accept, commit: Commit) -> Server {
        let finished = Server { shared: None };
        let (Ok(stop), Ok(done)) = (Event::new(), Event::new()) else { return finished };
        let shared = Arc::new(ServerShared { stop, done });
        let handle = {
            let mut reg = registry();
            let Some(pos) = reg.iter().position(|(i, s)| *i == self.id && matches!(s, Slot::Unserved(_))) else { return finished };
            let (_, slot) = reg.swap_remove(pos);
            let Slot::Unserved(handle) = slot else { return finished };
            // Registered under the same lock as the take, so a `shutdown` sees either the unserved handle or the server.
            reg.push((self.id, Slot::Serving(shared.clone())));
            handle
        };
        let exit = ServerExit { id: self.id, handle: Some(handle), shared: shared.clone() };
        // The join handle is not kept: `done` (set by `ServerExit`) says when the thread is through with the pipe,
        // which is what `stop_serving` and `is_finished` need. A spawn failure drops the closure, and with it `exit`,
        // which frees the name and sets `done`.
        let _ = std::thread::Builder::new().name("mdread-pipe".into()).spawn(move || {
            if let Some(h) = &exit.handle {
                serve_loop(h.as_raw_handle() as HANDLE, &exit.shared.stop, &*accept, &*commit);
            }
            drop(exit);
        });
        Server { shared: Some(shared) }
    }
}

/// Runs when the server thread ends, however it ends (also when the closure is dropped unspawned): disconnects any
/// client, closes the handle (freeing the name), leaves the registry, and only then sets `done`, so whoever waits on
/// `done` knows the name is free.
struct ServerExit {
    id: u64,
    handle: Option<OwnedHandle>,
    shared: Arc<ServerShared>,
}

impl Drop for ServerExit {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            // SAFETY: valid pipe handle; disconnecting an unconnected pipe only returns an error.
            unsafe { DisconnectNamedPipe(h.as_raw_handle() as HANDLE) };
            drop(h);
        }
        let mut reg = registry();
        if let Some(pos) = reg.iter().position(|(i, _)| *i == self.id) {
            reg.swap_remove(pos);
        }
        drop(reg);
        self.shared.done.set();
    }
}

impl Server {
    /// Stops this server (the setting turned off): signals its stop event and waits at most `STOP_MS` for the thread to cancel
    /// its I/O and close the handle, which frees the name. Sets no process-wide state, so the name can be claimed
    /// again. If the thread takes longer it finishes on its own; `is_finished` tells when. Idempotent.
    pub fn stop_serving(&self) {
        if let Some(s) = &self.shared {
            s.stop.set();
            s.done.wait(STOP_MS);
        }
    }

    /// True once the pipe handle is closed and the server has left the registry (or it never ran): the name is free
    /// and this server will never touch it again. Based on `done`, which `ServerExit` sets as its last step, not on
    /// the thread's own end, which follows a moment later; so it is reliable right after a `stop_serving` that
    /// returned in time.
    pub fn is_finished(&self) -> bool {
        self.shared.as_ref().is_none_or(|s| s.done.is_set())
    }
}

/// Process-final stop (on exit and from the panic hook): sets `CLOSING`, so every server refuses from now on and no pipe is created, then
/// stops every serving server (as `stop_serving`, sharing one `STOP_MS` wait) and closes every pipe that was never
/// served. Idempotent. Safe from the panic hook: it takes the registry lock with `try_lock` and gives up after about
/// 100 ms, because `Mutex` is not reentrant and a panic on a thread holding it must not deadlock the hook; `CLOSING`
/// alone already makes a running server refuse.
pub fn shutdown() {
    CLOSING.store(true, Ordering::SeqCst);
    let mut guard = None;
    for _ in 0..50 {
        match REGISTRY.try_lock() {
            Ok(g) => guard = Some(g),
            Err(TryLockError::Poisoned(p)) => guard = Some(p.into_inner()),
            Err(TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
        }
        break;
    }
    let Some(mut reg) = guard else { return };
    let mut unserved = Vec::new();
    let mut servers = Vec::new();
    let mut i = 0;
    while i < reg.len() {
        match reg.get(i) {
            Some((_, Slot::Unserved(_))) => {
                let (_, slot) = reg.swap_remove(i);
                unserved.push(slot);
            }
            Some((_, Slot::Serving(s))) => {
                servers.push(s.clone());
                i += 1;
            }
            None => break,
        }
    }
    // Released before the waits: each server thread takes the lock to leave the registry.
    drop(reg);
    // Closes the never-served handles.
    drop(unserved);
    for s in &servers {
        s.stop.set();
    }
    let until = Instant::now() + Duration::from_millis(u64::from(STOP_MS));
    for s in &servers {
        let left = until.saturating_duration_since(Instant::now()).as_millis() as u32;
        s.done.wait(left);
    }
}

#[cfg(test)]
fn reset_closing() {
    CLOSING.store(false, Ordering::SeqCst);
}

/// Sets `CLOSING` alone, without stopping anything, to test a live server that sees it.
#[cfg(test)]
fn set_closing() {
    CLOSING.store(true, Ordering::SeqCst);
}

/// Takes the raw server handle out of an unserved `PipeHandle`, for tests that play a misbehaving server.
#[cfg(test)]
fn take_raw(pipe: PipeHandle) -> Option<OwnedHandle> {
    let slot = {
        let mut reg = registry();
        let pos = reg.iter().position(|(i, s)| *i == pipe.id && matches!(s, Slot::Unserved(_)))?;
        reg.swap_remove(pos).1
    };
    // Dropped only after the lock is released: its `Drop` takes the lock too (and now finds nothing).
    drop(pipe);
    match slot {
        Slot::Unserved(h) => Some(h),
        Slot::Serving(_) => None,
    }
}

/// Whether to go on serving after a client.
enum Flow {
    Next,
    Stop,
}

fn serve_loop(h: HANDLE, stop: &Event, accept: &(dyn Fn(&Request) -> bool + Send + Sync), commit: &(dyn Fn(Request) + Send + Sync)) {
    let Ok(ev) = Event::new() else { return };
    // One request buffer for the thread's life, exactly the cap: a longer message ends in ERROR_MORE_DATA and is
    // refused, so nothing a client sends is buffered past `MAX_MESSAGE`.
    let mut buf = vec![0u8; MAX_MESSAGE];
    while !stop.is_set() {
        // Waits for a client with no timeout (only a stop ends it).
        // SAFETY: `h` is the server end; the `OVERLAPPED` comes from `overlapped`.
        match overlapped(h, &ev, Some(stop), INFINITE, |ov| unsafe { ConnectNamedPipe(h, ov) }) {
            // ERROR_PIPE_CONNECTED: the client connected between the last disconnect (or the creation) and this call;
            // that is a good connection.
            Io::Done(_) | Io::Failed(ERROR_PIPE_CONNECTED) => {}
            // ERROR_NO_DATA: the client connected and left before this call. Disconnect and listen again.
            Io::Failed(ERROR_NO_DATA) => {
                // SAFETY: valid pipe handle.
                unsafe { DisconnectNamedPipe(h) };
                continue;
            }
            Io::Stopped => return,
            other => {
                // Unexpected: give up the role (the name is freed and the next launch claims it) rather than spin.
                log::warn!("single-window pipe: ConnectNamedPipe failed ({other:?}); no longer receiving");
                return;
            }
        }
        let flow = serve_client(h, &ev, stop, &mut buf, accept, commit);
        // SAFETY: valid pipe handle. Drops the client, also one that is still connected after a timeout.
        unsafe { DisconnectNamedPipe(h) };
        if let Flow::Stop = flow {
            return;
        }
    }
}

/// One sender, from its request to its close.
fn serve_client(h: HANDLE, ev: &Event, stop: &Event, buf: &mut [u8], accept: &(dyn Fn(&Request) -> bool + Send + Sync), commit: &(dyn Fn(Request) + Send + Sync)) -> Flow {
    let decoded = match read_into(h, ev, Some(stop), REQUEST_MS, buf) {
        Io::Done(n) => decode_request(buf.get(..n as usize).unwrap_or(&[])),
        // Longer than the cap: refused, unread past `buf`.
        Io::Failed(ERROR_MORE_DATA) => Err(super::DecodeError::TooLong),
        Io::Stopped => return Flow::Stop,
        // A broken pipe, or no request within REQUEST_MS: drop the client.
        Io::Failed(_) | Io::TimedOut => return Flow::Next,
    };
    // CLOSING first: once the process is closing, `accept` (app code) is not asked any more.
    let accepted = match decoded {
        Ok(r) if !CLOSING.load(Ordering::SeqCst) && accept(&r) => Some(r),
        Ok(_) => None,
        Err(e) => {
            log::debug!("single-window pipe: refused a malformed request ({e:?})");
            None
        }
    };
    let reply = if accepted.is_some() { READY } else { REFUSED };
    match write_all(h, ev, Some(stop), WRITE_MS, &[reply]) {
        Io::Done(_) => {}
        Io::Stopped => return Flow::Stop,
        _ => return Flow::Next,
    }
    let Some(request) = accepted else { return wait_for_close(h, ev, stop) };
    match read_byte(h, ev, Some(stop), COMMIT_MS) {
        Ok(COMMIT) => {}
        Err(Io::Stopped) => return Flow::Stop,
        // No commit within COMMIT_MS, a broken pipe, or a wrong byte: drop the request. The sender sees the pipe
        // broken and opens its own window.
        _ => {
            log::debug!("single-window pipe: no commit; request dropped");
            return Flow::Next;
        }
    }
    // Closing since `ready`: drop it rather than hand a file to a window that is going away; no `done`, so the sender
    // opens its own window.
    if CLOSING.load(Ordering::SeqCst) || stop.is_set() {
        return Flow::Stop;
    }
    commit(request);
    match write_all(h, ev, Some(stop), WRITE_MS, &[DONE]) {
        Io::Stopped => Flow::Stop,
        _ => wait_for_close(h, ev, stop),
    }
}

/// Waits up to `CLOSE_MS` for the sender to close its end, so it reads the last reply before `DisconnectNamedPipe`
/// would throw it away. A read that ends in a broken pipe is the close; anything else (a stray byte, a timeout) just
/// ends the wait.
fn wait_for_close(h: HANDLE, ev: &Event, stop: &Event) -> Flow {
    match read_byte(h, ev, Some(stop), CLOSE_MS) {
        Err(Io::Stopped) => Flow::Stop,
        _ => Flow::Next,
    }
}

// ---- the sending end ----

/// How `forward` ended (`claim_or_forward` says what the launch does with each).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardOutcome {
    /// The receiver acted on the request (or took it and was slow to say so): this launch exits.
    Delivered,
    /// No pipe of that name: no window receives, so this launch may claim the role.
    NoReceiver,
    /// The receiver said no, or went away before `commit`: nothing was opened; close and retry as receiver.
    Refused,
    /// Anything else, an identity check included: nothing more is tried, this launch opens its own window.
    Failed,
}

/// Sends `req` to the receiver at `name`, as `id`. Everything before `commit` must finish by
/// `deadline`; after `commit` the waits are fixed (`DONE_MS`).
pub fn forward(name: &str, id: &Identity, req: &Request, deadline: Instant) -> ForwardOutcome {
    let bytes = encode_request(req);
    // The receiver would refuse it unread; not worth a connection.
    if bytes.len() > MAX_MESSAGE {
        return ForwardOutcome::Failed;
    }
    let pipe = match open_client(name, deadline) {
        Ok(p) => p,
        Err(outcome) => return outcome,
    };
    let h = pipe.as_raw_handle() as HANDLE;
    // Checked before a single byte is written: the request holds a path, which a squatter must not learn.
    let (server, pid) = match check_server(h, id) {
        Ok(found) => found,
        Err(why) => {
            log::warn!("single-window pipe {name}: not forwarding, {why}");
            return ForwardOutcome::Failed;
        }
    };
    let Ok(ev) = Event::new() else { return ForwardOutcome::Failed };
    match write_all(h, &ev, None, ms_until(deadline), &bytes) {
        Io::Done(_) => {}
        Io::Failed(e) if is_broken(e) => return ForwardOutcome::Refused,
        _ => return ForwardOutcome::Failed,
    }
    // Up to the deadline: a receiver that is still starting (cold start, WARP in a VM) answers once `App::new` serves.
    match read_byte(h, &ev, None, ms_until(deadline)) {
        Ok(READY) => {}
        Ok(REFUSED) => return ForwardOutcome::Refused,
        Err(Io::Failed(e)) if is_broken(e) => return ForwardOutcome::Refused,
        _ => return ForwardOutcome::Failed,
    }
    // Windows lets only the foreground process bring a window to the front; pass that right to the checked receiver
    // (never ASFW_ANY), and stay alive until `done` so it is used while its giver lives. Refusal is harmless (the
    // taskbar button flashes instead).
    // SAFETY: plain call with a process id.
    unsafe { AllowSetForegroundWindow(pid) };
    #[cfg(test)]
    tests::pause_before_commit();
    match write_all(h, &ev, None, WRITE_MS, &[COMMIT]) {
        Io::Done(_) => {}
        // The receiver went away before it read `commit`, so it did nothing.
        Io::Failed(e) if is_broken(e) => return ForwardOutcome::Refused,
        _ => return ForwardOutcome::Failed,
    }
    let outcome = match read_byte(h, &ev, None, DONE_MS) {
        Ok(DONE) => ForwardOutcome::Delivered,
        // The receiver's commit wait (COMMIT_MS) is shorter than DONE_MS, so only a receiver that took the request
        // and then stalled gets here: the file shows late rather than twice.
        Err(Io::TimedOut) => ForwardOutcome::Delivered,
        // A broken pipe after `commit`: the receiver dropped the request. Not a retry as receiver (one exists).
        _ => ForwardOutcome::Failed,
    };
    // The process handle is held to the end so the checked pid cannot be reused under the exchange.
    drop(server);
    outcome
}

/// Milliseconds left until `deadline` (0 when past: the wait then only collects an operation that is already done).
fn ms_until(deadline: Instant) -> u32 {
    u32::try_from(deadline.saturating_duration_since(Instant::now()).as_millis()).unwrap_or(u32::MAX - 1)
}

/// Opens the client end, waiting for a busy pipe until `deadline`.
fn open_client(name: &str, deadline: Instant) -> Result<OwnedHandle, ForwardOutcome> {
    let name_w = wide(name);
    loop {
        // SAFETY: `name_w` is NUL-terminated. No security attributes, so the handle is not inheritable (a child
        // started by `relaunch_with_wgpu` must not hold it). READ_CONTROL for `GetSecurityInfo` in `check_server`.
        // Identification level: the server may learn who connected (`ImpersonateNamedPipeClient` then gives an
        // identification token) but cannot act as this user, so a fake server gains nothing from a connection.
        let h = unsafe {
            CreateFileW(
                name_w.as_ptr(),
                GENERIC_READ | GENERIC_WRITE | READ_CONTROL,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            // SAFETY: `h` is a valid handle that nothing else owns.
            return Ok(unsafe { OwnedHandle::from_raw_handle(h) });
        }
        // SAFETY: trivially safe.
        match unsafe { GetLastError() } {
            ERROR_FILE_NOT_FOUND => return Err(ForwardOutcome::NoReceiver),
            // The one instance serves another sender (or is between two). Wait for it with the time left; another
            // waiter may win the race, hence the loop.
            ERROR_PIPE_BUSY => {
                let left = ms_until(deadline);
                if left == 0 {
                    return Err(ForwardOutcome::Failed);
                }
                // SAFETY: `name_w` is NUL-terminated. Never 0, which would mean the pipe's default wait.
                if unsafe { WaitNamedPipeW(name_w.as_ptr(), left.max(1)) } == 0 {
                    // Timed out, or the name went away (the next CreateFileW says which). A short pause so an
                    // immediate failure cannot spin.
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            _ => return Err(ForwardOutcome::Failed),
        }
    }
}

/// The checks against a squatter (another process holding the name), on the opened client end `h`, before anything is written:
/// - the pipe's owner is this user's SID and its mandatory label is this process's level (`GetSecurityInfo`). Another
///   user cannot make your SID the owner of an object, and a lower-integrity process of yours cannot label above itself;
/// - the server process (`GetNamedPipeServerProcessId`, opened once and kept open by the caller so the pid cannot be
///   reused meanwhile) runs as the same user, logon, session and integrity level.
///
/// Returns the server's process handle and pid.
fn check_server(h: HANDLE, id: &Identity) -> Result<(OwnedHandle, u32), String> {
    let mut owner: PSID = null_mut();
    let mut sacl: *mut ACL = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: `h` was opened with READ_CONTROL; the out pointers are written on success and point into `sd`.
    let rc = unsafe { GetSecurityInfo(h, SE_KERNEL_OBJECT, OWNER_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION, &mut owner, null_mut(), null_mut(), &mut sacl, &mut sd) };
    if rc != ERROR_SUCCESS {
        return Err(format!("cannot read the pipe's owner and label (error {rc})"));
    }
    let owner_sid = sid_string(owner);
    let label = label_rid(sacl);
    // SAFETY: `sd` came from `GetSecurityInfo`; `owner` and `sacl` are not used after this.
    unsafe { LocalFree(sd) };
    match owner_sid {
        Ok(s) if s == id.sid => {}
        Ok(s) => return Err(format!("the pipe is owned by {s}")),
        Err(e) => return Err(format!("cannot read the pipe's owner: {e}")),
    }
    if label != Some(id.level) {
        return Err(format!("the pipe's integrity label is {label:x?}, not {:#x}", id.level));
    }
    let mut pid = 0u32;
    // SAFETY: `h` is a pipe client handle; `pid` is written on success.
    if unsafe { GetNamedPipeServerProcessId(h, &mut pid) } == 0 {
        return Err(format!("no server process id: {}", io::Error::last_os_error()));
    }
    // SAFETY: plain call; null on failure.
    let p = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if p.is_null() {
        return Err(format!("cannot open server process {pid}: {}", io::Error::last_os_error()));
    }
    // SAFETY: `p` is a valid handle that nothing else owns.
    let process = unsafe { OwnedHandle::from_raw_handle(p) };
    let mut token: HANDLE = null_mut();
    // SAFETY: PROCESS_QUERY_LIMITED_INFORMATION is enough for OpenProcessToken; `token` is written on success.
    if unsafe { OpenProcessToken(process.as_raw_handle() as HANDLE, TOKEN_QUERY, &mut token) } == 0 {
        return Err(format!("cannot open the token of server process {pid}: {}", io::Error::last_os_error()));
    }
    // SAFETY: `token` is a valid handle that nothing else owns.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let theirs = token_identity(&token).map_err(|e| format!("cannot read the identity of server process {pid}: {e}"))?;
    if !identity_matches(id, &theirs) {
        return Err(format!("server process {pid} runs as {theirs:?}, not {id:?}"));
    }
    Ok((process, pid))
}

/// The level of the first mandatory label ACE in `sacl`, if any. A pipe without an explicit label fails the check:
/// ours always has one.
fn label_rid(sacl: *mut ACL) -> Option<u32> {
    if sacl.is_null() {
        return None;
    }
    // SAFETY: `sacl` points into a security descriptor from `GetSecurityInfo`; `GetAce` checks the index against the
    // ACE count, and every ACE starts with an ACE_HEADER. A mandatory label ACE's SID starts at `SidStart`.
    unsafe {
        for i in 0..u32::from((*sacl).AceCount) {
            let mut ace = null_mut();
            if GetAce(sacl, i, &mut ace) == 0 || ace.is_null() {
                return None;
            }
            if (*ace.cast::<ACE_HEADER>()).AceType == MANDATORY_LABEL_ACE {
                let label = ace.cast::<SYSTEM_MANDATORY_LABEL_ACE>();
                return sid_rid((&raw mut (*label).SidStart).cast());
            }
        }
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::PathBuf;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_IO_PENDING, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT};

    /// `shutdown` stops every server of the process and sets `CLOSING`, so the test that calls it runs alone (write
    /// lock) while the other serving tests run side by side (read lock).
    static SERIAL: RwLock<()> = RwLock::new(());

    pub(crate) fn shared() -> RwLockReadGuard<'static, ()> {
        SERIAL.read().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn exclusive() -> RwLockWriteGuard<'static, ()> {
        SERIAL.write().unwrap_or_else(|e| e.into_inner())
    }

    /// A test-only name, never one a real mdread would use (those start with `mdread-S-`).
    pub(crate) fn test_name() -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        format!(r"\\.\pipe\mdread-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
    }

    fn id() -> Identity {
        own_identity().expect("own_identity")
    }

    fn req(path: &str) -> Request {
        Request { path: PathBuf::from(path), anchor: Some("intro".into()), find: None }
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    /// A served test pipe that records commits; it stops on drop, also when the test fails, so no thread outlives it.
    pub(crate) struct Serving {
        server: Server,
        pub(crate) commits: mpsc::Receiver<Request>,
        committed: Arc<AtomicUsize>,
        accepted: Arc<AtomicUsize>,
    }

    impl Serving {
        pub(crate) fn start(name: &str, accept: bool) -> Serving {
            let pipe = create_pipe(name, &id()).expect("create_pipe");
            Serving::serve(pipe, accept)
        }

        pub(crate) fn serve(pipe: PipeHandle, accept: bool) -> Serving {
            Serving::serve_with(pipe, accept, Duration::ZERO)
        }

        /// As `serve`, with a `commit` that stalls for `commit_delay` after recording the commit.
        fn serve_with(pipe: PipeHandle, accept: bool, commit_delay: Duration) -> Serving {
            let (tx, commits) = mpsc::channel();
            let tx = Mutex::new(tx);
            let committed = Arc::new(AtomicUsize::new(0));
            let accepted = Arc::new(AtomicUsize::new(0));
            let (c, a) = (committed.clone(), accepted.clone());
            let server = pipe.serve(
                Arc::new(move |_r: &Request| {
                    a.fetch_add(1, Ordering::SeqCst);
                    accept
                }),
                Arc::new(move |r: Request| {
                    c.fetch_add(1, Ordering::SeqCst);
                    let _ = tx.lock().unwrap_or_else(|e| e.into_inner()).send(r);
                    std::thread::sleep(commit_delay);
                }),
            );
            Serving { server, commits, committed, accepted }
        }

        fn commits(&self) -> usize {
            self.committed.load(Ordering::SeqCst)
        }

        /// The server, to stop it from another thread while a test runs.
        pub(crate) fn server(&self) -> &Server {
            &self.server
        }
    }

    impl Drop for Serving {
        fn drop(&mut self) {
            self.server.stop_serving();
            if !wait_finished(&self.server) {
                // A panic while already unwinding would abort the whole test binary; the first failure is the one to see.
                if std::thread::panicking() {
                    eprintln!("server did not finish within 3 s (while unwinding from an earlier failure)");
                } else {
                    panic!("server did not finish within 3 s");
                }
            }
        }
    }

    /// Waits up to 3 s for `is_finished`; whether it got there.
    pub(crate) fn wait_finished(server: &Server) -> bool {
        let until = Instant::now() + Duration::from_secs(3);
        while !server.is_finished() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        server.is_finished()
    }

    fn wide_z(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// A hand-driven client, independent of the code under test, for clients that misbehave. Overlapped with timeouts
    /// so that a broken server fails the test instead of hanging it.
    struct Raw {
        h: OwnedHandle,
        ev: OwnedHandle,
    }

    impl Raw {
        fn connect(name: &str) -> Raw {
            Raw::connect_with(name, 0)
        }

        fn connect_with(name: &str, extra_access: u32) -> Raw {
            let w = wide_z(name);
            let until = Instant::now() + Duration::from_secs(3);
            loop {
                // SAFETY: `w` is NUL-terminated; no security attributes, no template.
                let h = unsafe { CreateFileW(w.as_ptr(), GENERIC_READ | GENERIC_WRITE | extra_access, 0, null(), OPEN_EXISTING, FILE_FLAG_OVERLAPPED, null_mut()) };
                if h != INVALID_HANDLE_VALUE {
                    // SAFETY: plain manual-reset event.
                    let ev = unsafe { CreateEventW(null(), 1, 0, null()) };
                    assert!(!ev.is_null());
                    // SAFETY: both handles are valid and owned from here on.
                    return unsafe { Raw { h: OwnedHandle::from_raw_handle(h), ev: OwnedHandle::from_raw_handle(ev) } };
                }
                // SAFETY: trivially safe.
                let e = unsafe { GetLastError() };
                assert!(e == ERROR_PIPE_BUSY && Instant::now() < until, "raw connect failed: {e}");
                // SAFETY: `w` is NUL-terminated.
                unsafe { WaitNamedPipeW(w.as_ptr(), 500) };
            }
        }

        fn io(&self, timeout_ms: u32, start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32) -> Result<u32, u32> {
            let h = self.h.as_raw_handle() as HANDLE;
            // SAFETY: zeroed OVERLAPPED with our event; it stays on this frame until the operation is complete or cancelled.
            unsafe {
                let mut ov: OVERLAPPED = std::mem::zeroed();
                ov.hEvent = self.ev.as_raw_handle() as HANDLE;
                if start(h, &mut ov) == 0 && GetLastError() != ERROR_IO_PENDING {
                    return Err(GetLastError());
                }
                let mut n = 0;
                if WaitForSingleObject(ov.hEvent, timeout_ms) != WAIT_OBJECT_0 {
                    CancelIoEx(h, &ov);
                    GetOverlappedResultEx(h, &ov, &mut n, INFINITE, 0);
                    return Err(WAIT_TIMEOUT);
                }
                if GetOverlappedResultEx(h, &ov, &mut n, 0, 0) == 0 { Err(GetLastError()) } else { Ok(n) }
            }
        }

        fn write(&self, bytes: &[u8]) -> bool {
            // SAFETY: `bytes` outlives the operation (see `io`).
            self.io(2000, |h, ov| unsafe { WriteFile(h, bytes.as_ptr(), bytes.len() as u32, null_mut(), ov) }) == Ok(bytes.len() as u32)
        }

        /// One message (up to 16 bytes), or the Win32 error (`WAIT_TIMEOUT` when nothing came in time).
        fn read(&self, timeout_ms: u32) -> Result<Vec<u8>, u32> {
            let mut buf = [0u8; 16];
            let p = buf.as_mut_ptr();
            // SAFETY: `buf` outlives the operation (see `io`).
            let n = self.io(timeout_ms, |h, ov| unsafe { ReadFile(h, p, 16, null_mut(), ov) })?;
            Ok(buf[..n as usize].to_vec())
        }
    }

    const READY_BYTE: u8 = b'y';
    const REFUSED_BYTE: u8 = b'n';

    thread_local! {
        /// How long `forward` on this thread pauses between `ready` and writing `commit`; zero (the default) except in
        /// a test that must order a server's disconnect before the commit write. Thread-local, so tests running side
        /// by side are not affected.
        static PAUSE_BEFORE_COMMIT: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
    }

    pub(super) fn pause_before_commit() {
        let d = PAUSE_BEFORE_COMMIT.with(std::cell::Cell::get);
        if !d.is_zero() {
            std::thread::sleep(d);
        }
    }

    /// Resets `CLOSING` when dropped, also when the test fails, so a failure cannot leave later tests refusing.
    struct ClosingReset;

    impl Drop for ClosingReset {
        fn drop(&mut self) {
            reset_closing();
        }
    }

    /// Plays a receiver that stops early, on a real pipe of `name` (real descriptor, this process, so `forward`'s
    /// checks pass): it reads the request, writes `ready`, reads `commit` if `read_commit`, then disconnects at once,
    /// without `done`. Every wait is bounded; the thread reports what went wrong on its side.
    fn rogue_server(name: &str, read_commit: bool) -> std::thread::JoinHandle<Result<(), String>> {
        let handle = take_raw(create_pipe(name, &id()).expect("create")).expect("raw handle");
        std::thread::spawn(move || {
            let h = handle.as_raw_handle() as HANDLE;
            let ev = Event::new().map_err(|e| e.to_string())?;
            // SAFETY: `h` is the server end; the OVERLAPPED comes from `overlapped`.
            match overlapped(h, &ev, None, 3000, |ov| unsafe { ConnectNamedPipe(h, ov) }) {
                Io::Done(_) | Io::Failed(ERROR_PIPE_CONNECTED) => {}
                other => return Err(format!("connect: {other:?}")),
            }
            let mut buf = vec![0u8; MAX_MESSAGE];
            match read_into(h, &ev, None, 2000, &mut buf) {
                Io::Done(_) => {}
                other => return Err(format!("request: {other:?}")),
            }
            match write_all(h, &ev, None, 1000, &[READY]) {
                Io::Done(_) => {}
                other => return Err(format!("ready: {other:?}")),
            }
            if read_commit {
                match read_byte(h, &ev, None, 2000) {
                    Ok(COMMIT) => {}
                    other => return Err(format!("commit: {other:?}")),
                }
            }
            // SAFETY: valid pipe handle.
            unsafe { DisconnectNamedPipe(h) };
            drop(handle);
            Ok(())
        })
    }

    #[test]
    fn takes_part_only_at_medium_and_above_outside_app_containers() {
        assert!(!takes_part(false, 0x1000), "low");
        assert!(takes_part(false, 0x2000), "medium");
        assert!(takes_part(false, 0x2100), "medium plus");
        assert!(takes_part(false, 0x3000), "high");
        assert!(!takes_part(true, 0x2000), "AppContainer");
        assert!(!takes_part(true, 0x1000), "AppContainer, low");
    }

    #[test]
    fn closing_live_server_refuses_without_asking_accept() {
        let _g = exclusive();
        let name = test_name();
        let s = Serving::start(&name, true);
        let _reset = ClosingReset;
        set_closing();
        assert_eq!(forward(&name, &id(), &req(r"C:\a.md"), soon()), ForwardOutcome::Refused);
        assert_eq!(s.accepted.load(Ordering::SeqCst), 0, "accept asked while closing");
        assert_eq!(s.commits(), 0);
    }

    #[test]
    fn no_done_within_two_seconds_counts_as_delivered() {
        let _g = shared();
        let name = test_name();
        let s = Serving::serve_with(create_pipe(&name, &id()).unwrap(), true, Duration::from_millis(2500));
        let started = Instant::now();
        assert_eq!(forward(&name, &id(), &req(r"C:\slow.md"), soon()), ForwardOutcome::Delivered);
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(1900) && took < Duration::from_millis(2500), "{took:?}: forward did not stop at DONE_MS");
        assert_eq!(s.commits.recv_timeout(Duration::from_secs(1)).unwrap().path, PathBuf::from(r"C:\slow.md"));
        assert_eq!(s.commits(), 1);
    }

    #[test]
    fn receiver_gone_after_ready_is_refused() {
        let _g = shared();
        let name = test_name();
        let rogue = rogue_server(&name, false);
        // Orders the rogue's disconnect before the commit write, which is what this case is about.
        PAUSE_BEFORE_COMMIT.with(|p| p.set(Duration::from_millis(300)));
        let outcome = forward(&name, &id(), &req(r"C:\a.md"), soon());
        PAUSE_BEFORE_COMMIT.with(|p| p.set(Duration::ZERO));
        assert_eq!(rogue.join().unwrap(), Ok(()));
        assert_eq!(outcome, ForwardOutcome::Refused);
    }

    #[test]
    fn receiver_gone_after_commit_is_failed() {
        let _g = shared();
        let name = test_name();
        let rogue = rogue_server(&name, true);
        let outcome = forward(&name, &id(), &req(r"C:\a.md"), soon());
        assert_eq!(rogue.join().unwrap(), Ok(()));
        assert_eq!(outcome, ForwardOutcome::Failed);
    }

    #[test]
    fn own_identity_looks_right() {
        let me = id();
        assert!(me.sid.starts_with("S-1-5-"), "{}", me.sid);
        assert!(me.level >= 0x2000, "{:x}", me.level);
        assert_ne!(me.logon, 0);
        let name = name_for_this_exe().unwrap();
        assert!(name.starts_with(&format!(r"\\.\pipe\mdread-{}-{:x}-{:x}-", me.sid, me.logon, me.level)), "{name}");
    }

    #[test]
    fn exchange_delivers_the_request() {
        let _g = shared();
        let name = test_name();
        let s = Serving::start(&name, true);
        let sent = Request { path: PathBuf::from(r"C:\x y\日本.md"), anchor: Some("intro".into()), find: Some("a b".into()) };
        assert_eq!(forward(&name, &id(), &sent, soon()), ForwardOutcome::Delivered);
        assert_eq!(s.commits.recv_timeout(Duration::from_secs(2)).unwrap(), sent);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(s.commits(), 1);
    }

    #[test]
    fn request_sent_before_serving_waits_in_the_pipe() {
        // A sender may connect before `App::new` starts the server; its request waits in the pipe.
        let _g = shared();
        let name = test_name();
        let pipe = create_pipe(&name, &id()).unwrap();
        let n = name.clone();
        let sender = std::thread::spawn(move || forward(&n, &id(), &req(r"C:\early.md"), soon()));
        std::thread::sleep(Duration::from_millis(300));
        let s = Serving::serve(pipe, true);
        assert_eq!(sender.join().unwrap(), ForwardOutcome::Delivered);
        assert_eq!(s.commits.recv_timeout(Duration::from_secs(2)).unwrap().path, PathBuf::from(r"C:\early.md"));
    }

    #[test]
    fn refused_request_is_not_committed() {
        let _g = shared();
        let name = test_name();
        let s = Serving::start(&name, false);
        assert_eq!(forward(&name, &id(), &req(r"C:\a.md"), soon()), ForwardOutcome::Refused);
        assert_eq!(s.accepted.load(Ordering::SeqCst), 1);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(s.commits(), 0);
    }

    #[test]
    fn stalled_client_does_not_block_the_next() {
        let _g = shared();
        let name = test_name();
        let s = Serving::start(&name, true);
        let stalled = Raw::connect(&name);
        let started = Instant::now();
        assert_eq!(forward(&name, &id(), &req(r"C:\b.md"), started + Duration::from_secs(4)), ForwardOutcome::Delivered);
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
        assert!(started.elapsed() >= Duration::from_millis(1500), "the stalled client was dropped too early: {:?}", started.elapsed());
        assert_eq!(s.commits(), 1);
        drop(stalled);
    }

    #[test]
    fn client_that_never_commits_is_dropped() {
        let _g = shared();
        let name = test_name();
        let s = Serving::start(&name, true);
        let raw = Raw::connect(&name);
        assert!(raw.write(&encode_request(&req(r"C:\c.md"))));
        assert_eq!(raw.read(2000), Ok(vec![READY_BYTE]));
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(s.commits(), 0, "committed without a commit byte");
        // The server gave up on it (COMMIT_MS is 1 s): its next read fails as disconnected, not as a timeout.
        let after = raw.read(500);
        assert!(matches!(after, Err(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED)), "{after:?}");
        // And the instance is free again: a new sender gets through at once, not after a further wait.
        let started = Instant::now();
        assert_eq!(forward(&name, &id(), &req(r"C:\d.md"), started + Duration::from_millis(1500)), ForwardOutcome::Delivered);
        assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
        assert_eq!(s.commits.recv_timeout(Duration::from_secs(2)).unwrap().path, PathBuf::from(r"C:\d.md"));
        assert_eq!(s.commits(), 1);
    }

    #[test]
    fn no_server_means_no_receiver() {
        let name = test_name();
        assert_eq!(forward(&name, &id(), &req(r"C:\a.md"), soon()), ForwardOutcome::NoReceiver);
    }

    #[test]
    fn second_create_is_taken() {
        let _g = shared();
        let name = test_name();
        let first = create_pipe(&name, &id()).unwrap();
        assert!(matches!(create_pipe(&name, &id()), Err(CreateError::Taken)));
        drop(first);
        // Dropping an unserved handle frees the name.
        let again = create_pipe(&name, &id()).expect("name free after drop");
        drop(again);
    }

    #[test]
    fn five_senders_at_once_all_deliver_in_some_order() {
        let _g = shared();
        let name = test_name();
        let s = Serving::start(&name, true);
        let senders: Vec<_> = (0..5)
            .map(|i| {
                let n = name.clone();
                std::thread::spawn(move || forward(&n, &id(), &req(&format!(r"C:\five\{i}.md")), Instant::now() + Duration::from_secs(10)))
            })
            .collect();
        for t in senders {
            assert_eq!(t.join().unwrap(), ForwardOutcome::Delivered);
        }
        let mut got: Vec<PathBuf> = (0..5).map(|_| s.commits.recv_timeout(Duration::from_secs(2)).unwrap().path).collect();
        got.sort();
        let want: Vec<PathBuf> = (0..5).map(|i| PathBuf::from(format!(r"C:\five\{i}.md"))).collect();
        assert_eq!(got, want);
        assert_eq!(s.commits(), 5);
    }

    #[test]
    fn stop_serving_frees_the_name() {
        let _g = shared();
        let name = test_name();
        let s = Serving::start(&name, true);
        assert_eq!(forward(&name, &id(), &req(r"C:\a.md"), soon()), ForwardOutcome::Delivered);
        let started = Instant::now();
        s.server.stop_serving();
        let again = create_pipe(&name, &id());
        let took = started.elapsed();
        assert!(again.is_ok(), "name not free after stop_serving: {:?}", again.err());
        assert!(took < Duration::from_millis(400), "{took:?}");
        assert!(s.server.is_finished());
        // Stopping sets no process state: the name can be served again.
        let s2 = Serving::serve(again.unwrap(), true);
        assert_eq!(forward(&name, &id(), &req(r"C:\b.md"), soon()), ForwardOutcome::Delivered);
        assert_eq!(s2.commits(), 1);
        // stop_serving is idempotent.
        s.server.stop_serving();
    }

    #[test]
    fn shutdown_makes_the_server_refuse() {
        let _g = exclusive();
        let name = test_name();
        let unserved_name = test_name();
        let s = Serving::start(&name, true);
        let unserved = create_pipe(&unserved_name, &id()).unwrap();
        shutdown();
        shutdown(); // idempotent
        let outcome = forward(&name, &id(), &req(r"C:\a.md"), Instant::now() + Duration::from_secs(2));
        let while_closing = create_pipe(&unserved_name, &id());
        reset_closing();
        assert!(matches!(outcome, ForwardOutcome::Refused | ForwardOutcome::NoReceiver), "{outcome:?}");
        assert_eq!(s.commits(), 0);
        assert!(s.server.is_finished(), "shutdown did not stop the server");
        assert!(matches!(while_closing, Err(CreateError::Other(_))), "create_pipe must fail while closing");
        // The never-served handle was closed by shutdown, so the name is free; serving that handle now gives a
        // finished server, not a thread.
        let again = create_pipe(&unserved_name, &id()).expect("unserved name freed by shutdown");
        let late = unserved.serve(Arc::new(|_: &Request| true), Arc::new(|_| {}));
        assert!(late.is_finished());
        drop(again);
        // After the reset the process can serve again.
        let s2 = Serving::start(&name, true);
        assert_eq!(forward(&name, &id(), &req(r"C:\b.md"), soon()), ForwardOutcome::Delivered, "works again after reset");
        assert_eq!(s2.commits(), 1);
    }

    #[test]
    fn malformed_bytes_get_refused() {
        let _g = shared();
        let name = test_name();
        let s = Serving::start(&name, true);
        let mut odd = encode_request(&req(r"C:\a.md"));
        odd.push(0);
        let bad_version: Vec<u8> = "mdread/9\0open\0C:\\a.md\0\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let relative: Vec<u8> = "mdread/1\0open\0a.md\0\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        for garbage in [vec![0xff, 0x00, 0x13], odd, bad_version, relative, vec![]] {
            let raw = Raw::connect(&name);
            assert!(raw.write(&garbage));
            assert_eq!(raw.read(2000), Ok(vec![REFUSED_BYTE]), "{garbage:?}");
        }
        assert_eq!(s.accepted.load(Ordering::SeqCst), 0, "accept must not see undecodable requests");
        assert_eq!(forward(&name, &id(), &req(r"C:\ok.md"), soon()), ForwardOutcome::Delivered);
        assert_eq!(s.commits(), 1);
    }

    #[test]
    fn oversized_message_is_refused_not_buffered() {
        let _g = shared();
        let name = test_name();
        let s = Serving::start(&name, true);
        let raw = Raw::connect(&name);
        // The write of a message over the cap may not complete until the server lets go, so it runs on its own thread.
        let writer = std::thread::spawn(move || {
            raw.write(&vec![b'a'; MAX_MESSAGE + 2]);
            raw.read(2000)
        });
        let reply = writer.join().unwrap();
        assert!(matches!(reply.as_deref(), Err(_) | Ok([REFUSED_BYTE])), "{reply:?}");
        assert_eq!(s.accepted.load(Ordering::SeqCst), 0);
        // A sender with too much to say fails before connecting.
        let huge = Request { path: PathBuf::from(format!(r"C:\{}", "a".repeat(MAX_MESSAGE))), anchor: None, find: None };
        assert_eq!(forward(&name, &id(), &huge, soon()), ForwardOutcome::Failed);
        assert_eq!(forward(&name, &id(), &req(r"C:\ok.md"), soon()), ForwardOutcome::Delivered);
        assert_eq!(s.commits(), 1);
    }

    #[test]
    fn pipe_with_the_wrong_label_or_none_gets_nothing() {
        // A same-user pipe labelled low (as a browser sandbox would create it), or with no explicit label: the sender
        // must refuse it before writing, so the server never sees a request.
        let _g = shared();
        let me = id();
        for sacl in ["S:(ML;;NWNR;;;LW)", ""] {
            let name = test_name();
            let pipe = create_pipe_sddl(&name, &format!("O:{sid}D:P(A;;GA;;;{sid}){sacl}", sid = me.sid)).expect("create test pipe");
            let s = Serving::serve(pipe, true);
            assert_eq!(forward(&name, &me, &req(r"C:\secret.md"), soon()), ForwardOutcome::Failed, "{sacl:?}");
            assert_eq!(s.accepted.load(Ordering::SeqCst), 0, "a request reached a pipe with label {sacl:?}");
        }
    }

    #[test]
    fn identity_of_a_squatter_is_rejected() {
        let own = Identity { sid: "S-1-5-21-1-2-3-1001".into(), logon: 0x1a2b3, level: 0x2000, session: 1 };
        assert!(identity_matches(&own, &own.clone()));
        assert!(!identity_matches(&own, &Identity { sid: "S-1-5-21-1-2-3-1002".into(), ..own.clone() }));
        assert!(!identity_matches(&own, &Identity { logon: 0x1a2b4, ..own.clone() }));
        assert!(!identity_matches(&own, &Identity { level: 0x3000, ..own.clone() }));
        assert!(!identity_matches(&own, &Identity { session: 2, ..own.clone() }));
    }

    #[test]
    fn real_descriptor_reads_back_as_own_owner_and_level() {
        // `forward` succeeding already implies it; this states it on its own, through the same check.
        let _g = shared();
        let name = test_name();
        let _pipe = create_pipe(&name, &id()).unwrap();
        let raw = Raw::connect_with(&name, READ_CONTROL);
        let checked = check_server(raw.h.as_raw_handle() as HANDLE, &id());
        assert!(checked.is_ok(), "{:?}", checked.err());
        assert_eq!(checked.unwrap().1, std::process::id());
    }
}
