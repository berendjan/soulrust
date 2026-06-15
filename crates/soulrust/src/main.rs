use std::sync::atomic::Ordering;
use std::time::Duration;

use rust_messenger::message_bus::atomic_circular_bus::CircularBus;
use rust_messenger::message_bus::condvar_bus::CondvarBus;

use soulrust::config::{default_config_path, load_config, AppContext};

fn main() {
    let config_path = default_config_path();
    let config = load_config(&config_path);
    println!(
        "soulrust v{} — config: {}",
        soulrust::version::VERSION,
        config_path.display()
    );

    let ctx = AppContext::new(config, config_path);
    let bus = CondvarBus::new(CircularBus::new(&ctx));
    let messenger = soulrust::wiring::Messenger::new(bus);
    let handles = messenger.run(&ctx);

    // The web bridge flips these flags on POST /quit and /restart.
    let restart = loop {
        std::thread::sleep(Duration::from_millis(200));
        if ctx.control.quit.load(Ordering::Relaxed) {
            break false;
        }
        if ctx.control.restart.load(Ordering::Relaxed) {
            break true;
        }
    };

    messenger.stop();
    handles.join();

    if restart {
        restart_self();
    }
}

/// Replaces this process with a fresh copy of the (possibly just-updated)
/// executable, preserving arguments.
fn restart_self() {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("cannot restart: {err}");
            return;
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(&exe).args(&args).exec();
        eprintln!("restart failed: {err}");
    }

    #[cfg(not(unix))]
    {
        match std::process::Command::new(&exe).args(&args).spawn() {
            Ok(_) => {}
            Err(err) => eprintln!("restart failed: {err}"),
        }
    }
}
