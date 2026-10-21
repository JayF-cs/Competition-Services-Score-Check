# scores_check

Service checks for the PLINKCOM (International Horse Plinko League, Lunar Operations) scoring engine. Each check is a small standalone binary that takes a JSON config file, tests one service, and reports pass/fail through its exit code. Written in Rust on top of `tokio`.

## Checks

| Binary | Service | Port | What it verifies |
|---|---|---|---|
| `ssh` | SSH | 22 | Password auth succeeds and `echo ok` exits 0 |
| `mysql` | MySQL | 3306 | Login to the `plinkcom` database and `SELECT 1` returns `1` |
| `jenkins` | Jenkins | 8080 | Browser login via `/login`, then the Jenkins UI loads |
| `open_webui` | Open WebUI | 80 | Browser login via `/auth`, then a session token is set |
| `web_app` | PLINKCOM dashboard | 80 | Login plus tamper detection (see below) |
| `rdp` | RDP | 3389 | NLA/CredSSP (NTLM) authentication succeeds |

### How the browser checks work

`jenkins`, `open_webui`, and `web_app` drive a real Chrome instance over the Chrome DevTools Protocol (CDP) using `chromiumoxide`. Each run connects to Chrome, creates an isolated browser context (so sessions and cookies never leak between runs), and disposes of it on exit. Pages are polled every 50ms, so checks finish as soon as the page is ready rather than sleeping for a fixed time.

### `web_app` tamper detection

After logging in, `web_app` compares the live dashboard against a known-good baseline (`WebExpect` in `src/lib.rs`) to catch changes red team can make through the admin panel.

**Scoring checks (any mismatch fails the service):**

- Page title suffix, organization name, header text, and footer text
- Post-login landing page (`/dashboard.php`)
- Mission banner, rocket name, vehicle ID, and destination (label and coordinates)
- Flight pattern: formation mode, center, radius, control points, and cycle length
- No injected scene content (`objects`, `phantoms`, `image_objects`) and no fleet overrides that rename the rocket or reroute it
- `/api/mission-map.php` returns 200
- Presence of the 3D map container
- Availability of `attachments.php` (upload form) and `cargo.php` (data table header)

The availability checks look for an element that exists **only on the healthy page**, never for the maintenance text, because red team controls that text. They check that the page is up, not what the database holds in it.

**Warning-only checks (logged, not scored):**

- 3D map `<canvas>` rendering (headless Chrome may not provide WebGL)
- Relay status settling out of `ACQUIRING`
- Clock offset and clock rate (still work in progress)

## Requirements

- Rust (stable, 2021 edition or newer)
- Chrome or Chromium with remote debugging enabled on `127.0.0.1:9222` (browser-based checks only)
- Network access from the scoring host to each service

Start Chrome before running any browser-based check:

```bash
chromium --headless=new --remote-debugging-port=9222 --no-sandbox
```

## Building

```bash
cargo build --release
```

Binaries are written to `target/release/` as `ssh`, `mysql`, `jenkins`, `open_webui`, `web_app`, and `rdp`.

## Configuration

Every binary takes a single argument: the path to a JSON config file.

```bash
./target/release/ssh config.json
```

### Standard config

Used by `ssh`, `mysql`, `jenkins`, `open_webui`, and `web_app`:

```json
{
  "host": "10.0.1.10",
  "team_num": 1,
  "user": {
    "username": "alice",
    "password": "hunter2"
  }
}
```

| Field | Type | Notes |
|---|---|---|
| `host` | string | Target hostname or IP |
| `team_num` | integer | Team number |
| `user` | object, optional | Required by every check, which fail if it is missing |

### RDP config

`rdp` uses its own flat format:

```json
{
  "host": "10.0.1.10",
  "port": 3389,
  "domain": "CORP",
  "username": "alice",
  "password": "hunter2",
  "timeout_secs": 10
}
```

`port` (default `3389`), `domain`, and `timeout_secs` (default `10`) are optional.

## Exit codes and output

| Binary | Exit code | Meaning |
|---|---|---|
| All except `rdp` | `0` | Check passed |
| All except `rdp` | `1` | Check failed, or bad config or setup |
| `rdp` | `0` | Prints `ACCESS_GRANTED` |
| `rdp` | `1` | Prints `ACCESS_DENIED` |
| `rdp` | `2` | Error or timeout (not a credential verdict) |

Diagnostics go to **stderr** in the form `[tag] OK (123ms)` or `[tag] FAILED: reason`. On browser-check failures, the current URL and the first 500 characters of the page HTML are also logged.

## Timeouts

| Check | Budget |
|---|---|
| `ssh`, `mysql`, `jenkins`, `open_webui` | 4s |
| `web_app` | 9s (login, relay wait, API call, extra page loads) |
| `rdp` | `timeout_secs` (default 10s) |

Make sure your scoring engine's own deadline is longer than these, especially for `web_app`.

## Tuning

Constants at the top of `src/lib.rs`:

- `CHECK_TIMEOUT` / `WEB_APP_TIMEOUT`: per-check time budgets
- `OPEN_WEBUI_FAST_PASS`: when `true`, Open WebUI passes once the auth token is set instead of waiting for the chat UI to render
- `MYSQL_DB`: database name used by the MySQL check
- `CDP_HOST`: Chrome DevTools endpoint
- `WebExpect::default()`: the `web_app` baseline. Set `title_suffix` to your real suffix before the competition; it ships as `// TESTING`
- `GATED_PAGES`: pages and healthy-state selectors used for availability checks

## Security notes

- SSH accepts any host key without verification.
- RDP accepts any TLS certificate and hostname, since RDP servers are typically self-signed.
- Credentials are read from plain JSON files, so lock down their permissions (`chmod 600`).

These are acceptable for an isolated competition network, but not for general use.

## Project layout

```
.
├── Cargo.toml
└── src
    ├── lib.rs            # Service config, ScoreChecker, web_app/jenkins/open_webui, mysql_check, ssh_check
    └── bin
        ├── ssh.rs
        ├── mysql.rs
        ├── jenkins.rs
        ├── open_webui.rs
        ├── web_app.rs
        └── rdp.rs        # standalone: raw X.224 + TLS + CredSSP via IronRDP
```
