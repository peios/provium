//! A minimal NBD server, so provium can stand between a guest and its
//! disk.
//!
//! Why this exists: `disk:fault_inject` is host-side only. It affects
//! sector access made *by the test*, not the guest's own I/O, so
//! nothing in provium can presently make the guest see a disk
//! misbehave — which leaves loregd's durability claims untestable end
//! to end (PEI-1104).
//!
//! The fault that matters for a write-ahead log is not `EIO`. It is
//! losing exactly the writes that were never fsynced. So this server
//! holds writes in memory and commits them to the backing file only on
//! `FLUSH`. [`MediatedDisk::power_cut`] then drops the overlay, and
//! what survives is precisely what the guest flushed.
//!
//! That is the part killing QEMU cannot give you: unflushed data
//! written through a normal `-drive` sits in the *host page cache*,
//! which outlives the process, so a SIGKILL "power cut" reports
//! durability that was never demonstrated. Holding the cache here, in
//! our own address space, is what makes dropping it exact.
//!
//! # Scope
//!
//! This is the PEI-1104 spike: one export, one connection at a time,
//! no fault policy, no Lua surface. It exists to establish whether
//! QEMU's block layer gives honest semantics through an NBD server
//! before anything gets built on top of it.
//!
//! # Protocol note
//!
//! The constants below were confirmed against a real QEMU client's
//! bytes rather than taken from the specification alone, and the two
//! disagree in a way that matters: a modern QEMU offers
//! `NBD_OPT_EXTENDED_HEADERS`, and if the server accepts it the entire
//! transmission phase changes shape — a different request magic
//! (`0x21e41c71`), 32-byte headers with a 64-bit length, and reads
//! answered with structured `OFFSET_DATA` chunks instead of simple
//! replies.
//!
//! We answer `NBD_REP_ERR_UNSUP` to that option, and to
//! `NBD_OPT_STRUCTURED_REPLY` and `NBD_OPT_SET_META_CONTEXT` with it,
//! which drops the client back to the classic 28-byte request and
//! 16-byte simple reply. Far less to implement, and none of it is
//! needed to answer the question the spike is asking.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Sector size, fixed at 512 bytes to match the rest of provium's disk
/// surface (`disk:read_sectors` and friends).
const SECTOR: usize = 512;

/// Ceiling on unflushed data held in memory.
///
/// A guest that writes without ever flushing would otherwise grow the
/// overlay until the host suffers. On crossing it the overlay is
/// committed and cleared, which is what a real write cache does under
/// pressure — but it does mean a power cut after that point loses less
/// than a true one would. A test wanting exact semantics must stay
/// under the cap; at 64 MiB that is every realistic registry workload.
const OVERLAY_CAP_BYTES: usize = 64 * 1024 * 1024;

// Handshake magics.
const NBD_MAGIC: u64 = 0x4e42_444d_4147_4943; // "NBDMAGIC"
const IHAVEOPT: u64 = 0x4948_4156_454f_5054; // "IHAVEOPT"
const REP_MAGIC: u64 = 0x0003_e889_0455_65a9;

// Transmission magics.
const REQUEST_MAGIC: u32 = 0x2560_9513;
const SIMPLE_REPLY_MAGIC: u32 = 0x6744_6698;

// Handshake flags (server -> client, 16 bits).
const FLAG_FIXED_NEWSTYLE: u16 = 1 << 0;
const FLAG_NO_ZEROES: u16 = 1 << 1;

// Client flags (client -> server, 32 bits).
const CLIENT_FLAG_NO_ZEROES: u32 = 1 << 1;

// Transmission flags, advertised in the export info.
const FLAG_HAS_FLAGS: u16 = 1 << 0;
const FLAG_SEND_FLUSH: u16 = 1 << 2;
const FLAG_SEND_FUA: u16 = 1 << 3;

// Options we act on. Everything else is refused.
const OPT_EXPORT_NAME: u32 = 1;
const OPT_ABORT: u32 = 2;
const OPT_INFO: u32 = 6;
const OPT_GO: u32 = 7;

