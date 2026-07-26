//! POSIX shared memory for the Kitty graphics protocol's `t=s` medium.
//!
//! The direct medium base64-encodes every pixel, which adds a third to the volume
//! and costs a pass over the data at both ends. Shared memory hands the terminal a
//! name instead and lets it map the bytes, so a frame costs one pass and no
//! encoding.
//!
//! One object per *frame*, not per tile. The protocol takes `O=` and `S=` -- an
//! offset and a length -- so every tile of a frame can be placed out of one
//! mapping, and a full-screen frame costs five system calls rather than five per
//! tile. Tiles are packed straight into that mapping, which is also what makes it
//! one pass over the pixels: the alternative is packing into a buffer and copying
//! the buffer in.
//!
//! Only useful when the terminal is on this machine. Over SSH the object would be
//! on the wrong side of the connection, so the caller decides.
//!
//! Ownership is subtle: the protocol makes the *terminal* responsible for
//! unlinking the object once it has read it, and Ghostty does. Unlinking eagerly
//! here would race the terminal to it and lose. So names are kept for a grace
//! period and swept afterwards, which covers the case where the terminal never
//! read the object at all -- a dropped frame, or a terminal that does not
//! implement the medium.

use std::collections::VecDeque;
use std::ffi::CString;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Names are unique per process, not per pool: two pools in one process would
/// otherwise hand out the same name and collide on `O_EXCL`.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// How long the terminal gets to consume an object before we assume it never will.
///
/// The terminal reads it while parsing the escape sequence, which is milliseconds
/// away, so this is already generous. It stays short on purpose: a full-screen
/// update is ninety-odd objects, and at thirty frames a second a two-second grace
/// would keep five thousand names waiting.
const GRACE: Duration = Duration::from_millis(500);

/// Cap on names awaiting a sweep, in case something goes very wrong.
const MAX_PENDING: usize = 4096;

pub struct ShmPool {
    counter: u64,
    pending: VecDeque<(CString, Instant)>,
}

impl ShmPool {
    pub fn new() -> Self {
        Self {
            counter: 0,
            pending: VecDeque::new(),
        }
    }

    /// Open a fresh object of exactly `len` bytes, mapped for writing.
    ///
    /// The name is remembered for sweeping here rather than by the returned frame:
    /// dropping the frame unmaps, and the object has to outlive that -- the terminal
    /// reads it when it parses the escapes, which is after the frame is gone.
    pub fn frame(&mut self, len: usize) -> io::Result<ShmFrame> {
        let (name, cname, fd) = self.create(len)?;
        // SAFETY: mapping the whole object we just sized, from a fd we own.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        // SAFETY: our own fd, closed exactly once. The mapping outlives it, as mmap
        // guarantees.
        unsafe { libc::close(fd) };
        if ptr == libc::MAP_FAILED {
            let err = io::Error::last_os_error();
            unlink(&cname);
            return Err(err);
        }
        self.remember(cname);
        Ok(ShmFrame {
            name,
            map: ptr.cast::<u8>(),
            len,
            used: 0,
        })
    }

