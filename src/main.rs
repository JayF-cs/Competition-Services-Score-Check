use scores_check::ScoreChecker;
use std::time::Instant;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let t0 = Instant::now();
    let checker = ScoreChecker::new().await?;
    eprintln!("[main] checker ready: {:?}", t0.elapsed());

    let t1 = Instant::now();
    let (web_app, jenkins, sql, open_webui) = tokio::join!(
        checker.web_app_score_check(),
        checker.jenkins_score_check(),
        checker.mysql_score_check(),
        checker.open_webui_score_check()
    );
    eprintln!("[main] all checks done: {:?}", t1.elapsed());

    let web_app = web_app?;
    let jenkins = jenkins?;
    let sql = sql?;
    let open_webui = open_webui?;

    if !web_app {
        println!("Web app failed");
    }

    if !jenkins {
        println!("Jenkins Failed");
    }

    if !sql {
        println!("SQL Failed");
    }

    if !open_webui {
        println!("Open WebUI Failed");
    }

    eprintln!("[main] total: {:?}", t0.elapsed());

    Ok(())
}