// Option reply types.
const REP_ACK: u32 = 1;
const REP_INFO: u32 = 3;
const REP_ERR_UNSUP: u32 = 0x8000_0001;

/// `NBD_INFO_EXPORT` — the only information type we volunteer.
const INFO_EXPORT: u16 = 0;

// Commands.
const CMD_READ: u16 = 0;
const CMD_WRITE: u16 = 1;
const CMD_DISC: u16 = 2;
const CMD_FLUSH: u16 = 3;

/// `NBD_CMD_FLAG_FUA` — this write must be durable before it is
/// acknowledged.
const CMD_FLAG_FUA: u16 = 1 << 0;

// Errors are errno values on the wire.
const NBD_EIO: u32 = 5;
const NBD_EINVAL: u32 = 22;

/// The backing image plus whatever the guest has written but not yet
/// flushed.
struct DiskState {
    file: std::fs::File,
    size: u64,
    /// Sector index -> that sector's contents, for sectors written
    /// since the last flush. A `BTreeMap` so committing walks the file
    /// in ascending offset order rather than at random.
    overlay: BTreeMap<u64, Vec<u8>>,
    /// Count of flushes served, so a test can assert the guest really
    /// did flush rather than inferring it from data that survived.
    flushes: u64,
}

impl DiskState {
    /// Read `len` bytes at `offset`, the backing file overlaid with any
    /// unflushed sectors.
    ///
    /// The guest must see its own writes back even before they are
    /// durable — a cache that loses reads is not a cache, it is a
    /// fault, and not the one being modelled.
    fn read(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let end = offset
            .checked_add(len as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "read overflows"))?;
        if end > self.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "read past end of disk",
            ));
        }
        let mut buf = vec![0u8; len];
        self.file.read_exact_at(&mut buf, offset)?;

        for sector in (offset / SECTOR as u64)..=((end - 1) / SECTOR as u64) {
            let Some(held) = self.overlay.get(&sector) else {
                continue;
            };
            let sector_start = sector * SECTOR as u64;
            let from = offset.max(sector_start);
            let to = end.min(sector_start + SECTOR as u64);
            buf[(from - offset) as usize..(to - offset) as usize]
                .copy_from_slice(&held[(from - sector_start) as usize..(to - sector_start) as usize]);
        }
        Ok(buf)
    }

    /// Take `data` into the overlay at `offset`. Nothing reaches the
    /// backing file until [`Self::flush`].
    fn write(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "write overflows"))?;
        if end > self.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "write past end of disk",
            ));
        }

        for sector in (offset / SECTOR as u64)..=((end - 1) / SECTOR as u64) {
            let sector_start = sector * SECTOR as u64;
            // A partial write has to start from what the sector holds
            // now, or committing it would zero the bytes either side of
            // the written range.
            if !self.overlay.contains_key(&sector) {
                let mut base = vec![0u8; SECTOR];
                self.file.read_exact_at(&mut base, sector_start)?;
                self.overlay.insert(sector, base);
            }
            // Unwrap: inserted immediately above when absent.
            let held = self.overlay.get_mut(&sector).unwrap();
            let from = offset.max(sector_start);
            let to = end.min(sector_start + SECTOR as u64);
            held[(from - sector_start) as usize..(to - sector_start) as usize]
                .copy_from_slice(&data[(from - offset) as usize..(to - offset) as usize]);
        }

        if self.overlay.len() * SECTOR > OVERLAY_CAP_BYTES {
            self.flush()?;
        }
        Ok(())
    }

    /// Commit the overlay and make it durable.
    fn flush(&mut self) -> io::Result<()> {
        for (sector, data) in std::mem::take(&mut self.overlay) {
            self.file.write_all_at(&data, sector * SECTOR as u64)?;
        }
        self.file.sync_data()?;
        self.flushes += 1;
        Ok(())
    }
}

