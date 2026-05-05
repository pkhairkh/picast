# Security Policy

## Reporting a Vulnerability

**Do not report security vulnerabilities through public GitHub issues.**

Instead, please report them through [GitHub Security Advisories](https://github.com/pkhairkh/picast/security/advisories/new).

Include as much of the following information as possible:

- Type of vulnerability (e.g., privilege escalation, information disclosure, RCE)
- Full paths of source files related to the vulnerability
- Step-by-step reproduction instructions
- Potential impact on a deployed PiCast appliance
- Any suggested mitigations or fixes

You should receive a response within **48 hours**. If you do not, please follow up
via the same channel to confirm receipt.

## Threat Model

PiCast is a Raspberry Pi 4B+ media casting appliance. The following summarizes
the primary security considerations that inform our design and review process:

| Threat Vector | Mitigation |
|---|---|
| **Network surveillance** | All outbound traffic is routed through Tor with stream isolation per session, preventing correlation of user activity. |
| **DRM attack surface** | No DRM modules are included or loaded. Playback uses GStreamer with open codecs only, eliminating DRM-related privilege escalation paths. |
| **Subprocess compromise** | Media playback and auxiliary processes run in isolated subprocesses with minimal capabilities. A compromised child process cannot access the parent's memory or escalate to the host. |
| **Supply chain** | `cargo-deny` enforces license allowlists and bans `openssl` / `curl` in favor of `rustls` / `hyper`, reducing C-library attack surface. |

## Supported Versions

Only the **latest release** receives security updates. There are no back-patched
LTS branches.

| Version | Supported |
|---|---|
| Latest | ✅ |
| Older | ❌ |

## Response Timeline

| Severity | Acknowledgement | Fix |
|---|---|---|
| Critical | ≤ 48 hours | ≤ 7 days |
| High | ≤ 48 hours | ≤ 14 days |
| Medium / Low | ≤ 72 hours | Next release |

## Disclosure Policy

Once a fix is released we will publish a GitHub Security Advisory with full
details. We ask that reporters allow us up to **90 days** to address a
vulnerability before public disclosure, in line with coordinated disclosure
best practices.
