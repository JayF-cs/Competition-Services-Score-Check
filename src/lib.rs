use async_trait::async_trait;
use chromiumoxide::browser::Browser;
use chromiumoxide::cdp::browser_protocol::browser::BrowserContextId;
use chromiumoxide::cdp::browser_protocol::page::NavigateParams;
use chromiumoxide::cdp::browser_protocol::target::{CreateBrowserContextParams, CreateTargetParams};
use chromiumoxide::Page;
use futures::StreamExt;
use mysql_async::{prelude::*, Conn, OptsBuilder};
use russh::client::{self, Handle};
use russh::ChannelMsg;
use russh_keys::key::PublicKey;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::Instant;

const CDP_HOST: &str = "http://127.0.0.1:9222";

const CHECK_TIMEOUT: Duration = Duration::from_millis(4000);

// web_app does login + dashboard relay wait + API + extra page loads, so it
// gets a longer budget. Make sure your scoring engine's own deadline allows it.
const WEB_APP_TIMEOUT: Duration = Duration::from_millis(9000);

const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(1500);

const DIAG_MARGIN: Duration = Duration::from_millis(400);

const POLL_INTERVAL: Duration = Duration::from_millis(50);

const OPEN_WEBUI_FAST_PASS: bool = true;

const MYSQL_DB: &str = "plinkcom";

// ---------------------------------------------------------------------------
// Web app baseline (known-good state). Matches the dashboard you pasted.
// ---------------------------------------------------------------------------

pub struct WebExpect {
    pub title_suffix: String,
    pub org_name: String,
    pub header_text: String,
    pub footer_text: String,
    pub banner: String,
    pub rocket_name: String,
    pub vehicle_id: String,
    pub destination_label: String,
    pub dest_x: i64,
    pub dest_y: i64,
    pub formation_mode: String,
    pub formation_center: Value,
    pub formation_radius: i64,
    pub control_points: Value,
    pub cycle_seconds: i64,
}

impl Default for WebExpect {
    fn default() -> Self {
        Self {
            title_suffix: "// TESTING".into(), // set to your real suffix
            org_name: "INTERNATIONAL HORSE PLINKO LEAGUE".into(),
            header_text: "LUNAR OPERATIONS DIVISION".into(),
            footer_text: "IHPL LUNAR OPERATIONS DIVISION".into(),
            banner: "IHPL LUNAR OPERATIONS // PROCEED TO MARE IMBRIUM".into(),
            rocket_name: "Equus I".into(),
            vehicle_id: "PLK-EQUUS-I-01".into(),
            destination_label: "Moon / Mare Imbrium".into(),
            dest_x: 880,
            dest_y: 230,
            formation_mode: "transit".into(),
            formation_center: json!({"x": 620, "y": 330}),
            formation_radius: 130,
            control_points: json!([{"x": 480, "y": 120}, {"x": 730, "y": 120}]),
            cycle_seconds: 1800,
        }
    }
}

/// Pages red team can flip to "Down for maintenance" (or a custom message).
/// The selector must exist ONLY on the healthy page. Never match the
/// maintenance text, because red team controls it.
const GATED_PAGES: &[(&str, &str)] = &[
    // The upload form itself: only present when the page is actually serving.
    ("attachments.php", "form[enctype='multipart/form-data'] input[type='file'][name='attachment']"),
    // The table header and search box exist whether or not the DB has rows,
    // so this checks the page is up, not what's in the table.
    ("cargo.php", "table.data-table thead"),
];

const DOM_JS: &str = r#"
(() => {
  const t = s => (document.querySelector(s)?.textContent || '').replace(/\s+/g, ' ').trim();
  return JSON.stringify({
    title:      document.title,
    org:        t('.topbar__identity .eyebrow'),
    header:     t('.topbar__title'),
    footer:     t('.footer__host'),
    banner:     t('[data-relay-banner]'),
    rocket:     t('.mission-hero__identity h1'),
    vehicle:    t('.mission-hero__identity span'),
    dest:       t('[data-destination]'),
    relay:      t('[data-relay-status]'),
    render:     t('[data-render-status]'),
    has_scene:  !!document.querySelector('#orbital3d'),
    has_canvas: !!document.querySelector('.orbital3d-canvas canvas')
  });
})()
"#;