/// State shared between the serving thread and the handle.
struct Shared {
    state: Mutex<DiskState>,
    shutdown: AtomicBool,
    /// The connection being served, if any, so dropping the handle can
    /// break a thread blocked reading from it.
    current: Mutex<Option<UnixStream>>,
    /// Clients that have connected. Lets a test assert a guest actually
    /// opened the export, rather than infer it from the absence of a
    /// crash.
    connections: AtomicU64,
}

/// A running NBD server for one disk image.
///
/// Dropping it stops the server and removes the socket.
pub struct MediatedDisk {
    shared: Arc<Shared>,
    socket_path: PathBuf,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MediatedDisk {
    /// Serve `image` on a fresh unix socket at `socket_path`.
    ///
    /// `socket_path` must be short: a unix socket address is capped at
    /// 108 bytes by `sockaddr_un`, and the failure — `UNIX socket path
    /// … is too long` — arrives at bind time, long after the path was
    /// chosen. Callers should prefer a `tempfile::TempDir` under `/tmp`
    /// over anywhere nested.
    pub fn start(image: &Path, socket_path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(image)?;
        let size = file.metadata()?.len();

        // A stale socket from a crashed run would make bind fail with
        // EADDRINUSE and nothing to say about why.
        if socket_path.exists() {
            std::fs::remove_file(socket_path)?;
        }
        let listener = UnixListener::bind(socket_path)?;
        // Non-blocking accept so the loop can notice a shutdown. The
        // *connection* stays blocking — a timeout mid-header would risk
        // consuming half a request and losing it.
        listener.set_nonblocking(true)?;

        let shared = Arc::new(Shared {
            state: Mutex::new(DiskState {
                file,
                size,
                overlay: BTreeMap::new(),
                flushes: 0,
            }),
            shutdown: AtomicBool::new(false),
            current: Mutex::new(None),
            connections: AtomicU64::new(0),
        });

        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("provium-nbd".to_owned())
            .spawn(move || accept_loop(listener, thread_shared))?;

        Ok(Self {
            shared,
            socket_path: socket_path.to_owned(),
            thread: Some(thread),
        })
    }

    /// Discard every write the guest has not flushed.
    ///
    /// This models the power going out, not the disk failing: flushed
    /// data stays, unflushed data is gone. The guest's *own* page cache
    /// is untouched and still holds what it wrote, so a test has to
    /// reboot — or at minimum drop the guest's caches — before reading
    /// back, or it is reading the guest's memory rather than the disk.
    pub fn power_cut(&self) {
        let mut state = self.shared.state.lock().unwrap();
        state.overlay.clear();
    }

    /// Number of `FLUSH` commands served so far.
    pub fn flushes(&self) -> u64 {
        self.shared.state.lock().unwrap().flushes
    }

    /// Number of clients that have connected since the server started.
    pub fn connections(&self) -> u64 {
        self.shared.connections.load(Ordering::SeqCst)
    }

    /// Sectors currently written but not flushed.
    pub fn unflushed_sectors(&self) -> usize {
        self.shared.state.lock().unwrap().overlay.len()
    }

    /// The socket this server listens on.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// The export as QEMU addresses it — suitable for a `-drive
    /// file=…` or `qemu-img` argument.
    pub fn qemu_url(&self) -> String {
        format!("nbd+unix:///?socket={}", self.socket_path.display())
    }
}

impl std::fmt::Debug for MediatedDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because the state behind the mutex is a whole
        // disk's worth of held sectors, which nobody wants printed.
        f.debug_struct("MediatedDisk")
            .field("socket_path", &self.socket_path)
            .finish_non_exhaustive()
    }
}

