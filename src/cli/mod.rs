use std::path::Path;
use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, Parser};

/// vde-tmux CLI。サブコマンドは後続マイルストーンで追加する。
#[derive(Debug, Parser)]
#[command(version, about = "tmux state & UI manager")]
struct Cli {}

fn invoked_name() -> String {
    std::env::args()
        .next()
        .as_deref()
        .map(Path::new)
        .and_then(Path::file_stem)
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "vt".to_string())
}

pub fn run() -> ExitCode {
    let command = Cli::command().bin_name(invoked_name());
    let matches = command.get_matches();
    let _cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return ExitCode::FAILURE;
        }
    };
    // サブコマンド未実装のあいだは help を出して正常終了する。
    let _ = Cli::command().bin_name(invoked_name()).print_help();
    ExitCode::SUCCESS
}
