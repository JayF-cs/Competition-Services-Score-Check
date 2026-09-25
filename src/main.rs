use scores_check::ScoreChecker;
use std::time::Instant;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let t0 = Instant::now();
    let checker = ScoreChecker::new().await?;
    eprintln!("[main] checker ready: {:?}", t0.elapsed());

    let t1 = Instant::now();
    let (web_app, jenkins, db) = tokio::join!(
        checker.web_app_score_check(),
        checker.jenkins_score_check(),
        checker.postgres_score_check(),
    );
    eprintln!("[main] both checks done: {:?}", t1.elapsed());

    let web_app = web_app?;
    let jenkins = jenkins?;
    let db = db?

    if !web_app {
        println!("Web app failed");
    }

    if !jenkins {
        println!("Jenkins Failed");
    }

    if !db {
        println!("DB Failed");

    }

    eprintln!("[main] total: {:?}", t0.elapsed());

    Ok(())
}