impl Drop for MediatedDisk {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        // Break a thread blocked reading the current connection.
        if let Some(stream) = self.shared.current.lock().unwrap().as_ref() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

fn accept_loop(listener: UnixListener, shared: Arc<Shared>) {
    while !shared.shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                shared.connections.fetch_add(1, Ordering::SeqCst);
                if let Ok(clone) = stream.try_clone() {
                    *shared.current.lock().unwrap() = Some(clone);
                }
                // A failed session is not a failed server: QEMU may
                // reconnect, and the next connection deserves a try.
                let _ = serve(stream, &shared);
                *shared.current.lock().unwrap() = None;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

/// Negotiate, then serve commands until the client disconnects.
fn serve(mut stream: UnixStream, shared: &Arc<Shared>) -> io::Result<()> {
    let size = shared.state.lock().unwrap().size;
    if !handshake(&mut stream, size)? {
        return Ok(());
    }
    transmission(&mut stream, shared)
}

/// Fixed-newstyle negotiation. Returns whether to proceed to the
/// transmission phase.
fn handshake(stream: &mut UnixStream, size: u64) -> io::Result<bool> {
    let mut hello = Vec::with_capacity(18);
    hello.extend_from_slice(&NBD_MAGIC.to_be_bytes());
    hello.extend_from_slice(&IHAVEOPT.to_be_bytes());
    hello.extend_from_slice(&(FLAG_FIXED_NEWSTYLE | FLAG_NO_ZEROES).to_be_bytes());
    stream.write_all(&hello)?;

    let client_flags = read_u32(stream)?;
    let no_zeroes = client_flags & CLIENT_FLAG_NO_ZEROES != 0;

    loop {
        let magic = read_u64(stream)?;
        if magic != IHAVEOPT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("option magic {magic:#x}"),
            ));
        }
        let option = read_u32(stream)?;
        let length = read_u32(stream)?;
        let mut data = vec![0u8; length as usize];
        stream.read_exact(&mut data)?;

        match option {
            // The classic path: no reply framing, just the export.
            OPT_EXPORT_NAME => {
                let mut out = Vec::new();
                out.extend_from_slice(&size.to_be_bytes());
                out.extend_from_slice(&transmission_flags().to_be_bytes());
                if !no_zeroes {
                    out.extend_from_slice(&[0u8; 124]);
                }
                stream.write_all(&out)?;
                return Ok(true);
            }
            // `GO` is what a modern QEMU uses: describe the export,
            // acknowledge, and move on.
            OPT_GO | OPT_INFO => {
                send_reply(stream, option, REP_INFO, &export_info(size))?;
                send_reply(stream, option, REP_ACK, &[])?;
                if option == OPT_GO {
                    return Ok(true);
                }
            }
            OPT_ABORT => {
                send_reply(stream, option, REP_ACK, &[])?;
                return Ok(false);
            }
            // Everything else — structured replies, extended headers,
            // meta context, listing. Refusing extended headers is what
            // keeps the transmission phase in its classic shape; see
            // the module docs.
            _ => send_reply(stream, option, REP_ERR_UNSUP, &[])?,
        }
    }
}

fn transmission_flags() -> u16 {
    FLAG_HAS_FLAGS | FLAG_SEND_FLUSH | FLAG_SEND_FUA
}

/// An `NBD_INFO_EXPORT` payload: what the export is and what it can do.
fn export_info(size: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    out.extend_from_slice(&INFO_EXPORT.to_be_bytes());
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(&transmission_flags().to_be_bytes());
    out
}

