# Security Policy

## Supported versions

| Version | Supported |
|---|---|
| 0.3.x   | Yes       |

Kynoptic is pre-1.0; only the latest 0.3.x release receives security
fixes. Please update to the latest version before reporting.

## Outbound network & offline operation

The only outbound call Kynoptic makes is a once-daily **version check**.
Recorded data never leaves the machine, and local surfaces (dashboard,
MCP server) bind to `127.0.0.1` only.

- **What the check fetches.** The check is a single `GET` of GitHub's
  entire **Releases list** (owner/repo `releases`), not a lightweight
  single-field lookup. As of writing the response is on the order of
  ~100 KB; it grows with the number of releases and attached assets, and
  the GitHub API can paginate. Found versions are shown in the tray menu
  and are never auto-installed.
- **Fully offline deployments.** The check honors the standard proxy
  environment variables. Point `HTTPS_PROXY` at an unreachable proxy to
  make the check fail with no outbound traffic at all (an `https://` URL
  consults `HTTPS_PROXY` / `https_proxy` only, not `HTTP_PROXY`).
  - The **tray** runs the check in the background and fails silently:
    with a dead proxy you simply never see an "update available" prompt.
  - Running `kynoptic update --check` **by hand is not silent**: it exits
    non-zero and prints the failed URL. That is the expected
    "no network" signal, not a fault.

## Reporting a vulnerability

Email <kynoptic@outlook.com> with:

- A description of the issue and its impact
- Steps to reproduce or a proof of concept
- The affected version (commit hash if building from source)

We will acknowledge reports within **72 hours** and keep the report
confidential until a fix is released. Please do not open a public
issue for security reports.

Kynoptic's threat model is local: the collector, dashboard, and MCP
server must never transmit recorded data off the machine, and local
surfaces (dashboard, MCP) must bind to 127.0.0.1 only. Reports about
violations of that boundary are especially welcome.
