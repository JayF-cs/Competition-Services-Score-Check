use scores_check::{mysql_check, Service};
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

    if mysql_check(&svc).await {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