fn send_reply(
    stream: &mut UnixStream,
    option: u32,
    reply_type: u32,
    payload: &[u8],
) -> io::Result<()> {
    let mut out = Vec::with_capacity(20 + payload.len());
    out.extend_from_slice(&REP_MAGIC.to_be_bytes());
    out.extend_from_slice(&option.to_be_bytes());
    out.extend_from_slice(&reply_type.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    stream.write_all(&out)
}

fn transmission(stream: &mut UnixStream, shared: &Arc<Shared>) -> io::Result<()> {
    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        let magic = match read_u32(stream) {
            Ok(m) => m,
            // A client that vanishes has disconnected, which is not an
            // error worth propagating.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        if magic != REQUEST_MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("request magic {magic:#x}"),
            ));
        }
        let flags = read_u16(stream)?;
        let command = read_u16(stream)?;
        let cookie = read_u64(stream)?;
        let offset = read_u64(stream)?;
        let length = read_u32(stream)?;

        match command {
            CMD_READ => {
                let result = shared.state.lock().unwrap().read(offset, length as usize);
                match result {
                    Ok(data) => send_simple_reply(stream, 0, cookie, &data)?,
                    Err(_) => send_simple_reply(stream, NBD_EIO, cookie, &[])?,
                }
            }
            CMD_WRITE => {
                let mut data = vec![0u8; length as usize];
                stream.read_exact(&mut data)?;
                let mut state = shared.state.lock().unwrap();
                let mut result = state.write(offset, &data);
                // FUA means the guest wants this write durable now, so
                // honouring it is the difference between modelling a
                // cache and lying about one.
                if result.is_ok() && flags & CMD_FLAG_FUA != 0 {
                    result = state.flush();
                }
                drop(state);
                match result {
                    Ok(()) => send_simple_reply(stream, 0, cookie, &[])?,
                    Err(_) => send_simple_reply(stream, NBD_EIO, cookie, &[])?,
                }
            }
            CMD_FLUSH => {
                let result = shared.state.lock().unwrap().flush();
                match result {
                    Ok(()) => send_simple_reply(stream, 0, cookie, &[])?,
                    Err(_) => send_simple_reply(stream, NBD_EIO, cookie, &[])?,
                }
            }
            CMD_DISC => return Ok(()),
            _ => send_simple_reply(stream, NBD_EINVAL, cookie, &[])?,
        }
    }
}

fn send_simple_reply(
    stream: &mut UnixStream,
    error: u32,
    cookie: u64,
    data: &[u8],
) -> io::Result<()> {
    let mut out = Vec::with_capacity(16 + data.len());
    out.extend_from_slice(&SIMPLE_REPLY_MAGIC.to_be_bytes());
    out.extend_from_slice(&error.to_be_bytes());
    out.extend_from_slice(&cookie.to_be_bytes());
    out.extend_from_slice(data);
    stream.write_all(&out)
}

fn read_u16(stream: &mut impl Read) -> io::Result<u16> {
    let mut buf = [0u8; 2];
    stream.read_exact(&mut buf)?;
    Ok(u16::from_be_bytes(buf))
}

fn read_u32(stream: &mut impl Read) -> io::Result<u32> {
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf)?;
    Ok(u32::from_be_bytes(buf))
}

