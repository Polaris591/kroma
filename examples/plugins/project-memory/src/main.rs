//! Kroma project-memory plugin worker.
//!
//! Worker mode speaks JSONL on stdin/stdout and writes no other stdout output.
//! `--help` and `--version` are the only local CLI modes.

use std::io::{self, BufReader, BufWriter};

fn main() {
    if let Err(error) = run() {
        eprintln!("kroma-memory: {error}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let mut arguments = std::env::args().skip(1);
    if let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            "--version" | "-V" => {
                println!("kroma-memory {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => anyhow::bail!("unknown argument; use --help"),
        }
    }

    let stdin = io::stdin();
    let stdout = io::stdout();
    let reader = BufReader::new(stdin.lock());
    let writer = BufWriter::new(stdout.lock());
    kroma_memory::protocol::run(reader, writer)
}

fn print_help() {
    println!(
        "kroma-memory {}\n\
         Local, workdir-isolated project memory plugin for Kroma.\n\
         \n\
         Usage:\n\
         \x20 kroma-memory            Run the JSONL plugin worker on stdin/stdout\n\
         \x20 kroma-memory --help     Show this help\n\
         \x20 kroma-memory --version  Show the version\n\
         \n\
         Worker requests use Kroma plugin API 2:\n\
         \x20 ping, tool (memory_search, memory_read, memory_remember, memory_forget),\n\
         \x20 command (memory), request_context, task_summary and ui panel.\n\
         Storage is private local SQLite; Git worktrees share a common-dir identity; no network access.",
        env!("CARGO_PKG_VERSION")
    );
}