// Synchronous XHR so we don't depend on how Page::evaluate handles promises.
// It reuses the logged-in session cookie.
const API_JS: &str = r#"
(() => {
  try {
    const x = new XMLHttpRequest();
    x.open('GET', '/api/mission-map.php?mission_id=1', false);
    x.send();
    let body = null;
    try { body = JSON.parse(x.responseText); } catch (e) {}
    return JSON.stringify({status: x.status, body: body});
  } catch (e) {
    return JSON.stringify({status: 0, body: null, error: String(e)});
  }
})()
"#;

const RELAY_SETTLED_JS: &str = r#"
(() => {
  const t = (document.querySelector('[data-relay-status]')?.textContent || '').trim();
  return t !== '' && !t.includes('ACQUIRING');
})()
"#;

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

struct Client;

#[async_trait]
impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[allow(dead_code)]
async fn debug_shot(page: &Page, name: &str) {
    use chromiumoxide::page::ScreenshotParams;
    if let Ok(bytes) = page
        .screenshot(ScreenshotParams::builder().full_page(true).build())
        .await
    {
        let _ = tokio::fs::write(format!("/tmp/debug_{name}.png"), bytes).await;
    }
}

async fn dump_failure(page: &Page, tag: &str) {
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

/// Run JS that returns a JSON string and parse it.
async fn eval_json(page: &Page, js: &str) -> anyhow::Result<Value> {
    let s = page.evaluate(js).await?.into_value::<String>()?;
    Ok(serde_json::from_str(&s)?)
}

/// `fails` are scoring; `warns` are logged only.
#[derive(Default)]
struct Findings {
    fails: Vec<String>,
    warns: Vec<String>,
}

impl Findings {
    fn check(&mut self, name: &str, ok: bool, detail: impl FnOnce() -> String) {
        if !ok {
            self.fails.push(format!("{name}: {}", detail()));
        }
    }
    fn soft(&mut self, name: &str, ok: bool, detail: impl FnOnce() -> String) {
        if !ok {
            self.warns.push(format!("{name}: {}", detail()));
        }
    }
}

// ---------------------------------------------------------------------------
// Browser-based checks
// ---------------------------------------------------------------------------

pub struct ScoreChecker {
    browser: Browser,
    ctx: BrowserContextId,
    expect: WebExpect,
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

        Ok(Self {
            browser,
            ctx,
            expect: WebExpect::default(),
        })
    }

    pub async fn close(self) {
        let _ = self.browser.dispose_browser_context(self.ctx).await;
    }

    async fn new_page(&self, url: &str) -> anyhow::Result<Page> {
        let params = CreateTargetParams::builder()
            .url(url)
            .browser_context_id(self.ctx.clone())
            .build()
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(self.browser.new_page(params).await?)
    }

    async fn navigate(page: &Page, url: &str) -> anyhow::Result<()> {
        let resp = page.execute(NavigateParams::new(url)).await?;
        if let Some(err) = &resp.result.error_text {
            anyhow::bail!("navigation to {url} failed: {err}");
        }
        Ok(())
    }

    async fn wait_for_selector(
        page: &Page,
        selector: &str,
        until: Instant,
    ) -> anyhow::Result<chromiumoxide::element::Element> {
        loop {
            let now = Instant::now();
            if now >= until {
                anyhow::bail!("timed out waiting for {selector}");
            }
            let attempt = std::cmp::min(until, now + ATTEMPT_TIMEOUT);
            if let Ok(Ok(el)) = tokio::time::timeout_at(attempt, page.find_element(selector)).await {
                return Ok(el);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn wait_for_js(page: &Page, expr: &str, until: Instant) -> anyhow::Result<()> {
        loop {
            let now = Instant::now();
            if now >= until {
                anyhow::bail!("timed out waiting for js: {expr}");
            }
            let attempt = std::cmp::min(until, now + ATTEMPT_TIMEOUT);
            if let Ok(Ok(r)) = tokio::time::timeout_at(attempt, page.evaluate(expr)).await {
                if r.into_value::<bool>().unwrap_or(false) {
                    return Ok(());
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn wait_for_present(page: &Page, selector: &str, until: Instant) -> anyhow::Result<()> {
        let sel = serde_json::to_string(selector)?;
        let expr = format!("!!document.querySelector({sel})");
        Self::wait_for_js(page, &expr, until).await
    }

    // -----------------------------------------------------------------------
    // web_app: login, then tampering checks
    // -----------------------------------------------------------------------

    async fn tampering_checks(
        &self,
        page: &Page,
        host: &str,
        until: Instant,
    ) -> anyhow::Result<Findings> {
        let exp = &self.expect;
        let mut f = Findings::default();

        // ---- Dashboard DOM (already logged in) ----
        // Give the relay poll a short window to overwrite the server-rendered
        // banner. If it never settles, skip the DOM banner check (the API
        // check below covers it) instead of failing the whole run.
        let relay_until = std::cmp::min(until, Instant::now() + Duration::from_secs(5));
        let relay_ok = Self::wait_for_js(page, RELAY_SETTLED_JS, relay_until).await.is_ok();
        f.soft("relay_settled", relay_ok, || "relay status never left ACQUIRING".into());

        let dom = eval_json(page, DOM_JS).await?;
        let s = |k: &str| dom[k].as_str().unwrap_or("").to_string();

        f.check("title_suffix", s("title").ends_with(&exp.title_suffix), || s("title"));
        f.check("org_name", s("org") == exp.org_name, || s("org"));
        f.check("header_text", s("header") == exp.header_text, || s("header"));
        f.check("footer_text", s("footer").contains(&exp.footer_text), || s("footer"));
        if relay_ok {
            f.check("banner_dom", s("banner").contains(&exp.banner), || s("banner"));
        }
        f.check("rocket_dom", s("rocket") == exp.rocket_name, || s("rocket"));
        f.check("dest_dom", s("dest") == exp.destination_label, || s("dest"));
        f.check("vehicle_line", s("vehicle").contains(&exp.destination_label), || s("vehicle"));

        // ---- 3D map availability (lives on the dashboard) ----
        f.check("map_present", dom["has_scene"] == true, || "#orbital3d missing".into());
        // Soft until you've confirmed WebGL renders in your headless Chrome.
        // If it does, change this to f.check.
        f.soft("map_canvas", dom["has_canvas"] == true, || {
            format!("no canvas; render status: {}", s("render"))
        });

        // ---- API state (flight pattern, destination, clock, fleet) ----
        let api = eval_json(page, API_JS).await?;
        f.check("api_status", api["status"] == 200, || api["status"].to_string());
        let cfg = &api["body"]["configuration"];

        f.check("banner_api", cfg["banner"].as_str().unwrap_or("").contains(&exp.banner),
            || cfg["banner"].to_string());
        f.check("destination_label", cfg["destination"]["label"] == exp.destination_label.as_str(),
            || cfg["destination"].to_string());
        f.check("destination_xy",
            cfg["destination"]["x"] == exp.dest_x && cfg["destination"]["y"] == exp.dest_y,
            || cfg["destination"].to_string());

        // Flight pattern
        f.check("formation_mode", cfg["formation"]["mode"] == exp.formation_mode.as_str(),
            || cfg["formation"].to_string());
        f.check("formation_center", cfg["formation"]["center"] == exp.formation_center,
            || cfg["formation"]["center"].to_string());
        f.check("formation_radius", cfg["formation"]["radius"] == exp.formation_radius,
            || cfg["formation"]["radius"].to_string());
        f.check("control_points", cfg["control_points"] == exp.control_points,
            || cfg["control_points"].to_string());
        f.check("cycle_seconds", cfg["cycle_seconds"] == exp.cycle_seconds,
            || cfg["cycle_seconds"].to_string());

        // Injected scene content (assumes baseline has none)
        for k in ["objects", "phantoms", "image_objects"] {
            f.check(k, cfg[k].as_array().map_or(false, |a| a.is_empty()), || cfg[k].to_string());
        }

        // Rockets and fleet overrides
        let rockets = api["body"]["rockets"].as_array().cloned().unwrap_or_default();
        f.check("rocket_name_api", rockets.iter().any(|r| r["name"] == exp.rocket_name.as_str()),
            || format!("{rockets:?}"));
        f.check("vehicle_id",
            rockets.iter().any(|r| r["vehicle_identifier"] == exp.vehicle_id.as_str()),
            || format!("{rockets:?}"));
        for (id, ov) in cfg["fleet_overrides"].as_object().into_iter().flatten() {
            f.check("fleet_override_name", ov["name"] == exp.rocket_name.as_str(),
                || format!("rocket {id}: {ov}"));
            f.check("fleet_override_dest", ov["destination"] == exp.destination_label.as_str(),
                || format!("rocket {id}: {ov}"));
        }

        // ---- Clock (WIP: non-scoring warnings only) ----
        f.soft("clock_offset", cfg["clock_offset_seconds"] == 0,
            || cfg["clock_offset_seconds"].to_string());
        f.soft("clock_rate", cfg["clock_rate"] == 1, || cfg["clock_rate"].to_string());

        // ---- Service availability (navigates away, so keep this last) ----
        for (path, healthy_sel) in GATED_PAGES {
            Self::navigate(page, &format!("http://{host}/{path}")).await?;
            let up = Self::wait_for_selector(page, healthy_sel, until).await.is_ok();
            f.check(&format!("availability:{path}"), up, || "maintenance/missing".into());
        }

        Ok(f)
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

        let outer = t0 + WEB_APP_TIMEOUT;
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

            // Login + post-login destination
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

            // Tampering checks
            let findings = self.tampering_checks(&page, &svc.host, until).await?;
            for w in &findings.warns {
                eprintln!("[{TAG}] warn: {w}");
            }
            if !findings.fails.is_empty() {
                dump_failure(&page, TAG).await;
                anyhow::bail!("tampering detected: {}", findings.fails.join("; "));
            }

            anyhow::Ok(())
        };

        let result = tokio::time::timeout_at(outer, op).await;

        let _ = page.close().await;
        report(TAG, t0, result)
    }

    // -----------------------------------------------------------------------
    // jenkins
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // open_webui
    // -----------------------------------------------------------------------

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

            if let Err(e) = Self::wait_for_present(&page, "input[type='email']", until).await {
                dump_failure(&page, TAG).await;
                return Err(e);
            }
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
                Self::wait_for_present(&page, "#chat-input", until).await
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

// ---------------------------------------------------------------------------
// SSH check
// ---------------------------------------------------------------------------

pub async fn ssh_check(svc: &Service) -> bool {
    const TAG: &str = "ssh";
    let (username, password) = match svc.creds() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[{TAG}] {e}");
            return false;
        }
    };

    let t0 = Instant::now();
    let result = tokio::time::timeout(CHECK_TIMEOUT, async {
        let config = std::sync::Arc::new(client::Config::default());
        let mut session = client::connect(config, (svc.host.as_str(), 22), Client).await?;

        let auth = session.authenticate_password(username, password).await?;

        if !auth {
            anyhow::bail!("password rejected");
        }

        let mut channel = session.channel_open_session().await?;
        channel.exec(true, "echo ok").await?;

        let mut out = Vec::new();
        let mut code = None;
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { ref data } => out.extend_from_slice(data),
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                _ => {}
            }
        }

        if code != Some(0) {
            anyhow::bail!("remote command exit status {code:?}");
        }

        let _ = session
            .disconnect(russh::Disconnect::ByApplication, "", "")
            .await;

        anyhow::Ok(())
    })
    .await;

    report(TAG, t0, result)
}
