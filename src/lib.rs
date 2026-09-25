use chromiumoxide::browser::{Browser, BrowserConfig};
use futures::StreamExt;
use std::time::Duration;

const CDP_HOST: &str = "http://127.0.0.1:9222";


#[allow(dead_code)]
async fn debug_shot(page: &chromiumoxide::Page, name: &str) {
    use chromiumoxide::page::ScreenshotParams;
    if let Ok(bytes) = page
        .screenshot(ScreenshotParams::builder().full_page(true).build())
        .await
    {
        let _ = tokio::fs::write(format!("/tmp/debug_{name}.png"), bytes).await;
    }
}

pub struct ScoreChecker {
    browser: Browser,
}

impl ScoreChecker {
    //Attempt to connect to existing webrowser
    pub async fn new() -> anyhow::Result<Self> {
        let t0 = tokio::time::Instant::now();
        let browser = match Self::connect_existing().await {
            Ok(b) => {
                eprintln!("Connected to existing browser at {CDP_HOST} ({:?})", t0.elapsed());
                b
            }
            Err(e) => {
                eprintln!("No existing browser found ({e}), launching a new one");
                Self::launch_new().await?
            }
        };

        Ok(Self { browser })
    }

    async fn connect_existing() -> anyhow::Result<Browser> {
        let ws_url = Self::get_ws_endpoint(CDP_HOST).await?;

        let (browser, mut handler) = Browser::connect(&ws_url)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;

        tokio::spawn(async move {
            while let Some(h) = handler.next().await {
                if let Err(e) = h {
                    eprintln!("Handler error, browser connection closing: {e:?}");
                    break;
                }
            }
            eprintln!("Handler task exiting");
        });

        Ok(browser)
    }

    /// Fallback: launch a fresh Chrome process
    async fn launch_new() -> anyhow::Result<Browser> {
        let (browser, mut handler) = Browser::launch(
            BrowserConfig::builder()
                .no_sandbox()
                .chrome_executable("/usr/bin/google-chrome")
                .arg("--incognito")
                .arg("--headless=new")
                .arg(format!(
                    "--remote-debugging-port={}",
                    CDP_HOST.rsplit(':').next().unwrap_or("9222")
                ))
                .build()
                .map_err(|e| anyhow::anyhow!(e))?,
        )
        .await?;

        tokio::spawn(async move {
            while let Some(h) = handler.next().await {
                if let Err(e) = h {
                    eprintln!("Handler error, browser connection closing: {e:?}");
                    break;
                }
            }
            eprintln!("Handler task exiting");
        });

        Ok(browser)
    }

    async fn get_ws_endpoint(base: &str) -> anyhow::Result<String> {
        let resp = reqwest::get(format!("{base}/json/version"))
            .await?
            .json::<serde_json::Value>()
            .await?;

        resp["webSocketDebuggerUrl"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("no webSocketDebuggerUrl in /json/version response"))
    }