    /// Open an object of `len` bytes and return its name, its C name and its fd.
    ///
    /// The caller owns the fd and, until it hands the name to [`Self::remember`], the
    /// object: an error on the way out has to unlink it or the name leaks until the
    /// process ends.
    fn create(&mut self, len: usize) -> io::Result<(String, CString, libc::c_int)> {
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "no data"));
        }
        self.sweep();

        // macOS caps shared memory names at 31 bytes including the leading slash,
        // so this stays terse rather than descriptive.
        self.counter = COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let name = format!("/vt{:x}-{:x}", std::process::id(), self.counter);
        let cname = CString::new(name.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad name"))?;

        let mut fd = open(&cname);
        if fd < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::AlreadyExists {
                return Err(err);
            }
            // A previous run died before its objects were consumed and the kernel
            // has since reused its pid. The stale object is ours by name, so take
            // it back rather than failing for the rest of the session.
            unlink(&cname);
            fd = open(&cname);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        // SAFETY: fd is a fresh shared memory object we own.
        if unsafe { libc::ftruncate(fd, len as libc::off_t) } < 0 {
            let err = io::Error::last_os_error();
            // SAFETY: our own fd, closed exactly once.
            unsafe { libc::close(fd) };
            unlink(&cname);
            return Err(err);
        }
        Ok((name, cname, fd))
    }

    /// Take responsibility for unlinking a name, once the terminal has had its chance.
    fn remember(&mut self, cname: CString) {
        self.pending.push_back((cname, Instant::now()));
        if self.pending.len() > MAX_PENDING
            && let Some((name, _)) = self.pending.pop_front()
        {
            unlink(&name);
        }
    }

    /// Unlink objects the terminal has had long enough to read.
    pub fn sweep(&mut self) {
        while let Some((_, at)) = self.pending.front() {
            if at.elapsed() < GRACE {
                break;
            }
            if let Some((name, _)) = self.pending.pop_front() {
                unlink(&name);
            }
        }
    }

    #[cfg(test)]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }
}

impl Drop for ShmPool {
    fn drop(&mut self) {
        for (name, _) in self.pending.drain(..) {
            unlink(&name);
        }
    }
}

/// One frame's pixels, in one object the terminal maps.
///
/// Tiles are written into it back to back and placed with an offset and a length each,
/// which is what `O=` and `S=` are for. Dropping it unmaps; the object itself outlives
/// that, because the terminal reads it when it parses the escapes -- by which time the
/// frame is long gone and [`ShmPool`] owns the name.
pub struct ShmFrame {
    name: String,
    /// A mapping of exactly `len` bytes, writable, owned until dropped.
    map: *mut u8,
    len: usize,
    used: usize,
}

impl ShmFrame {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The next `len` bytes to write into, and the offset they begin at.
    ///
    /// `None` when the object has no room, which means the caller measured the frame
    /// wrong -- there is nothing to be done about it here beyond refusing.
    pub fn next(&mut self, len: usize) -> Option<(u32, &mut [u8])> {
        let at = self.used;
        if len == 0 || at + len > self.len {
            return None;
        }
        self.used = at + len;
        // SAFETY: `map` is a mapping of `self.len` bytes and `at + len` is inside it.
        // Each call hands out a disjoint range, `used` only ever moving forward, so no
        // two of these slices overlap and none outlives the mapping.
        let slice = unsafe { std::slice::from_raw_parts_mut(self.map.add(at), len) };
        Some((at as u32, slice))
    }
}

impl Drop for ShmFrame {
    fn drop(&mut self) {
        // SAFETY: our own mapping, of the length we mapped, unmapped exactly once.
        unsafe { libc::munmap(self.map.cast::<libc::c_void>(), self.len) };
    }
}

/// Create a shared memory object, exclusively.
fn open(name: &CString) -> libc::c_int {
    // SAFETY: a valid NUL-terminated name; O_EXCL means an existing object is
    // reported rather than silently adopted.
    unsafe {
        libc::shm_open(
            name.as_ptr(),
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
            0o600 as libc::c_uint,
        )
    }
}

/// Unlink, ignoring failure: the terminal has usually got there first, and a
/// missing object is exactly what we wanted.
fn unlink(name: &CString) {
    // SAFETY: a valid NUL-terminated name.
    unsafe { libc::shm_unlink(name.as_ptr()) };
}

impl Default for ShmPool {
    fn default() -> Self {
        Self::new()
    }
}

