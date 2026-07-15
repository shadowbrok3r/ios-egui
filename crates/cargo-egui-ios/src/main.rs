//! `cargo egui-ios` — thin CLI wrapper over the cargo-egui-ios library.

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use cargo_egui_ios::{BuildArgs, cmd_build, cmd_new, cmd_run, plugin_cmd};

#[derive(Parser)]
#[command(bin_name = "cargo")]
enum CargoCli {
    #[command(name = "egui-ios", version, about)]
    EguiIos(Cli),
}

#[derive(Args)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scaffold a new egui iOS app in a new directory.
    New {
        /// App / directory name (e.g. my-cool-app).
        name: String,
        /// Bundle identifier (default: com.example.<name>).
        #[arg(long)]
        bundle_id: Option<String>,
        /// Display name shown on the home screen (default: <name>).
        #[arg(long)]
        display_name: Option<String>,
    },
    /// Cross-compile the Rust staticlib for iOS.
    Build(BuildArgs),
    /// Cross-compile, then run on a connected device via `xtool dev`.
    Run(BuildArgs),
    /// Manage WASM UI plugins (scaffold, build, serve for hot reload).
    Plugin {
        #[command(subcommand)]
        cmd: plugin_cmd::PluginCmd,
    },
}

fn main() -> Result<()> {
    let CargoCli::EguiIos(cli) = CargoCli::parse();
    match cli.cmd {
        Cmd::New {
            name,
            bundle_id,
            display_name,
        } => cmd_new(&name, bundle_id, display_name),
        Cmd::Build(args) => {
            cmd_build(&args)?;
            Ok(())
        }
        Cmd::Run(args) => cmd_run(&args),
        Cmd::Plugin { cmd } => plugin_cmd::run(cmd),
    }
}
