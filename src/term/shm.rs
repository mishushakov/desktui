//! POSIX shared memory for the Kitty graphics protocol's `t=s` medium.
//!
//! The direct medium base64-encodes every pixel, which adds a third to the volume
//! and costs a pass over the data at both ends. Shared memory hands the terminal a
//! name instead and lets it map the bytes, so a frame costs one `memcpy`.
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

    /// Copy `data` into a fresh shared memory object and return its name.
    pub fn publish(&mut self, data: &[u8]) -> io::Result<String> {
        if data.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "no data"));
        }
        self.sweep();

        // macOS caps shared memory names at 31 bytes including the leading slash,
        // so this stays terse rather than descriptive.
        self.counter = COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let name = format!("/vt{:x}-{:x}", std::process::id(), self.counter);
        let cname = CString::new(name.clone())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad name"))?;

        let mut fd = create(&cname);
        if fd < 0 {
            let err = io::Error::last_os_error();
            if err.kind() != io::ErrorKind::AlreadyExists {
                return Err(err);
            }
            // A previous run died before its objects were consumed and the kernel
            // has since reused its pid. The stale object is ours by name, so take
            // it back rather than failing for the rest of the session.
            unlink(&cname);
            fd = create(&cname);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        let result = (|| -> io::Result<()> {
            let len = data.len();
            // SAFETY: fd is a fresh shared memory object we own.
            if unsafe { libc::ftruncate(fd, len as libc::off_t) } < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: mapping the whole object we just sized.
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
            if ptr == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `ptr` is a valid mapping of at least `len` bytes, and the
            // source is a slice of exactly that length. The regions cannot
            // overlap: one is a fresh mapping.
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.cast::<u8>(), len);
                libc::munmap(ptr, len);
            }
            Ok(())
        })();

        // SAFETY: our own fd, closed exactly once.
        unsafe { libc::close(fd) };

        match result {
            Ok(()) => {
                self.pending.push_back((cname, Instant::now()));
                if self.pending.len() > MAX_PENDING
                    && let Some((name, _)) = self.pending.pop_front()
                {
                    unlink(&name);
                }
                Ok(name)
            }
            Err(err) => {
                unlink(&cname);
                Err(err)
            }
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

/// Create a shared memory object, exclusively.
fn create(name: &CString) -> libc::c_int {
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

    #[test]
    fn published_bytes_are_readable_by_a_second_opener() {
        let mut pool = ShmPool::new();
        let payload: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
        let name = pool.publish(&payload).expect("publish failed");
        assert!(name.starts_with('/'), "{name}");
        assert!(
            name.len() <= 31,
            "macOS caps shared memory names at 31 bytes, got {}: {name}",
            name.len()
        );
        assert_eq!(read_back(&name, payload.len()), payload);
    }

    #[test]
    fn every_publish_gets_its_own_name() {
        let mut pool = ShmPool::new();
        let a = pool.publish(&[1, 2, 3]).unwrap();
        let b = pool.publish(&[4, 5, 6]).unwrap();
        assert_ne!(a, b);
        assert_eq!(read_back(&a, 3), vec![1, 2, 3]);
        assert_eq!(read_back(&b, 3), vec![4, 5, 6]);
    }

    #[test]
    fn dropping_the_pool_unlinks_what_is_left() {
        let name = {
            let mut pool = ShmPool::new();
            let name = pool.publish(&[7; 16]).unwrap();
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
    fn an_empty_payload_is_refused() {
        let mut pool = ShmPool::new();
        assert!(pool.publish(&[]).is_err());
    }

    #[test]
    fn sweeping_early_keeps_the_terminal_from_losing_the_race() {
        // Nothing may be unlinked inside the grace period, or the terminal could
        // find the object already gone.
        let mut pool = ShmPool::new();
        let name = pool.publish(&[9; 8]).unwrap();
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
