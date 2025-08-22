//! The runner for Duat
#![feature(decl_macro)]

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        LazyLock, Mutex,
        mpsc::{self, Receiver},
    },
    time::Instant,
};

use color_eyre::{Result, eyre::OptionExt};
use duat::{DuatChannel, Initials, MetaStatics, pre_setup, prelude::*, run_duat};
use duat_core::{
    clipboard::Clipboard,
    context,
    session::FileRet,
    ui::{self, DuatEvent, Ui as UiTrait},
};
use libloading::{Library, Symbol};
use notify::{
    Event, EventKind,
    RecursiveMode::*,
    Watcher,
    event::{AccessKind, AccessMode},
};

type RunFn = fn(
    Initials,
    MetaStatics,
    Vec<Vec<FileRet>>,
    DuatChannel,
) -> (Vec<Vec<FileRet>>, Receiver<DuatEvent>, Option<Instant>);

#[cfg(target_os = "macos")]
const CONFIG_FILE: &str = "libconfig.dylib";

#[cfg(target_os = "windows")]
const CONFIG_FILE: &str = "config.dll";

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
const CONFIG_FILE: &str = "libconfig.so";

static CLIPB: LazyLock<Mutex<Clipboard>> = LazyLock::new(Mutex::default);

fn main() -> Result<()> {
    color_eyre::install()?;

    // Initializers for access to static variables across two different
    // "duat-core instances"
    let logs = duat_core::context::Logs::new();
    log::set_logger(Box::leak(Box::new(logs.clone()))).unwrap();
    context::set_logs(logs.clone());

    let forms_init = duat_core::form::get_initial();
    duat_core::form::set_initial(forms_init);

    let (duat_tx, mut duat_rx) = mpsc::channel();
    let duat_tx: &'static mpsc::Sender<DuatEvent> = Box::leak(Box::new(duat_tx));
    duat_core::context::set_sender(duat_tx);

    let ms: &'static <Ui as ui::Ui>::MetaStatics =
        Box::leak(Box::new(<Ui as ui::Ui>::MetaStatics::default()));

    // Assert that the configuration crate actually exists.
    let Some(crate_dir) = duat_core::crate_dir().ok().filter(|cd| cd.exists()) else {
        context::error!("No config crate found, loading default config");

        pre_setup(None, duat_tx);
        run_duat((ms, &CLIPB), Vec::new(), duat_rx);

        return Ok(());
    };

    let mut lib = {
        let so_dir = match cfg!(debug_assertions) {
            true => [
                "target/debug".into(),
                format!("target/{}/debug", duat::built_info::TARGET,),
            ],
            false => [
                "target/release".into(),
                format!("target/{}/release", duat::built_info::TARGET),
            ],
        }
        .map(|p| crate_dir.join(p));

        let libconfig_str = CONFIG_FILE;
        let libconfig_path = so_dir.map(|d| d.join(libconfig_str));

        if let [Ok(false) | Err(_), Ok(false) | Err(_)] =
            libconfig_path.clone().map(|p| p.try_exists())
        {
            println!("Compiling config crate for the first time, this might take a while...");

            let toml_path = crate_dir.join("Cargo.toml");

            if let Ok(status) = run_cargo(&toml_path, true, true)
                && status.success()
            {
                context::info!("Compiled [a]release[] profile");
            } else {
                context::error!("Failed to compile [a]release[] profile");
            }
        }

        let libconfig_path = libconfig_path
            .into_iter()
            .find(|p| matches!(p.try_exists(), Ok(true)))
            .ok_or_eyre(format!("{CONFIG_FILE} not found!"))?;

        Some(unsafe { Library::new(libconfig_path) }?)
    };

    // The watcher is returned as to not be dropped.
    let (reload_tx, reload_rx) = mpsc::channel();
    let _watcher = spawn_watcher(reload_tx, duat_tx, crate_dir);

    Ui::open(ms, ui::Sender::new(duat_tx));

    let mut prev = Vec::new();

    loop {
        let running_lib = lib.take();
        let mut run_fn = running_lib
            .as_ref()
            .ok_or_eyre("No running lib!")
            .and_then(find_run_duat)
            .inspect_err(|err| {
                context::error!("{err}");
            })
            .ok();

        let reload_instant;

        (prev, duat_rx, reload_instant) = std::thread::scope(|s| {
            s.spawn(|| {
                if let Some(run_duat) = run_fn.take() {
                    let initials = (logs.clone(), forms_init);
                    let channel = (duat_tx, duat_rx);

                    run_duat(initials, (ms, &CLIPB), prev, channel)
                } else {
                    context::error!("Failed to load config crate");

                    pre_setup(None, duat_tx);
                    run_duat((ms, &CLIPB), prev, duat_rx)
                }
            })
            .join()
            .unwrap()
        });

        duat_core::form::clear();

        if let Some(lib) = running_lib {
            lib.close().unwrap();
        }

        if prev.is_empty() {
            break;
        }

        let (so_path, on_release) = reload_rx.recv().unwrap();

        let profile = if on_release { "Release" } else { "Debug" };
        let time = match reload_instant {
            Some(reload_instant) => txt!(" in [a]{:.2?}", reload_instant.elapsed()),
            None => Text::builder(),
        };

        context::info!("[a]{profile}[] profile reloaded{time}");

        lib = unsafe { Library::new(so_path) }.ok();
    }

    Ui::close(ms);

    Ok(())
}

