# Security Policy

## Scope

APFS Explorer is a filesystem driver and read/write library. Security issues
in this project can have direct consequences for **data integrity and
availability** on APFS volumes. All reports are taken seriously.

In-scope issues include:

- Data corruption or silent data loss caused by a write-path bug.
- Incorrect free-space accounting leading to overwritten live data.
- Parsing bugs that cause incorrect data to be returned (confidentiality).
- Denial-of-service conditions triggered by crafted on-disk structures.
- Privilege escalation via the WinFsp driver interface.

Out-of-scope (non-goals of this project):

- FileVault / volume encryption (not implemented, not planned).
- Sealed volume verification.
- Fusion Drive or container-resize operations.

---

## Reporting a vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

### Preferred method - GitHub Security Advisories

1. Go to the repository on GitHub.
2. Click **Security** → **Advisories** → **Report a vulnerability**.
3. Fill in the form with a description, steps to reproduce, and impact
   assessment.
4. Your report will be visible only to maintainers until disclosure.

### Alternative - email

Send a PGP-encrypted message to the maintainer email listed in `Cargo.toml`.
If no PGP key is published, send plaintext to that address with
`[SECURITY]` in the subject line.

---

## Severity tiers

| Tier | Examples | Response SLA |
|------|----------|--------------|
| Critical | Silent data loss, volume corruption, data read from wrong inode | Triage 24 h, fix 14 days |
| High | Crash on malformed volume, write to wrong block range | Triage 48 h, fix 30 days |
| Medium | Incorrect metadata (timestamps, permissions) | Triage 7 days, fix 60 days |
| Low | Cosmetic errors, wrong error messages, availability (non-data) | Triage 14 days, fix next release |

---

## Response process

1. Maintainer acknowledges receipt within the triage window above.
2. Maintainer confirms scope and reproduces the issue.
3. A fix is developed on a private branch.
4. A coordinated disclosure date is agreed with the reporter (default: 90 days
   from report, shorter for critical issues).
5. A patch release is published and a GitHub Security Advisory is made public.
6. Reporter is credited in the advisory unless they request anonymity.

---

## Known limitations

- **No encryption support.** FileVault-encrypted volumes are not supported and
  there are no plans to add support. Do not report missing encryption as a
  security issue.
- **Read-only mode available.** If you are uncertain about write safety on a
  production volume, mount read-only using the `--read-only` flag.
- **Experimental software.** This project has not undergone a formal security
  audit. Use on irreplaceable data at your own risk; maintain backups.

---

## Supported versions

| Version | Supported |
|---------|-----------|
| 1.0.0-rc1 (current) | Yes |
| 0.9.x | Critical fixes only |
| < 0.9.0 | No |
