---
applyTo: "**"
---
# Phased Commit --- Mandatory for Every Commit

**1 Stage** --- relevant files only; no artifacts, temp files, or unintentional changes.

**2 Review** --- adversarial diff check: correct intent, no logic errors or side effects, tests intact, accurate commit message. Anything wrong: unstage, fix, restart.

**3 Security Audit WARNING** --- scan every staged file:

| Category | Examples |
|---|---|
| Keys / tokens | sk-ant-, sk-or-, Bearer, Authorization: values |
| Passwords / secrets | Any credential-looking string |
| Hostnames / IPs | Internal names, LAN IPs, private domains |
| SSH paths | Machine-specific key paths |
| PII | Names, emails, phone numbers, user IDs |
| Env var values | Actual secret values (var **names** are OK) |

Any hit: remove it and restart from 1 before committing.

**4 Commit** --- present-tense message (`Add X`, `Fix Y`, `Update Z`). Commit locally.

**5 Push - EXPLICIT APPROVAL REQUIRED** --- always get approval before `git push`; report commit + concerns and wait for confirmation.

**6 PR Quality Loop** --- if this push resolves comments on an open PR, immediately request a fresh Copilot review. A new review catches anything the first pass missed or any regressions introduced by the fix. Do not skip this step.
