mod camera;
mod config;
mod controller;
mod encoder;
mod live;
mod motion;
mod overlay;
mod pre_roll;
mod process_guard;
mod recording;
mod state;
mod storage;

use anyhow::{Context, Result, bail};
use config::Config;
use signal_hook::consts::{SIGINT, SIGTERM};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn log(message: impl std::fmt::Display) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    eprintln!("[{secs}] {message}");
}

fn run(config: &Config, quit: &AtomicBool) -> Result<()> {
    storage::prepare_recording(&config.recording)?;
    log(format!(
        "monitoring camera {}; output {}",
        config.camera.index,
        config.recording.output_dir.display()
    ));
    log(if !config.recording.enabled {
        "recording mode: disabled"
    } else if config.motion.enabled {
        "recording mode: motion-triggered"
    } else {
        "recording mode: continuous"
    });
    let stream = live::LiveServer::start(&config.stream, &config.camera)?;
    while !quit.load(Ordering::Relaxed) {
        let result = camera::run(config, quit, stream.as_ref().map(|server| server.hub()));
        if let Err(error) = &result {
            log(format!("camera capture failed: {error:#}"));
        }
        if quit.load(Ordering::Relaxed) {
            result?;
            break;
        }
        log(format!(
            "reinitializing camera in {}s",
            config.camera.restart_seconds
        ));
        let until = Instant::now() + Duration::from_secs(config.camera.restart_seconds);
        while Instant::now() < until && !quit.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100));
        }
    }
    log("stopped");
    Ok(())
}

fn main() -> Result<()> {
    let mut config_path = PathBuf::from("/etc/pivigil.toml");
    let mut check = false;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--config") => {
                config_path = args.next().context("--config requires a path")?.into()
            }
            Some("--check-config") => check = true,
            Some("--version") => {
                println!("pivigil {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            Some("--help" | "-h") => {
                println!(
                    "pivigil [--config PATH] [--check-config]\n\n\
                    Direct libcamera motion recorder with hardware or software H.264 encoding.\n\
                    Default config: /etc/pivigil.toml\n\
                    Runs in the foreground; no child processes are permitted.\n\
                    --check-config validates configuration without accessing camera hardware."
                );
                return Ok(());
            }
            _ => bail!("unknown argument {}; use --help", arg.to_string_lossy()),
        }
    }
    let config = Config::load(&config_path)?;
    if check {
        let _ =
            overlay::VideoOverlay::new(&config.overlay, config.camera.width, config.camera.height)?;
        println!("configuration valid");
        return Ok(());
    }
    let quit = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGTERM, Arc::clone(&quit))?;
    signal_hook::flag::register(SIGINT, Arc::clone(&quit))?;
    // SAFETY: umask takes an integer and retains no pointers.
    unsafe {
        libc::umask(0o027);
    }
    // Install before libcamera initializes any threads or IPA modules. All threads inherit it.
    process_guard::install()?;
    run(&config, &quit)
}
