use std::process::ExitCode;

use deadlock_core::instance::{self, Instance, Request};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let request = match (args.next(), args.next()) {
        (None, _) => Request::Welcome,
        // Canonical here: the running instance has a different working directory.
        (Some(path), None) => match std::fs::canonicalize(&path) {
            Ok(path) => Request::Open(path),
            Err(e) => {
                eprintln!("deadlock: {}: {e}", path.to_string_lossy());
                return ExitCode::FAILURE;
            }
        },
        (Some(_), Some(_)) => {
            eprintln!("usage: deadlock [project folder or .sql file]");
            return ExitCode::FAILURE;
        }
    };
    let listener = match instance::claim(&instance::socket_path(), &request) {
        Ok(Instance::Primary(listener)) => Some(listener),
        Ok(Instance::Forwarded) => {
            eprintln!("deadlock: forwarded {request:?} to the running instance");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("deadlock: single instance unavailable, running alone: {e}");
            None
        }
    };
    deadlock::run(request, listener);
    ExitCode::SUCCESS
}
