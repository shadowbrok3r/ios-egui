//! `cargo egui-mobile` — one subcommand for both platforms: dispatches to the cargo-egui-ios
//! and cargo-egui-android libraries based on --ios/-i or --android/-a.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use cargo_egui_ios::plugin_cmd;

#[derive(Parser)]
#[command(bin_name = "cargo")]
enum CargoCli {
    #[command(name = "egui-mobile", version, about)]
    EguiMobile(Cli),
}

#[derive(Args)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Required, mutually exclusive platform selector.
#[derive(Args, Clone, Copy)]
#[group(required = true, multiple = false)]
struct Platform {
    /// Target iOS.
    #[arg(long, short = 'i')]
    ios: bool,
    /// Target Android.
    #[arg(long, short = 'a')]
    android: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scaffold a new egui app for the selected platform in a new directory.
    New {
        /// App / directory name (e.g. my-cool-app).
        name: String,
        #[command(flatten)]
        platform: Platform,
        /// iOS bundle identifier (default: com.example.<name>).
        #[arg(long, conflicts_with = "android")]
        bundle_id: Option<String>,
        /// Android package id (default: com.example.<name>).
        #[arg(long, conflicts_with = "ios")]
        package_id: Option<String>,
        /// Display name shown on the home screen / launcher (default: <name>).
        #[arg(long)]
        display_name: Option<String>,
    },
    /// Cross-compile the app for the selected platform.
    Build(BuildArgs),
    /// Cross-compile, then run on a connected device.
    Run(BuildArgs),
    /// Manage WASM UI plugins (scaffold, build, serve for hot reload); platform-neutral.
    Plugin {
        #[command(subcommand)]
        cmd: plugin_cmd::PluginCmd,
    },
}

#[derive(Args, Clone)]
struct BuildArgs {
    #[command(flatten)]
    platform: Platform,
    /// Build the release configuration.
    #[arg(long)]
    release: bool,
    /// iOS: build for the simulator target (only meaningful on a macOS host).
    #[arg(long, conflicts_with = "android")]
    simulator: bool,
    /// iOS: bundle an assets directory into the app (rsync, dereferencing symlinks).
    #[arg(long, conflicts_with = "android")]
    assets: Option<PathBuf>,
}

impl BuildArgs {
    fn ios(&self) -> cargo_egui_ios::BuildArgs {
        cargo_egui_ios::BuildArgs {
            simulator: self.simulator,
            assets: self.assets.clone(),
            release: self.release,
        }
    }

    fn android(&self) -> cargo_egui_android::BuildArgs {
        cargo_egui_android::BuildArgs { release: self.release }
    }
}

fn main() -> Result<()> {
    let CargoCli::EguiMobile(cli) = CargoCli::parse();
    match cli.cmd {
        Cmd::New {
            name,
            platform,
            bundle_id,
            package_id,
            display_name,
        } => {
            if platform.ios {
                cargo_egui_ios::cmd_new(&name, bundle_id, display_name)
            } else {
                cargo_egui_android::cmd_new(&name, package_id, display_name)
            }
        }
        Cmd::Build(args) => {
            if args.platform.ios {
                cargo_egui_ios::cmd_build(&args.ios())?;
                Ok(())
            } else {
                cargo_egui_android::cmd_build(&args.android())
            }
        }
        Cmd::Run(args) => {
            if args.platform.ios {
                cargo_egui_ios::cmd_run(&args.ios())
            } else {
                cargo_egui_android::cmd_run(&args.android())
            }
        }
        Cmd::Plugin { cmd } => plugin_cmd::run(cmd),
    }
}
