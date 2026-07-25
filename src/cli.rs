//! Command line surface.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

/// A pixel-perfect VNC client for terminals that speak the Kitty graphics protocol.
#[derive(Debug, Parser)]
#[command(name = "vnctui", version, about, long_about = None)]
pub struct Args {
    /// VNC server to connect to, as `host`, `host:port` or `host::port`.
    ///
    /// A bare `host` uses port 5900. A single colon is interpreted the way every
    /// other VNC client does it: `host:1` means display 1, i.e. port 5901. Use a
    /// double colon to give a raw port number: `host::5900`.
    #[arg(value_name = "HOST[:DISPLAY]", required_unless_present_any = ["print_caps", "test_pattern"])]
    pub server: Option<String>,

    /// Read the VNC password from this file (first line, trailing newline stripped).
    #[arg(long, value_name = "PATH")]
    pub password_file: Option<PathBuf>,

    /// How remote pixels map onto terminal pixels.
    #[arg(long, value_enum, default_value_t = ScaleMode::Native)]
    pub scale: ScaleMode,

    /// How image data reaches the terminal.
    #[arg(long, value_enum, default_value_t = Transfer::Auto)]
    pub transfer: Transfer,

    /// Target frame rate. Frames are dropped rather than queued when the
    /// terminal cannot keep up.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=240))]
    pub fps: u32,

    /// Never send input to the server.
    #[arg(long)]
    pub view_only: bool,

    /// Do not synchronise clipboards in either direction.
    #[arg(long)]
    pub no_clipboard: bool,

    /// Local command prefix key, as a single character interpreted as Ctrl+<char>.
    #[arg(long, default_value = "a", value_name = "CHAR")]
    pub prefix: char,

    /// Reconnect automatically, with backoff, when the session drops.
    #[arg(long)]
    pub reconnect: bool,

    /// Render even if the terminal does not answer the Kitty graphics probe.
    #[arg(long)]
    pub force: bool,

    /// Report terminal capabilities and exit.
    #[arg(long)]
    pub print_caps: bool,

    /// Render a synthetic test pattern instead of connecting to a server.
    #[arg(long)]
    pub test_pattern: bool,

    /// Append diagnostics to this file. Logging is off when unset, because
    /// anything written to stdout would corrupt the graphics stream.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<PathBuf>,
}

/// How the remote framebuffer is mapped onto the terminal's pixel area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ScaleMode {
    /// Ask the server to resize its desktop to exactly the terminal's pixel
    /// area, so one remote pixel is one terminal pixel. Falls back to `fit` if
    /// the server refuses.
    Native,
    /// Scale to fit, preserving aspect ratio, letterboxing the remainder.
    Fit,
    /// Largest whole-number scale factor that fits. Never interpolates.
    Integer,
    /// No scaling: show a 1:1 window onto the remote desktop and pan it.
    #[value(name = "1:1")]
    OneToOne,
}

impl Transfer {
    /// Pick a concrete medium.
    ///
    /// Shared memory needs two things: a terminal that answered the `t=s` probe,
    /// and a terminal on this machine. Over SSH the object would be created on the
    /// wrong side of the connection, and an unprobed terminal would fail silently,
    /// because frames go out with responses suppressed.
    pub fn resolve(self, caps: &crate::term::caps::Caps) -> crate::render::Transfer {
        match self {
            Transfer::Direct => crate::render::Transfer::Direct,
            // An explicit request is honoured, but say so if the terminal never
            // agreed to it.
            Transfer::Shm => {
                if !caps.shm_graphics {
                    tracing::warn!(
                        "shared memory was requested but the terminal did not \
                         answer the t=s probe; expect a blank screen"
                    );
                }
                if !crate::term::shm::terminal_is_local() {
                    tracing::warn!(
                        "shared memory was requested but this looks like an ssh \
                         session; the object would be created on the wrong machine"
                    );
                }
                crate::render::Transfer::Shm
            }
            // Shared memory whenever it is actually available, because the gap is
            // not small. Measured on a full-screen 1600x832 update in 91 tiles
            // (`cargo test --release --test perf -- --ignored --nocapture`):
            //
            //   pack BGRA->RGB            1.4 ms/frame
            //   direct: +zlib +base64    21.0 ms/frame   ->  48 fps ceiling
            //   shm: +object per tile     2.1 ms/frame   -> 466 fps ceiling
            //
            // The six syscalls per tile cost 0.7ms across all 91 of them; zlib
            // costs twenty. The probe is what makes this safe to default to --
            // frames go out with responses suppressed, so a medium the terminal
            // cannot handle would fail invisibly, and silence about `t=s` sends us
            // down the path that always works.
            Transfer::Auto => {
                if caps.shm_graphics && crate::term::shm::terminal_is_local() {
                    crate::render::Transfer::Shm
                } else {
                    crate::render::Transfer::Direct
                }
            }
        }
    }
}

