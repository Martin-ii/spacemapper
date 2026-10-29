// SPDX-License-Identifier: GPL-3.0-or-later
//! Spacemapper: treemap disk usage viewer.

// no console window on Windows
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod dupes;
mod export;
#[cfg(target_os = "linux")]
mod fallback_libs;
mod history;
mod filter;
mod layout;
mod scan;
mod theme;
mod tree;
mod util;

use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;

pub const APP_NAME: &str = "Spacemapper";
pub const APP_ID: &str = "spacemapper";

fn main() -> eframe::Result<()> {
    // started by sudo as its password dialog (macOS full-access scan); the
    // privileged helper itself never asks
    #[cfg(target_os = "macos")]
    if std::env::var_os("SPACEMAPPER_ASKPASS").is_some() && std::env::args().nth(1).as_deref() != Some("--scan-helper") {
        std::process::exit(util::askpass());
    }
    let mut start: Option<PathBuf> = None;
    if std::env::args().len() > 1 {
        util::attach_console();
    }
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "-h" | "--help" => {
                println!(
                    "{APP_NAME} {} - treemap disk usage viewer\n\n\
                     Usage:\n  \
                     spacemapper [FOLDER]\n      \
                     Open FOLDER, or the drive list if no folder is given.\n  \
                     spacemapper --json FOLDER [--max-depth N] [--cross]\n      \
                     Scan FOLDER and print the result as JSON.\n      \
                     --max-depth N  list entries only N levels deep (default: all)\n      \
                     --cross        also scan other filesystems mounted inside\n\n\
                     Spacemapper only reads files; it does not modify or delete them.",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            "--scan-helper" => {
                // privileged read-only scanner started by the GUI via pkexec (macOS: sudo)
                let args: Vec<String> = std::env::args().collect();
                let Some(p) = args.get(2) else { std::process::exit(2) };
                let cross = args.iter().any(|a| a == "--cross");
                std::process::exit(scan::run_helper(std::path::Path::new(p), cross));
            }
            "--json" => {
                let args: Vec<String> = std::env::args().skip(2).collect();
                let mut path: Option<PathBuf> = None;
                let mut max_depth = u32::MAX;
                let mut cross = false;
                let mut it = args.iter();
                while let Some(a) = it.next() {
                    match a.as_str() {
                        "--cross" => cross = true,
                        "--max-depth" => match it.next().and_then(|v| v.parse().ok()) {
                            Some(d) => max_depth = d,
                            None => {
                                eprintln!("--max-depth needs a number");
                                std::process::exit(2);
                            }
                        },
                        _ if path.is_none() => path = Some(PathBuf::from(a)),
                        _ => {
                            eprintln!("Unexpected argument: {a}");
                            std::process::exit(2);
                        }
                    }
                }
                let Some(p) = path else {
                    eprintln!("Usage: spacemapper --json FOLDER [--max-depth N] [--cross]");
                    std::process::exit(2);
                };
                let p = util::absolute(&p);
                std::process::exit(export::run(&p, max_depth, cross));
            }
            "-V" | "--version" => {
                println!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            // macOS may add a process serial number when Finder starts the app
            _ if cfg!(target_os = "macos") && a.starts_with("-psn_") => {}
            _ => {
                let p = PathBuf::from(&a);
                start = Some(util::absolute(&p));
            }
        }
    }

    #[cfg(target_os = "linux")]
    fallback_libs::load();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_app_id(APP_ID)
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([640.0, 400.0])
            .with_icon(Arc::new(app_icon()))
            .with_drag_and_drop(true),
        // Don't block on the compositor when presenting a frame. With vsync on,
        // a window on a hidden Wayland workspace never gets its frame callback,
        // so a repaint there would hang the event loop and the compositor would
        // report the app as not responding. macOS keeps vsync: without it,
        // the scan spinner and tooltips redraw as fast as the machine allows.
        vsync: cfg!(target_os = "macos"),
        ..Default::default()
    };
    let result = eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, start)))),
    );
    if let Err(e) = &result {
        let needs = if cfg!(target_os = "macos") {
            "It needs OpenGL 2 or newer."
        } else {
            "It needs a graphical session (Wayland or X11) and OpenGL 2 or newer\n\
             (Mesa or your graphics driver)."
        };
        eprintln!(
            "{APP_NAME} could not open its window: {e}\n\n\
             {needs} To scan without a window, use:\n  \
             spacemapper --json FOLDER"
        );
    }
    result
}

/// The app icon (128x128 RGBA, pre-decoded so no image library is needed).
pub fn app_icon() -> egui::IconData {
    egui::IconData { rgba: ICON_RGBA.to_vec(), width: 128, height: 128 }
}

pub const ICON_RGBA: &[u8] = include_bytes!("../assets/icon/spacemapper-128.rgba");
