//! Terminal setup, teardown and geometry.

pub mod caps;
pub mod input;
pub mod keysym;
pub mod kitty;
pub mod shm;
pub mod writer;

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use crossterm::terminal;

static RAW: AtomicBool = AtomicBool::new(false);
static FULL_SCREEN: AtomicBool = AtomicBool::new(false);

/// Everything the full-screen session turns on.
///
/// Autowrap is off because a write that reaches the last column would scroll the
/// screen, and scrolling moves every graphics placement with it. Mouse reporting
/// is `1003` (all motion, not just while dragging) plus `1006` for the SGR
/// framing and `1016` to get that position in pixels rather than cells. The
/// keyboard flags are disambiguate, report-event-types and report-all-keys,
/// which is what turns key releases into events -- RFB needs down and up.
const SETUP: &str = concat!(
    "\x1b[?1049h", // alternate screen
    "\x1b[?25l",   // hide the text cursor
    "\x1b[?7l",    // no autowrap
    "\x1b[?1003h", // report all mouse motion
    "\x1b[?1006h", // SGR mouse framing
    "\x1b[?1016h", // ... in pixels
    "\x1b[?2004h", // bracketed paste
    "\x1b[>11u",   // kitty keyboard: disambiguate | event types | all keys
    "\x1b[2J",
);

/// The inverse of [`SETUP`], plus a delete-all-images so no placements are left
/// in the terminal's image store.
const TEARDOWN: &str = concat!(
    "\x1b_Ga=d,d=A,q=2\x1b\\",
    "\x1b[<u",
    "\x1b[?2004l",
    "\x1b[?1016l",
    "\x1b[?1006l",
    "\x1b[?1003l",
    "\x1b[?7h",
    "\x1b[?25h",
    "\x1b[?1049l",
);

/// Owns every mode change we make, and undoes them on drop and on panic.
///
/// Construction only enables raw mode: capability probing has to happen in raw
/// mode but before the alternate screen, so that a terminal which cannot do
/// graphics can be refused without a screen flash.
pub struct TerminalGuard {
    _private: (),
}

impl TerminalGuard {
    pub fn enter() -> Result<Self> {
        if !io::stdout().is_terminal() {
            anyhow::bail!("stdout is not a terminal");
        }
        install_panic_hook();
        terminal::enable_raw_mode().context("failed to enter raw mode")?;
        RAW.store(true, Ordering::SeqCst);
        Ok(Self { _private: () })
    }

    /// Switch to the alternate screen and turn on input reporting.
    pub fn begin_full_screen(&self) -> Result<()> {
        let mut out = io::stdout();
        out.write_all(SETUP.as_bytes())?;
        out.flush()?;
        FULL_SCREEN.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Restore the terminal. Idempotent, and safe from a panic or signal path.
    pub fn leave() {
        if FULL_SCREEN.swap(false, Ordering::SeqCst) {
            let mut out = io::stdout();
            let _ = out.write_all(TEARDOWN.as_bytes());
            let _ = out.flush();
        }
        if RAW.swap(false, Ordering::SeqCst) {
            let _ = terminal::disable_raw_mode();
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::leave();
    }
}

/// Restore the terminal before the default hook prints anything, so the panic
/// message lands on a usable screen rather than the alternate one.
fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            TerminalGuard::leave();
            default(info);
        }));
    });
}

/// The terminal's geometry, in both cells and pixels.
///
/// Cell size is derived by division rather than taken from `CSI 16 t`: the
/// numbers that matter are the ones consistent with the grid we address, and
/// dividing the reported pixel area by the reported cell count guarantees that.
/// The `CSI 16 t` answer is still collected during probing, where a mismatch
/// between the two is worth reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metrics {
    pub cols: u16,
    pub rows: u16,
    pub px_w: u32,
    pub px_h: u32,
    pub cell_w: u32,
    pub cell_h: u32,
}

impl Metrics {
    /// Query the terminal, preferring `TIOCGWINSZ` because it costs no round
    /// trip, and falling back to `CSI 14 t` when the kernel reports no pixel
    /// size.
    pub fn query() -> Result<Self> {
        let (cols, rows) = terminal::size().context("failed to read terminal size")?;
        anyhow::ensure!(cols > 0 && rows > 0, "terminal reports a zero-sized grid");

        let (mut px_w, mut px_h) = match terminal::window_size() {
            Ok(ws) => (u32::from(ws.width), u32::from(ws.height)),
            Err(_) => (0, 0),
        };
        if px_w == 0 || px_h == 0 {
            let (w, h) = caps::query_pixel_geometry()?;
            px_w = w;
            px_h = h;
        }
        anyhow::ensure!(
            px_w > 0 && px_h > 0,
            "terminal did not report its size in pixels, which a pixel-exact \
             client cannot work without"
        );

        Ok(Self {
            cols,
            rows,
            px_w,
            px_h,
            cell_w: (px_w / u32::from(cols)).max(1),
            cell_h: (px_h / u32::from(rows)).max(1),
        })
    }

    /// Rows available to the remote framebuffer: everything but the status line.
    pub fn image_rows(&self) -> u16 {
        self.rows.saturating_sub(1)
    }

    /// Pixel area available to the remote framebuffer.
    ///
    /// Derived from the cell size rather than from `px_w` directly: when the
    /// window is not an exact multiple of the cell size, the reported text area
    /// is larger than the cells actually cover, and the excess is not
    /// addressable.
    pub fn image_area(&self) -> (u32, u32) {
        (
            self.cell_w * u32::from(self.cols),
            self.cell_h * u32::from(self.image_rows()),
        )
    }
}
