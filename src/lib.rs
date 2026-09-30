use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::browser::BrowserContextId;
use chromiumoxide::cdp::browser_protocol::page::NavigateParams;
use chromiumoxide::cdp::browser_protocol::target::{CreateBrowserContextParams, CreateTargetParams};
use futures::StreamExt;
use mysql_async::{prelude::*, Conn, OptsBuilder};
use std::time::Duration;

const CDP_HOST: &str = "http://127.0.0.1:9222";

const POLL_INTERVAL: Duration = Duration::from_millis(50);

const OPEN_WEBUI_FAST_PASS: bool = false;

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
    ctx: BrowserContextId,
}

impl ScoreChecker {
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

        if let Ok(pages) = browser.pages().await {
            let leaked = pages.len();
            if leaked > 0 {
                eprintln!("Closing {leaked} leftover page(s) from a previous run");
                for page in pages {
                    let _ = page.close().await;
                }
            }
        }

        let ctx = browser
            .create_browser_context(CreateBrowserContextParams::default())
            .await?;

        Ok(Self { browser, ctx })
    }

    pub async fn close(self) {
        let _ = self.browser.dispose_browser_context(self.ctx).await;
    }

    async fn new_page(&self, url: &str) -> anyhow::Result<chromiumoxide::Page> {
        let params = CreateTargetParams::builder()
            .url(url)
            .browser_context_id(self.ctx.clone())
            .build()
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(self.browser.new_page(params).await?)
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

    async fn launch_new() -> anyhow::Result<Browser> {
        let (browser, mut handler) = Browser::launch(
            BrowserConfig::builder()
                .no_sandbox()
                .chrome_executable("/usr/bin/google-chrome")
                .arg("--incognito")
                .arg("--headless=new")
                // Concurrent tabs in one browser can get throttled in the
                // background, which hurts SPAs like Open WebUI.
                .arg("--disable-background-timer-throttling")
                .arg("--disable-renderer-backgrounding")
                .arg("--disable-backgrounding-occluded-windows")
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

    async fn navigate(page: &chromiumoxide::Page, url: &str) -> anyhow::Result<()> {
        let resp = page.execute(NavigateParams::new(url)).await?;
        if let Some(err) = &resp.result.error_text {
            anyhow::bail!("navigation to {url} failed: {err}");
        }
        Ok(())
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
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
            }
        }
    }

    async fn wait_for_js(
        page: &chromiumoxide::Page,
        expr: &str,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let start = tokio::time::Instant::now();
        loop {
            if let Ok(r) = page.evaluate(expr).await {
                if r.into_value::<bool>().unwrap_or(false) {
                    return Ok(());
                }
            }
            if start.elapsed() >= timeout {
                anyhow::bail!("timed out waiting for js: {expr}");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    pub async fn web_app_score_check(&self) -> anyhow::Result<bool> {
        let t0 = tokio::time::Instant::now();
        let page = self.new_page("about:blank").await?;
        eprintln!("[web_app] new_page: {:?}", t0.elapsed());

        let op = async {
            let t = tokio::time::Instant::now();
            Self::navigate(&page, "http://172.16.255.10/login.php").await?;
            eprintln!("[web_app] navigate login: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            let username_input =
                Self::wait_for_selector(&page, "input[name='username']", Duration::from_secs(8))
                    .await?;
            eprintln!("[web_app] find username element: {:?}", t.elapsed());
            let t = tokio::time::Instant::now();
            username_input.focus().await?;
            username_input.type_str("team1").await?;
            eprintln!("[web_app] username filled: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            let password_input =
                Self::wait_for_selector(&page, "input[name='password']", Duration::from_secs(8))
                    .await?;
            password_input.focus().await?;
            password_input.type_str("blackteam").await?;
            eprintln!("[web_app] password filled: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            page.evaluate("document.querySelector(\"button[type='submit']\").click()")
                .await?;
            eprintln!("[web_app] submit clicked: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            match Self::wait_for_selector(
                &page,
                "a.nav__item.is-active[href='/dashboard.php']",
                Duration::from_secs(8),
            )
            .await
            {
                Ok(_) => {
                    eprintln!("[web_app] dashboard selector found: {:?}", t.elapsed());
                }
                Err(e) => {
                    // Login didn't land where expected — capture what the
                    // page actually looks like right now before we give up.
                    debug_shot(&page, "web_app_failure").await;

                    let url = page.url().await.ok().flatten();
                    eprintln!("[web_app] failed, current URL: {url:?}");

                    if let Ok(html) = page.content().await {
                        eprintln!(
                            "[web_app] page HTML snippet: {}",
                            &html.chars().take(2000).collect::<String>()
                        );
                    }

                    return Err(e);
                }
            }

            anyhow::Ok(true)
        };

        let result = tokio::time::timeout(Duration::from_secs(30), op).await;

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
        let page = self.new_page("about:blank").await?;
        eprintln!("[jenkins] new_page: {:?}", t0.elapsed());

        let op = async {
            let t = tokio::time::Instant::now();
            Self::navigate(&page, "http://172.16.255.30:8080/login?from=%2F").await?;
            eprintln!("[jenkins] navigate login: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            let username_input =
                Self::wait_for_selector(&page, "input[name='j_username']", Duration::from_secs(8))
                    .await?;
            username_input.focus().await?;
            username_input.type_str("blackteam").await?;
            eprintln!("[jenkins] username filled: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            let password_input =
                Self::wait_for_selector(&page, "input[name='j_password']", Duration::from_secs(8))
                    .await?;
            password_input.focus().await?;
            password_input.type_str("blackteam").await?;
            eprintln!("[jenkins] password filled: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            page.evaluate(
                "document.querySelector(\"button[type='submit'].jenkins-button--primary\").click()",
            )
            .await?;
            eprintln!("[jenkins] submit clicked: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            Self::wait_for_selector(&page, "#root-action-UserAction", Duration::from_secs(10))
                .await?;
            eprintln!("[jenkins] user action found: {:?}", t.elapsed());

            anyhow::Ok(true)
        };

        let result = tokio::time::timeout(Duration::from_secs(35), op).await;

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

    pub async fn mysql_score_check(&self) -> anyhow::Result<bool> {
        let t0 = tokio::time::Instant::now();

        let opts = OptsBuilder::default()
            .ip_or_hostname("172.16.255.40")
            .tcp_port(3306)
            .db_name(Some("plinkcom"))
            .user(Some("plinkcom"))
            .pass(Some("blackteam"));

        // Put connect + query inside one timeout so a hung query can't stall the check.
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            let mut conn = Conn::new(opts).await?;
            let value: Option<i32> = conn.query_first("SELECT 1").await?;
            let _ = conn.disconnect().await; // ignore disconnect errors
            Ok::<_, mysql_async::Error>(value)
        })
        .await;

        match result {
            Ok(Ok(Some(value))) => {
                eprintln!(
                    "mysql check succeeded: SELECT 1 = {} ({:?})",
                    value,
                    t0.elapsed()
                );
                Ok(value == 1)
            }
            Ok(Ok(None)) => {
                eprintln!("mysql query returned no rows");
                Ok(false)
            }
            Ok(Err(e)) => {
                eprintln!("mysql check failed: {e}");
                Ok(false)
            }
            Err(_) => {
                eprintln!("mysql check timed out");
                Ok(false)
            }
        }
    }

    pub async fn open_webui_score_check(&self) -> anyhow::Result<bool> {
        let t0 = tokio::time::Instant::now();
        let page = self.new_page("about:blank").await?;
        eprintln!("[open_webui] new_page: {:?}", t0.elapsed());

        let op = async {
            let t = tokio::time::Instant::now();
            Self::navigate(&page, "http://172.16.255.20/auth").await?;
            eprintln!("[open_webui] navigate login: {:?}", t.elapsed());

                    let t = tokio::time::Instant::now();
            Self::wait_for_selector(&page, "input[type='email']", Duration::from_secs(10)).await?;
            eprintln!("[open_webui] login form mounted: {:?}", t.elapsed());

                       let t = tokio::time::Instant::now();
            let filled = page
                .evaluate(
                    r#"
                    (function() {
                        const email = document.querySelector("input[type='email']");
                        const password = document.querySelector("input[type='password']");
                        if (!email || !password) return false;

                        const setter = Object.getOwnPropertyDescriptor(
                            window.HTMLInputElement.prototype, 'value'
                        ).set;

                        setter.call(email, "admin@admin.com");
                        email.dispatchEvent(new Event('input', { bubbles: true }));

                        setter.call(password, "admin");
                        password.dispatchEvent(new Event('input', { bubbles: true }));
                        return true;
                    })();
                    "#,
                )
                .await?
                .into_value::<bool>()?;
            if !filled {
                anyhow::bail!("login form fields missing when filling");
            }
            eprintln!("[open_webui] email+password filled: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            page.evaluate("document.querySelector(\"button[type='submit']\").click()")
                .await?;
            eprintln!("[open_webui] submit clicked: {:?}", t.elapsed());

            let t = tokio::time::Instant::now();
            let logged_in = if OPEN_WEBUI_FAST_PASS {
                Self::wait_for_js(
                    &page,
                    "!!localStorage.getItem('token')",
                    Duration::from_secs(10),
                )
                .await
            } else {
                Self::wait_for_selector(&page, "#chat-input", Duration::from_secs(10))
                    .await
                    .map(|_| ())
            };

            match logged_in {
                Ok(()) => {
                    eprintln!("[open_webui] login confirmed: {:?}", t.elapsed());
                }
                Err(e) => {
                    debug_shot(&page, "open_webui_failure").await;

                    let url = page.url().await.ok().flatten();
                    eprintln!("[open_webui] failed, current URL: {url:?}");

                    if let Ok(html) = page.content().await {
                        eprintln!(
                            "[open_webui] page HTML snippet: {}",
                            &html.chars().take(2000).collect::<String>()
                        );
                    }

                    return Err(e);
                }
            }

            anyhow::Ok(true)
        };

        let result = tokio::time::timeout(Duration::from_secs(30), op).await;

        let _ = page.close().await;

        match result {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                eprintln!("open webui check failed: {e}");
                Ok(false)
            }
            Err(_) => {
                eprintln!("open webui check timed out");
                Ok(false)
            }
        }
    }
}