fn read_u64(stream: &mut impl Read) -> io::Result<u64> {
    let mut buf = [0u8; 8];
    stream.read_exact(&mut buf)?;
    Ok(u64::from_be_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-rolled client, so the tests exercise the wire format
    /// rather than a shared helper's idea of it.
    struct Client {
        stream: UnixStream,
        cookie: u64,
    }

    impl Client {
        fn connect(disk: &MediatedDisk) -> Client {
            let mut stream = UnixStream::connect(disk.socket_path()).unwrap();

            let mut hello = [0u8; 18];
            stream.read_exact(&mut hello).unwrap();
            assert_eq!(u64::from_be_bytes(hello[0..8].try_into().unwrap()), NBD_MAGIC);
            assert_eq!(u64::from_be_bytes(hello[8..16].try_into().unwrap()), IHAVEOPT);

            // Claim fixed-newstyle + no-zeroes, exactly as QEMU does.
            stream.write_all(&3u32.to_be_bytes()).unwrap();

            // NBD_OPT_GO, empty export name, no info requests.
            let mut payload = Vec::new();
            payload.extend_from_slice(&0u32.to_be_bytes());
            payload.extend_from_slice(&0u16.to_be_bytes());
            let mut request = Vec::new();
            request.extend_from_slice(&IHAVEOPT.to_be_bytes());
            request.extend_from_slice(&OPT_GO.to_be_bytes());
            request.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            request.extend_from_slice(&payload);
            stream.write_all(&request).unwrap();

            // NBD_REP_INFO then NBD_REP_ACK.
            loop {
                assert_eq!(read_u64(&mut stream).unwrap(), REP_MAGIC);
                assert_eq!(read_u32(&mut stream).unwrap(), OPT_GO);
                let reply_type = read_u32(&mut stream).unwrap();
                let length = read_u32(&mut stream).unwrap();
                let mut body = vec![0u8; length as usize];
                stream.read_exact(&mut body).unwrap();
                if reply_type == REP_ACK {
                    break;
                }
                assert_eq!(reply_type, REP_INFO);
            }
            Client { stream, cookie: 0 }
        }

        fn request(&mut self, command: u16, flags: u16, offset: u64, length: u32, data: &[u8]) {
            self.cookie += 1;
            let mut out = Vec::new();
            out.extend_from_slice(&REQUEST_MAGIC.to_be_bytes());
            out.extend_from_slice(&flags.to_be_bytes());
            out.extend_from_slice(&command.to_be_bytes());
            out.extend_from_slice(&self.cookie.to_be_bytes());
            out.extend_from_slice(&offset.to_be_bytes());
            out.extend_from_slice(&length.to_be_bytes());
            out.extend_from_slice(data);
            self.stream.write_all(&out).unwrap();
        }

        fn reply(&mut self, data_len: usize) -> (u32, Vec<u8>) {
            assert_eq!(read_u32(&mut self.stream).unwrap(), SIMPLE_REPLY_MAGIC);
            let error = read_u32(&mut self.stream).unwrap();
            let _cookie = read_u64(&mut self.stream).unwrap();
            let mut data = vec![0u8; data_len];
            if data_len > 0 && error == 0 {
                self.stream.read_exact(&mut data).unwrap();
            }
            (error, data)
        }

        fn write(&mut self, offset: u64, data: &[u8]) {
            self.request(CMD_WRITE, 0, offset, data.len() as u32, data);
            assert_eq!(self.reply(0).0, 0);
        }

        fn read(&mut self, offset: u64, length: u32) -> Vec<u8> {
            self.request(CMD_READ, 0, offset, length, &[]);
            let (error, data) = self.reply(length as usize);
            assert_eq!(error, 0);
            data
        }

        fn flush(&mut self) {
            self.request(CMD_FLUSH, 0, 0, 0, &[]);
            assert_eq!(self.reply(0).0, 0);
        }
    }

    /// An image of `size` bytes, plus the temp dir keeping it alive.
    fn image(size: u64) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(size).unwrap();
        (dir, path)
    }

    fn serve(path: &Path, dir: &tempfile::TempDir) -> MediatedDisk {
        MediatedDisk::start(path, &dir.path().join("nbd.sock")).unwrap()
    }

    #[test]
    fn a_write_is_visible_to_the_guest_before_it_is_flushed() {
        // A cache that loses reads is not a cache. The guest must see
        // its own unflushed write, even though the disk has not taken
        // it yet.
        let (dir, path) = image(64 * 1024);
        let disk = serve(&path, &dir);
        let mut client = Client::connect(&disk);

        client.write(0, &[0xab; SECTOR]);
        assert_eq!(client.read(0, SECTOR as u32), vec![0xab; SECTOR]);
        assert_eq!(disk.unflushed_sectors(), 1, "the write is still only held");
    }

    #[test]
    fn an_unflushed_write_has_not_reached_the_backing_file() {
        let (dir, path) = image(64 * 1024);
        let disk = serve(&path, &dir);
        let mut client = Client::connect(&disk);

        client.write(0, &[0xab; SECTOR]);
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(
            &on_disk[..SECTOR],
            &[0u8; SECTOR],
            "nothing may reach the image until the guest flushes"
        );
    }

    #[test]
    fn a_power_cut_keeps_the_flushed_write_and_loses_the_unflushed_one() {
        // The whole point of PEI-1104, in one assertion: durability is
        // demonstrated by what does *not* survive.
        let (dir, path) = image(64 * 1024);
        let disk = serve(&path, &dir);
        let mut client = Client::connect(&disk);

        client.write(0, &[0x11; SECTOR]);
        client.flush();
        client.write(SECTOR as u64, &[0x22; SECTOR]);
        disk.power_cut();

        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(&on_disk[..SECTOR], &[0x11; SECTOR], "the flushed write survives");
        assert_eq!(
            &on_disk[SECTOR..2 * SECTOR],
            &[0u8; SECTOR],
            "the unflushed write is gone"
        );
    }

    #[test]
    fn fua_makes_a_single_write_durable_without_a_flush() {
        let (dir, path) = image(64 * 1024);
        let disk = serve(&path, &dir);
        let mut client = Client::connect(&disk);

        client.request(CMD_WRITE, CMD_FLAG_FUA, 0, SECTOR as u32, &[0x33; SECTOR]);
        assert_eq!(client.reply(0).0, 0);
        disk.power_cut();

        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(&on_disk[..SECTOR], &[0x33; SECTOR]);
    }

    #[test]
    fn a_partial_sector_write_preserves_the_rest_of_the_sector() {
        // The overlay is sector-granular, so a sub-sector write has to
        // start from what the sector already holds or committing it
        // would zero the bytes either side.
        let (dir, path) = image(64 * 1024);
        std::fs::write(&path, vec![0x55; 64 * 1024]).unwrap();
        let disk = serve(&path, &dir);
        let mut client = Client::connect(&disk);

        client.write(4, b"XY");
        client.flush();

        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(&on_disk[0..4], &[0x55; 4], "bytes before are untouched");
        assert_eq!(&on_disk[4..6], b"XY");
        assert_eq!(&on_disk[6..8], &[0x55; 2], "bytes after are untouched");
    }

    #[test]
    fn a_read_spanning_flushed_and_unflushed_sectors_sees_both() {
        let (dir, path) = image(64 * 1024);
        let disk = serve(&path, &dir);
        let mut client = Client::connect(&disk);

        client.write(0, &[0x11; SECTOR]);
        client.flush();
        client.write(SECTOR as u64, &[0x22; SECTOR]);

        let span = client.read(0, 2 * SECTOR as u32);
        assert_eq!(&span[..SECTOR], &[0x11; SECTOR]);
        assert_eq!(&span[SECTOR..], &[0x22; SECTOR]);
    }

    #[test]
    fn a_read_past_the_end_of_the_disk_is_refused() {
        let (dir, path) = image(SECTOR as u64);
        let disk = serve(&path, &dir);
        let mut client = Client::connect(&disk);

        client.request(CMD_READ, 0, 0, 2 * SECTOR as u32, &[]);
        assert_eq!(client.reply(0).0, NBD_EIO, "a read off the end must error");
    }

    #[test]
    fn flushes_are_counted_so_a_test_can_assert_the_guest_flushed() {
        let (dir, path) = image(64 * 1024);
        let disk = serve(&path, &dir);
        let mut client = Client::connect(&disk);

        assert_eq!(disk.flushes(), 0);
        client.write(0, &[0x11; SECTOR]);
        client.flush();
        assert_eq!(disk.flushes(), 1);
    }

    #[test]
    fn the_server_takes_a_second_connection_after_the_first_disconnects() {
        // QEMU reconnects, and a reboot means a fresh connection to the
        // same export. A server that served one client and stopped
        // would fail the reboot half of every durability test.
        let (dir, path) = image(64 * 1024);
        let disk = serve(&path, &dir);

        let mut first = Client::connect(&disk);
        first.write(0, &[0x44; SECTOR]);
        first.flush();
        first.request(CMD_DISC, 0, 0, 0, &[]);
        drop(first);

        let mut second = Client::connect(&disk);
        assert_eq!(second.read(0, SECTOR as u32), vec![0x44; SECTOR]);
    }
}
