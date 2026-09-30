use scores_check::{ScoreChecker, Service};
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let svc = match Service::from_args() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let checker = match ScoreChecker::connect().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let ok = checker.jenkins(&svc).await;
    checker.close().await;

    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}