/// Which Kitty graphics transmission medium to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Transfer {
    /// Whatever is known to work everywhere, currently the same as `direct`.
    Auto,
    /// Base64 in the escape sequence itself. Works everywhere, including SSH.
    Direct,
    /// POSIX shared memory: skips base64 and compression, at six syscalls per
    /// tile. Worth it for heavy motion on a local terminal, and useless over SSH,
    /// where the object would be created on the wrong machine.
    Shm,
}

impl Args {
    /// Resolve the `host[:display]` argument into a socket address string.
    pub fn server_addr(&self) -> anyhow::Result<String> {
        let spec = self
            .server
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("no server given"))?;
        parse_server(spec)
    }

    /// The prefix key as a control byte, e.g. `a` -> 0x01.
    pub fn prefix_char(&self) -> char {
        self.prefix.to_ascii_lowercase()
    }
}

/// Turn a VNC server spec into `host:port`.
///
/// * `host`         -> `host:5900`
/// * `host:1`       -> `host:5901`   (display number, as per convention)
/// * `host:5901`    -> `host:5901`   (values >= 5900 are taken as ports)
/// * `host::5901`   -> `host:5901`   (explicit port)
/// * `[::1]:1`      -> `[::1]:5901`  (bracketed IPv6)
fn parse_server(spec: &str) -> anyhow::Result<String> {
    let spec = spec.trim();
    if spec.is_empty() {
        anyhow::bail!("empty server address");
    }

    // `fe80::1` is a valid IPv6 address and also looks exactly like
    // `host::port`. Resolve the ambiguity by testing for an address first: no
    // hostname can parse as one, so this only ever wins when it should.
    if spec.parse::<std::net::Ipv6Addr>().is_ok() {
        return Ok(format!("[{spec}]:5900"));
    }

    // Split off the host part first so IPv6 colons don't confuse us.
    let (host, rest) = if let Some(stripped) = spec.strip_prefix('[') {
        let end = stripped
            .find(']')
            .ok_or_else(|| anyhow::anyhow!("unterminated IPv6 address in `{spec}`"))?;
        (&stripped[..end], &stripped[end + 1..])
    } else {
        match spec.split_once(':') {
            // A bare IPv6 literal has more than one colon and no brackets.
            Some((_, tail)) if tail.contains(':') && !tail.starts_with(':') => (spec, ""),
            Some((host, _)) => (host, &spec[host.len()..]),
            None => (spec, ""),
        }
    };

    if host.is_empty() {
        anyhow::bail!("missing host in `{spec}`");
    }

    let port = if let Some(explicit) = rest.strip_prefix("::") {
        explicit
            .parse::<u16>()
            .map_err(|_| anyhow::anyhow!("invalid port `{explicit}` in `{spec}`"))?
    } else if let Some(display) = rest.strip_prefix(':') {
        let n: u32 = display
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid display or port `{display}` in `{spec}`"))?;
        if n >= 5900 {
            u16::try_from(n).map_err(|_| anyhow::anyhow!("port {n} out of range"))?
        } else {
            u16::try_from(5900 + n).map_err(|_| anyhow::anyhow!("display {n} out of range"))?
        }
    } else if rest.is_empty() {
        5900
    } else {
        anyhow::bail!("trailing junk `{rest}` in `{spec}`");
    };

    if host.contains(':') {
        Ok(format!("[{host}]:{port}"))
    } else {
        Ok(format!("{host}:{port}"))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_server;

    #[test]
    fn bare_host_defaults_to_5900() {
        assert_eq!(parse_server("desk").unwrap(), "desk:5900");
    }

    #[test]
    fn single_colon_is_a_display_number() {
        assert_eq!(parse_server("desk:1").unwrap(), "desk:5901");
        assert_eq!(parse_server("desk:0").unwrap(), "desk:5900");
    }

    #[test]
    fn large_single_colon_values_are_ports() {
        assert_eq!(parse_server("desk:5901").unwrap(), "desk:5901");
    }

    #[test]
    fn double_colon_is_an_explicit_port() {
        assert_eq!(parse_server("desk::5999").unwrap(), "desk:5999");
        assert_eq!(parse_server("desk::80").unwrap(), "desk:80");
    }

    #[test]
    fn ipv6_needs_brackets_for_a_display() {
        assert_eq!(parse_server("[::1]:1").unwrap(), "[::1]:5901");
        assert_eq!(parse_server("[::1]::5900").unwrap(), "[::1]:5900");
        // Unbracketed IPv6 is accepted as a host with the default port.
        assert_eq!(parse_server("fe80::1").unwrap(), "[fe80::1]:5900");
    }

    #[test]
    fn rejects_nonsense() {
        assert!(parse_server("").is_err());
        assert!(parse_server(":1").is_err());
        assert!(parse_server("desk:abc").is_err());
        assert!(parse_server("[::1").is_err());
    }
}
