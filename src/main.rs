//! desktui: a VNC client that draws the remote desktop with the Kitty graphics
//! protocol, one remote pixel per terminal pixel.

mod app;
mod cli;
mod render;
mod rfb;
mod session;
mod term;
mod ui;

use std::io::Write;

use anyhow::{Context, Result};
use clap::Parser;

use cli::Args;
use term::{Metrics, TerminalGuard, caps};

fn main() -> std::process::ExitCode {
    let args = Args::parse();
    init_logging(&args);

    match run(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            // The guard is gone by now, so the terminal is back to normal and
            // this lands on the primary screen where the user can read it.
            let _ = writeln!(std::io::stderr(), "desktui: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<()> {
    // Raw mode first: probing needs it, and it must happen before the alternate
    // screen so an unsupported terminal can be refused without a screen flash.
    let guard = TerminalGuard::enter()?;
    let caps = caps::probe().context("failed to probe the terminal")?;
    let metrics = Metrics::query()?;

    if args.print_caps {
        drop(guard);
        print_caps(&caps, &metrics);
        return Ok(());
    }

    if !caps.kitty_graphics && !args.force {
        drop(guard);
        anyhow::bail!(
            "this terminal did not answer the Kitty graphics query{}\n\
             \n\
             desktui draws the remote screen as real pixels, which needs that \
             protocol.\n\
             Known-good terminals: Ghostty, kitty, WezTerm.\n\
             Run with --force to try anyway, or --print-caps to see what was \
             detected.",
            match &caps.da1 {
                Some(da1) => format!(" (it identifies itself as CSI ?{da1}c)"),
                None => " (it answered nothing at all)".to_string(),
            }
        );
    }

    if args.test_pattern {
        return app::run_test_pattern(args, &caps, &guard);
    }

    let addr = args.server_addr()?;
    let password = resolve_password(args)?;

    // The session is the only part that needs async, so the runtime is built here
    // rather than wrapping main: --print-caps and --test-pattern stay synchronous.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("failed to start the async runtime")?;
    runtime.block_on(session::run(args, &caps, &guard, &addr, password))
}

/// A password from the arguments or the environment, if either has one.
///
/// Otherwise `None`, and the session prompts only if the server actually asks.
fn resolve_password(args: &Args) -> Result<Option<String>> {
    if let Some(path) = &args.password_file {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read the password file {}", path.display()))?;
        let line = contents.lines().next().unwrap_or_default().to_string();
        anyhow::ensure!(!line.is_empty(), "{} is empty", path.display());
        return Ok(Some(line));
    }
    if let Ok(password) = std::env::var("VNC_PASSWORD")
        && !password.is_empty()
    {
        return Ok(Some(password));
    }
    Ok(None)
}

/// Ask for a password on the terminal.
///
/// Raw mode is already on, so nothing is echoed and the read has to be done a
/// byte at a time. Called only when the server has said it wants one.
pub fn prompt_password(addr: &str) -> Result<String> {
    use std::io::Read;

    let mut out = std::io::stdout();
    write!(out, "Password for {addr}: ")?;
    out.flush()?;

    let mut password = String::new();
    let mut byte = [0u8; 1];
    let mut stdin = std::io::stdin();
    loop {
        if stdin.read(&mut byte)? == 0 {
            break;
        }
        match byte[0] {
            b'\r' | b'\n' => break,
            // Ctrl+C and Ctrl+D abandon the attempt.
            0x03 | 0x04 => {
                writeln!(out, "\r")?;
                anyhow::bail!("cancelled");
            }
            0x7f | 0x08 => {
                password.pop();
            }
            b => password.push(b as char),
        }
    }
    write!(out, "\r\n")?;
    out.flush()?;
    Ok(password)
}

fn print_caps(caps: &caps::Caps, metrics: &Metrics) {
    let yes_no = |b: bool| if b { "yes" } else { "no" };

    println!("terminal");
    println!(
        "  TERM={}  TERM_PROGRAM={}",
        std::env::var("TERM").unwrap_or_else(|_| "?".into()),
        std::env::var("TERM_PROGRAM").unwrap_or_else(|_| "?".into()),
    );
    if let Some(da1) = &caps.da1 {
        println!("  primary DA           CSI ?{da1}c");
    }
    println!();
    println!("capabilities");
    println!("  kitty graphics       {}", yes_no(caps.kitty_graphics));
    println!(
        "  pixel mouse (1016)   {}{}",
        yes_no(caps.pixel_mouse),
        if caps.pixel_mouse {
            ""
        } else {
            "   [pointer falls back to cell centres]"
        }
    );
    println!(
        "  sync output (2026)   {}{}",
        yes_no(caps.sync_output),
        if caps.sync_output {
            ""
        } else {
            "   [multi-tile frames may tear]"
        }
    );
    println!();
    println!("geometry");
    println!(
        "  grid                 {} x {} cells",
        metrics.cols, metrics.rows
    );
    println!("  ioctl pixel area     {} x {}", metrics.px_w, metrics.px_h);
    match caps.text_area_px {
        Some((w, h)) => println!("  CSI 14 t             {w} x {h}"),
        None => println!("  CSI 14 t             (no answer)"),
    }
    println!(
        "  derived cell         {} x {}",
        metrics.cell_w, metrics.cell_h
    );
    match caps.cell_px {
        Some((w, h)) => println!("  CSI 16 t cell        {w} x {h}"),
        None => println!("  CSI 16 t cell        (no answer)"),
    }
    let (area_w, area_h) = metrics.image_area();
    println!("  usable image area    {area_w} x {area_h} pixels");

    // A mismatch here is the HiDPI trap: if one source reports device pixels and
    // the other reports points, everything downstream is off by the scale factor.
    let mut warned = false;
    if let Some((w, h)) = caps.text_area_px
        && (w, h) != (metrics.px_w, metrics.px_h)
    {
        println!();
        println!(
            "warning: ioctl says {}x{} but CSI 14 t says {w}x{h}",
            metrics.px_w, metrics.px_h
        );
        warned = true;
    }
    if let Some((w, h)) = caps.cell_px
        && (w, h) != (metrics.cell_w, metrics.cell_h)
    {
        if !warned {
            println!();
        }
        println!(
            "warning: derived cell size {}x{} disagrees with CSI 16 t's {w}x{h}",
            metrics.cell_w, metrics.cell_h
        );
    }
}

/// Logging goes to a file or nowhere. Anything written to stdout would land in
/// the middle of a graphics escape sequence.
fn init_logging(args: &Args) {
    let Some(path) = &args.log_file else {
        return;
    };
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let filter = tracing_subscriber::EnvFilter::try_from_env("DESKTUI_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(file)
        .with_ansi(false)
        .try_init();
}