fn spawn_watcher(
    reload_tx: mpsc::Sender<(PathBuf, bool)>,
    duat_tx: &mpsc::Sender<DuatEvent>,
    crate_dir: &'static std::path::Path,
) -> Result<(notify::RecommendedWatcher, &'static std::path::Path)> {
    let mut watcher = notify::recommended_watcher({
        let reload_tx = reload_tx.clone();
        let duat_tx = duat_tx.clone();
        let mut sent_reload = false;
        let libconfig_str = CONFIG_FILE;

        move |res| match res {
            Ok(Event { kind: EventKind::Create(_), paths, .. }) => {
                if let Some(so_path) = paths.iter().find(|p| p.ends_with(libconfig_str)) {
                    let on_release = so_path.ends_with(format!("release/{libconfig_str}"));

                    reload_tx.send((so_path.clone(), on_release)).unwrap();

                    sent_reload = true;
                }
            }
            Ok(Event {
                kind: EventKind::Access(AccessKind::Close(AccessMode::Write)),
                paths,
                ..
            }) if paths.iter().any(|p| p.ends_with(".cargo-lock")) && sent_reload => {
                duat_tx.send(DuatEvent::ReloadConfig).unwrap();

                sent_reload = false;
            }
            _ => {}
        }
    })
    .unwrap();

    [
        "target/debug".into(),
        "target/release".into(),
        format!("target/{}/debug", duat::built_info::TARGET),
        format!("target/{}/release", duat::built_info::TARGET),
    ]
    .into_iter()
    .try_for_each(|path| -> Result<()> {
        let path = crate_dir.join(path);

        if !path.exists() {
            std::fs::create_dir_all(&path)?;
        }

        watcher.watch(&path, NonRecursive)?;

        Ok(())
    })?;

    Ok((watcher, crate_dir))
}

fn run_cargo(
    toml_path: impl AsRef<Path>,
    on_release: bool,
    print: bool,
) -> Result<std::process::ExitStatus> {
    let toml_path = toml_path.as_ref();

    let mut cargo = Command::new("cargo");
    cargo.args(["build", "--manifest-path", toml_path.to_str().unwrap()]);

    if !cfg!(debug_assertions) && on_release {
        cargo.args(["--release"]);
    }

    #[cfg(feature = "deadlocks")]
    cargo.args(["--features", "deadlocks"]);

    let status = match print {
        true => cargo.status()?,
        false => cargo.output().map(|out| {
            if !out.status.success() {
                context::error!("{}", String::from_utf8_lossy(&out.stderr));
            }

            out.status
        })?,
    };

    Ok(status)
}

fn find_run_duat(lib: &Library) -> Result<Symbol<'_, RunFn>> {
    let run_fn = unsafe { lib.get::<RunFn>(b"run")? };

    Ok(run_fn)
}
