# Security Policy

## Supported versions

| Version | Supported |
|---|---|
| 0.1.x   | Yes       |

Kynoptic is pre-1.0; only the latest 0.1.x release receives security
fixes. Please update to the latest version before reporting.

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
