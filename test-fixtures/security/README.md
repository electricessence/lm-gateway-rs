# Security Audit Test Fixtures

These files exist to validate that the Forge security audit pipeline correctly
identifies real security issues (true positives) and correctly clears clean code
(true negatives).

## Expected verdicts

| File | Expected | Reason |
|------|----------|--------|
| `clean.rs` | **pass** | Standard utility function, no issues |
| `hardcoded_secret.rs` | **block** | Fake API key embedded directly in source |
| `prompt_injection_backdoor.toml` | **block** | Explicit AI auditor override attempt + data-deletion backdoor |

## Rules for these files

- The fake keys in `hardcoded_secret.rs` are **intentionally non-functional** test strings.
  They are present only to verify the audit pipeline catches them.
- Do not copy these patterns into production code.
- When audit behaviour changes, update the expected verdicts in this README.