/// Is the terminal on this machine?
///
/// Shared memory only works if it is; over SSH the object would be created on the
/// wrong side of the connection.
pub fn terminal_is_local() -> bool {
    std::env::var_os("SSH_CONNECTION").is_none()
        && std::env::var_os("SSH_CLIENT").is_none()
        && std::env::var_os("SSH_TTY").is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read an object back through `/dev/fd` style mapping, the way the terminal
    /// would.
    fn read_back(name: &str, len: usize) -> Vec<u8> {
        let cname = CString::new(name).unwrap();
        // SAFETY: opening an existing object read-only.
        let fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDONLY, 0) };
        assert!(fd >= 0, "shm_open failed: {}", io::Error::last_os_error());
        // SAFETY: mapping a region we just opened, of a length we published.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap failed");
        // SAFETY: `len` bytes were published at this mapping.
        let data = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len).to_vec() };
        // SAFETY: unmapping our own mapping, closing our own fd.
        unsafe {
            libc::munmap(ptr, len);
            libc::close(fd);
        }
        data
    }

    /// Fill a frame with `payload`, in one reservation, and return its name.
    fn publish(pool: &mut ShmPool, payload: &[u8]) -> io::Result<String> {
        let mut frame = pool.frame(payload.len())?;
        let (at, into) = frame.next(payload.len()).expect("no room for the payload");
        assert_eq!(at, 0, "the first reservation starts at the beginning");
        into.copy_from_slice(payload);
        Ok(frame.name().to_string())
    }

    #[test]
    fn written_bytes_are_readable_by_a_second_opener() {
        let mut pool = ShmPool::new();
        let payload: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        let name = publish(&mut pool, &payload).expect("frame failed");
        assert!(name.starts_with('/'), "{name}");
        assert!(
            name.len() <= 31,
            "macOS caps shared memory names at 31 bytes, got {}: {name}",
            name.len()
        );
        assert_eq!(read_back(&name, payload.len()), payload);
    }

    #[test]
    fn a_frame_hands_out_disjoint_ranges_back_to_back() {
        // What one object per frame rests on: each tile gets its own stretch of the
        // object, and the offsets it is placed with are where they actually landed.
        let mut pool = ShmPool::new();
        let mut frame = pool.frame(9).unwrap();
        let (first, a) = frame.next(4).unwrap();
        a.copy_from_slice(&[1, 2, 3, 4]);
        let (second, b) = frame.next(5).unwrap();
        b.copy_from_slice(&[5, 6, 7, 8, 9]);
        assert_eq!((first, second), (0, 4));
        assert!(frame.next(1).is_none(), "handed out more than it has");

        let name = frame.name().to_string();
        drop(frame);
        assert_eq!(read_back(&name, 9), vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn every_frame_gets_its_own_name() {
        let mut pool = ShmPool::new();
        let a = publish(&mut pool, &[1, 2, 3]).unwrap();
        let b = publish(&mut pool, &[4, 5, 6]).unwrap();
        assert_ne!(a, b);
        assert_eq!(read_back(&a, 3), vec![1, 2, 3]);
        assert_eq!(read_back(&b, 3), vec![4, 5, 6]);
    }

    #[test]
    fn dropping_the_pool_unlinks_what_is_left() {
        let name = {
            let mut pool = ShmPool::new();
            let name = publish(&mut pool, &[7; 16]).unwrap();
            assert_eq!(pool.pending(), 1);
            name
        };
        // The object is gone, so opening it must fail.
        let cname = CString::new(name).unwrap();
        // SAFETY: a valid name; failure is the expected outcome.
        let fd = unsafe { libc::shm_open(cname.as_ptr(), libc::O_RDONLY, 0) };
        assert!(fd < 0, "the object outlived the pool");
    }

    #[test]
    fn an_empty_frame_is_refused() {
        let mut pool = ShmPool::new();
        assert!(pool.frame(0).is_err());
        assert_eq!(pool.pending(), 0, "a refusal must not leave a name behind");
    }

    #[test]
    fn sweeping_early_keeps_the_terminal_from_losing_the_race() {
        // Nothing may be unlinked inside the grace period, or the terminal could
        // find the object already gone.
        let mut pool = ShmPool::new();
        let name = publish(&mut pool, &[9; 8]).unwrap();
        pool.sweep();
        assert_eq!(pool.pending(), 1, "swept an object still within its grace");
        assert_eq!(read_back(&name, 8), vec![9; 8]);
    }

    #[test]
    fn locality_is_decided_by_the_ssh_variables() {
        // Not much to assert without mutating the environment, but the answer must
        // at least be stable.
        let first = terminal_is_local();
        assert_eq!(first, terminal_is_local());
    }
}