    async fn wait_for_selector(
        page: &chromiumoxide::Page,
        selector: &str,
        timeout: Duration,
    ) -> anyhow::Result<chromiumoxide::element::Element> {
        let start = tokio::time::Instant::now();
        loop {
            match page.find_element(selector).await {
                Ok(el) => return Ok(el),
                Err(e) => {
                    if start.elapsed() >= timeout {
                        return Err(anyhow::anyhow!("timed out waiting for {selector}: {e}"));
                    }
                    // swallow transient "no node found" / stale document errors and retry
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
    }

    pub async fn web_app_score_check(&self) -> anyhow::Result<bool> {
        let t0 = tokio::time::Instant::now();
        let page = self.browser.new_page("about:blank").await?;
        //eprintln!("[web_app] new_page: {:?}", t0.elapsed());

        let op = async {
            
            let t = tokio::time::Instant::now();
            page.goto("http://172.16.255.10/login.php").await?;
            //eprintln!("[web_app] goto login: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            let username_input = page.find_element("input[name='username']").await?;
            //eprintln!("[web_app] find username element: {:?}", t.elapsed());
            let t = tokio::time::Instant::now();
            username_input.focus().await?;
            username_input.type_str("team1").await?;
            //eprintln!("[web_app] username filled: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            let password_input = page.find_element("input[name='password']").await?;
            password_input.focus().await?;
            password_input.type_str("blackteam").await?;
            //eprintln!("[web_app] password filled: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            page.evaluate("document.querySelector(\"button[type='submit']\").click()")
                .await?;
            //eprintln!("[web_app] submit clicked: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            Self::wait_for_selector(
                &page,
                "a.nav__item.is-active[href='/dashboard.php']",
                Duration::from_secs(8),
            )
            .await?;
            //eprintln!("[web_app] dashboard selector found: {:?}", t.elapsed());

            anyhow::Ok(true)
        };

        let result = tokio::time::timeout(Duration::from_secs(15), op).await;

        // Always close the tab so it doesn't linger on the shared browser.
        let _ = page.close().await;

        match result {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                eprintln!("web app check failed: {e}");
                Ok(false)
            }
            Err(_) => {
                eprintln!("web app check timed out");
                Ok(false)
            }
        }
    }

    pub async fn jenkins_score_check(&self) -> anyhow::Result<bool> {
        let t0 = tokio::time::Instant::now();
        let page = self
            .browser
            .new_page("http://172.16.255.30:8080/login?from=%2F")
            .await?;
        //eprintln!("[jenkins] new_page+goto: {:?}", t0.elapsed());

        let op = async {
            let t = tokio::time::Instant::now();
            let username_input = page.find_element("input[name='j_username']").await?;
            username_input.focus().await?;
            username_input.type_str("blackteam").await?;
            //eprintln!("[jenkins] username filled: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            let password_input = page.find_element("input[name='j_password']").await?;
            password_input.focus().await?;
            password_input.type_str("blackteam").await?;
            //eprintln!("[jenkins] password filled: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            page.evaluate(
                "document.querySelector(\"button[type='submit'].jenkins-button--primary\").click()",
            )
            .await?;
            //eprintln!("[jenkins] submit clicked: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            Self::wait_for_selector(&page, "#root-action-UserAction", Duration::from_secs(10))
                .await?;
            //eprintln!("[jenkins] user action found: {:?}", t.elapsed());

            anyhow::Ok(true)
        };

        let result = tokio::time::timeout(Duration::from_secs(15), op).await;

        let _ = page.close().await;

        match result {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                eprintln!("jenkins check failed: {e}");
                Ok(false)
            }
            Err(_) => {
                eprintln!("jenkins check timed out");
                Ok(false)
            }
        }
    }

    pub async fn postgres_score_check(&self) -> anyhow::Result<bool> {
        let t0 = tokio::time::Instant::now();

        let config = "host=172.16.255.40 port=5432 dbname=plinkcom user=plinkcom password=blackteam";

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_postgres::connect(config, tokio_postgres::NoTls),
        )
        .await;

        match result {
            Ok(Ok((client, connection))) => {
                // PostgreSQL connection handling runs separately.
                tokio::spawn(async move {
                    if let Err(e) = connection.await {
                        eprintln!("PostgreSQL connection error: {e}");
                    }
                });

                // Actually verify that PostgreSQL can execute a query.
                match client.query_one("SELECT 1", &[]).await {
                    Ok(row) => {
                        let value: i32 = row.get(0);

                        eprintln!(
                            "postgres check succeeded: SELECT 1 = {} ({:?})",
                            value,
                            t0.elapsed()
                        );

                        Ok(value == 1)
                    }

                    Err(e) => {
                        eprintln!("postgres query failed: {e}");
                        Ok(false)
                    }
                }
            }

            Ok(Err(e)) => {
                eprintln!("postgres connection failed: {e}");
                Ok(false)
            }

            Err(_) => {
                eprintln!("postgres check timed out");
                Ok(false)
            }
        }
    }
}
