//! `cargo egui-android` — thin CLI wrapper over the cargo-egui-android library.

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use cargo_egui_android::{BuildArgs, RunArgs, cmd_adb_connect, cmd_build, cmd_env, cmd_new, cmd_run};

#[derive(Parser)]
#[command(bin_name = "cargo")]
enum CargoCli {
    #[command(name = "egui-android", version, about)]
    EguiAndroid(Cli),
}

#[derive(Args)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scaffold a new egui Android app in a new directory.
    New {
        name: String,
        /// Android package id (default: com.example.<name>).
        #[arg(long)]
        package_id: Option<String>,
        /// Display name / launcher label (default: <name>).
        #[arg(long)]
        display_name: Option<String>,
    },
    /// Cross-compile + package the APK.
    Build(BuildArgs),
    /// Build, install (adb), and launch on a connected device/emulator.
    Run(RunArgs),
    /// `adb connect host[:port]` for wireless debugging (default port 5555).
    AdbConnect {
        /// Phone address, e.g. `192.168.1.20` or `192.168.1.20:5555`.
        host: String,
    },
    /// Print shell exports for SDK/NDK/JDK/Kotlin (for bare `cargo apk2`).
    Env,
}

fn main() -> Result<()> {
    let CargoCli::EguiAndroid(cli) = CargoCli::parse();
    match cli.cmd {
        Cmd::New {
            name,
            package_id,
            display_name,
        } => cmd_new(&name, package_id, display_name),
        Cmd::Build(a) => cmd_build(&a),
        Cmd::Run(a) => cmd_run(&a),
        Cmd::AdbConnect { host } => cmd_adb_connect(&host),
        Cmd::Env => cmd_env(),
    }
}
