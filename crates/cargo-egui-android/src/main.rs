//! `cargo egui-android` — thin CLI wrapper over the cargo-egui-android library.

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use cargo_egui_android::{BuildArgs, cmd_build, cmd_new, cmd_run};

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
    Run(BuildArgs),
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
    }
}
