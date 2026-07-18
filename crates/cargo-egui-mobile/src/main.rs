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
    Run(RunArgs),
    /// `adb connect host[:port]` for wireless debugging (default port 5555).
    AdbConnect {
        /// Phone address, e.g. `192.168.1.20` or `192.168.1.20:5555`.
        host: String,
    },
    /// Print shell exports for the Android toolchain (SDK/NDK/JDK/Kotlin).
    ///
    /// For bare `cargo apk2`: `eval "$(cargo egui-mobile env -a)"`.
    Env {
        #[command(flatten)]
        platform: Platform,
    },
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
    /// Android: cargo features to enable on the app crate (e.g. `tls`).
    #[arg(long, value_delimiter = ',', conflicts_with = "ios")]
    features: Vec<String>,
}

#[derive(Args, Clone)]
struct RunArgs {
    #[command(flatten)]
    build: BuildArgs,
    /// Android: connect over wireless adb before install/run (`host` or `host:port`; default port 5555).
    #[arg(long, value_name = "HOST[:PORT]", conflicts_with = "ios")]
    tcp: Option<String>,
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
        cargo_egui_android::BuildArgs { release: self.release, features: self.features.clone() }
    }
}

impl RunArgs {
    fn android(&self) -> cargo_egui_android::RunArgs {
        cargo_egui_android::RunArgs {
            build: self.build.android(),
            tcp: self.tcp.clone(),
        }
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
            if args.build.platform.ios {
                cargo_egui_ios::cmd_run(&args.build.ios())
            } else {
                cargo_egui_android::cmd_run(&args.android())
            }
        }
        Cmd::AdbConnect { host } => cargo_egui_android::cmd_adb_connect(&host),
        Cmd::Env { platform } => {
            if platform.ios {
                anyhow::bail!("env is Android-only; use -a/--android");
            }
            cargo_egui_android::cmd_env()
        }
        Cmd::Plugin { cmd } => plugin_cmd::run(cmd),
    }
}
