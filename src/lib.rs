use chromiumoxide::browser::Browser;
use chromiumoxide::cdp::browser_protocol::browser::BrowserContextId;
use chromiumoxide::cdp::browser_protocol::page::NavigateParams;
use chromiumoxide::cdp::browser_protocol::target::{CreateBrowserContextParams, CreateTargetParams};
use futures::StreamExt;
use mysql_async::{prelude::*, Conn, OptsBuilder};
use serde::Deserialize;
use std::time::Duration;
use tokio::time::Instant;

const CDP_HOST: &str = "http://127.0.0.1:9222";


const CHECK_TIMEOUT: Duration = Duration::from_millis(4000);


const DIAG_MARGIN: Duration = Duration::from_millis(400);

const POLL_INTERVAL: Duration = Duration::from_millis(50);


const OPEN_WEBUI_FAST_PASS: bool = true;


const MYSQL_DB: &str = "plinkcom";

// ---------------------------------------------------------------------------
// Service config (the JSON file the agent passes as argv[1])
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct User {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct Service {
    pub host: String,
    pub team_num: u32,
    pub user: Option<User>,
}

impl Service {
    pub fn from_args() -> anyhow::Result<Self> {
        let path = std::env::args()
            .nth(1)
            .ok_or_else(|| anyhow::anyhow!("usage: <check> <config.json>"))?;
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
        serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("bad config json: {e}"))
    }

    fn creds(&self) -> anyhow::Result<(&str, &str)> {
        match &self.user {
            Some(u) => Ok((u.username.as_str(), u.password.as_str())),
            None => anyhow::bail!("service config has no `user` block (is a user list set?)"),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

async fn dump_failure(page: &chromiumoxide::Page, tag: &str) {
    let url = page.url().await.ok().flatten();
    eprintln!("[{tag}] failed, current URL: {url:?}");
    if let Ok(html) = page.content().await {
        eprintln!(
            "[{tag}] page HTML snippet: {}",
            html.chars().take(500).collect::<String>()
        );
    }
}

fn report(
    tag: &str,
    started: Instant,
    result: Result<anyhow::Result<()>, tokio::time::error::Elapsed>,
) -> bool {
    match result {
        Ok(Ok(())) => {
            eprintln!("[{tag}] OK ({:?})", started.elapsed());
            true
        }
        Ok(Err(e)) => {
            eprintln!("[{tag}] FAILED: {e}");
            false
        }
        Err(_) => {
            eprintln!("[{tag}] FAILED: timed out after {:?}", started.elapsed());
            false
        }
    }
}

async fn get_ws_endpoint(base: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let resp = client
        .get(format!("{base}/json/version"))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    resp["webSocketDebuggerUrl"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("no webSocketDebuggerUrl in /json/version response"))
}

// ---------------------------------------------------------------------------
// Browser-based checks
// ---------------------------------------------------------------------------

pub struct ScoreChecker {
    browser: Browser,
    ctx: BrowserContextId,
}

impl ScoreChecker {
    pub async fn connect() -> anyhow::Result<Self> {
        let ws_url = get_ws_endpoint(CDP_HOST).await.map_err(|e| {
            anyhow::anyhow!("cannot reach Chrome at {CDP_HOST} (is it running?): {e}")
        })?;

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
        });

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
        until: Instant,
    ) -> anyhow::Result<chromiumoxide::element::Element> {
        loop {

            match tokio::time::timeout_at(until, page.find_element(selector)).await {
                Ok(Ok(el)) => return Ok(el),
                Ok(Err(e)) => {
                    if Instant::now() >= until {
                        return Err(anyhow::anyhow!("timed out waiting for {selector}: {e}"));
                    }

                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                Err(_) => {
                    return Err(anyhow::anyhow!(
                        "timed out waiting for {selector} (CDP call never returned)"
                    ));
                }
            }
        }
    }


    async fn wait_for_js(
        page: &chromiumoxide::Page,
        expr: &str,
        until: Instant,
    ) -> anyhow::Result<()> {
        loop {
            match tokio::time::timeout_at(until, page.evaluate(expr)).await {
                Ok(Ok(r)) => {
                    if r.into_value::<bool>().unwrap_or(false) {
                        return Ok(());
                    }
                }
                Ok(Err(_)) => {}
                Err(_) => {
                    anyhow::bail!("timed out waiting for js (evaluate never returned): {expr}");
                }
            }
            if Instant::now() >= until {
                anyhow::bail!("timed out waiting for js: {expr}");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    pub async fn web_app(&self, svc: &Service) -> bool {
        const TAG: &str = "web_app";
        let (username, password) = match svc.creds() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[{TAG}] {e}");
                return false;
            }
        };

        let t0 = Instant::now();
        let page = match self.new_page("about:blank").await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[{TAG}] could not open page: {e}");
                return false;
            }
        };

        let outer = t0 + CHECK_TIMEOUT;
        let until = outer - DIAG_MARGIN;


        let op = async {
            Self::navigate(&page, &format!("http://{}/login.php", svc.host)).await?;

            let user_input =
                Self::wait_for_selector(&page, "input[name='username']", until).await?;
            user_input.focus().await?;
            user_input.type_str(username).await?;

            let pass_input =
                Self::wait_for_selector(&page, "input[name='password']", until).await?;
            pass_input.focus().await?;
            pass_input.type_str(password).await?;

            page.evaluate("document.querySelector(\"button[type='submit']\").click()")
                .await?;

            if let Err(e) = Self::wait_for_selector(
                &page,
                "a.nav__item.is-active[href='/dashboard.php']",
                until,
            )
            .await
            {
                dump_failure(&page, TAG).await;
                return Err(e);
            }

            anyhow::Ok(())
        };

        let result = tokio::time::timeout_at(outer, op).await;

        let _ = page.close().await;
        report(TAG, t0, result)
    }

    pub async fn jenkins(&self, svc: &Service) -> bool {
        const TAG: &str = "jenkins";
        let (username, password) = match svc.creds() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[{TAG}] {e}");
                return false;
            }
        };

        let t0 = Instant::now();
        let page = match self.new_page("about:blank").await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[{TAG}] could not open page: {e}");
                return false;
            }
        };

        let outer = t0 + CHECK_TIMEOUT;
        let until = outer - DIAG_MARGIN;

        let op = async {
            Self::navigate(&page, &format!("http://{}:8080/login?from=%2F", svc.host)).await?;

            let user_input =
                Self::wait_for_selector(&page, "input[name='j_username']", until).await?;
            user_input.focus().await?;
            user_input.type_str(username).await?;

            let pass_input =
                Self::wait_for_selector(&page, "input[name='j_password']", until).await?;
            pass_input.focus().await?;
            pass_input.type_str(password).await?;

            page.evaluate(
                "document.querySelector(\"button[type='submit'].jenkins-button--primary\").click()",
            )
            .await?;

            // Positive + negative: we're off the login page, the login form is gone,
            // and a Jenkins-authenticated-page marker is present.
            if let Err(e) = Self::wait_for_js(
                &page,
                "location.pathname !== '/login' && !document.querySelector('input[name=\"j_username\"]') && !!document.querySelector('#breadcrumb-menu, .jenkins-breadcrumbs, .jenkins-header')",
                until,
            )
            .await
            {
                dump_failure(&page, TAG).await;
                return Err(e);
            }

            anyhow::Ok(())
        };

        let result = tokio::time::timeout_at(outer, op).await;
        let _ = page.close().await;
        report(TAG, t0, result)
    }

    pub async fn open_webui(&self, svc: &Service) -> bool {
        const TAG: &str = "open_webui";
        let (username, password) = match svc.creds() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[{TAG}] {e}");
                return false;
            }
        };

        let (user_js, pass_js) = match (
            serde_json::to_string(username),
            serde_json::to_string(password),
        ) {
            (Ok(u), Ok(p)) => (u, p),
            _ => {
                eprintln!("[{TAG}] could not encode credentials");
                return false;
            }
        };

        let t0 = Instant::now();
        let page = match self.new_page("about:blank").await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[{TAG}] could not open page: {e}");
                return false;
            }
        };

        let outer = t0 + CHECK_TIMEOUT;
        let until = outer - DIAG_MARGIN;

        let op = async {
            Self::navigate(&page, &format!("http://{}/auth", svc.host)).await?;
            eprintln!("[{TAG}] navigated: {:?}", t0.elapsed());

            Self::wait_for_selector(&page, "input[type='email']", until).await?;
            eprintln!("[{TAG}] form mounted: {:?}", t0.elapsed());

            let fill_js = r#"
                (function() {
                    const email = document.querySelector("input[type='email']");
                    const password = document.querySelector("input[type='password']");
                    if (!email || !password) return false;

                    const setter = Object.getOwnPropertyDescriptor(
                        window.HTMLInputElement.prototype, 'value'
                    ).set;

                    setter.call(email, __USER__);
                    email.dispatchEvent(new Event('input', { bubbles: true }));

                    setter.call(password, __PASS__);
                    password.dispatchEvent(new Event('input', { bubbles: true }));
                    return true;
                })();
            "#
            .replace("__USER__", &user_js)
            .replace("__PASS__", &pass_js);

            let filled = page.evaluate(fill_js).await?.into_value::<bool>()?;
            eprintln!("[{TAG}] fields filled ({filled}): {:?}", t0.elapsed());
            if !filled {
                anyhow::bail!("login form fields missing when filling");
            }

            page.evaluate("document.querySelector(\"button[type='submit']\").click()")
                .await?;
            eprintln!("[{TAG}] submit clicked: {:?}", t0.elapsed());

            let logged_in = if OPEN_WEBUI_FAST_PASS {
                Self::wait_for_js(&page, "!!localStorage.getItem('token')", until).await
            } else {
                Self::wait_for_selector(&page, "#chat-input", until)
                    .await
                    .map(|_| ())
            };

            if let Err(e) = logged_in {
                dump_failure(&page, TAG).await;
                return Err(e);
            }
            eprintln!("[{TAG}] login confirmed: {:?}", t0.elapsed());

            anyhow::Ok(())
        };

        let result = tokio::time::timeout_at(outer, op).await;
        let _ = page.close().await;
        report(TAG, t0, result)
    }
}

// ---------------------------------------------------------------------------
// MySQL check (no browser involved)
// ---------------------------------------------------------------------------

pub async fn mysql_check(svc: &Service) -> bool {
    const TAG: &str = "mysql";
    let (username, password) = match svc.creds() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[{TAG}] {e}");
            return false;
        }
    };

    let t0 = Instant::now();
    let opts = OptsBuilder::default()
        .ip_or_hostname(svc.host.clone())
        .tcp_port(3306)
        .db_name(Some(MYSQL_DB))
        .user(Some(username))
        .pass(Some(password));

    let result = tokio::time::timeout(CHECK_TIMEOUT, async {
        let mut conn = Conn::new(opts).await?;
        let value: Option<i32> = conn.query_first("SELECT 1").await?;
        let _ = conn.disconnect().await; // ignore disconnect errors
        if value != Some(1) {
            anyhow::bail!("SELECT 1 returned {value:?}");
        }
        anyhow::Ok(())
    })
    .await;

    report(TAG, t0, result)
}
