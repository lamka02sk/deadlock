# Security policy

## Supported versions

Deadlock is pre-1.0. Only the latest release and the `master` branch receive security
fixes.

## Reporting a vulnerability

Please do not open a public issue. Report it privately through GitHub:

<https://github.com/lamka02sk/deadlock/security/advisories/new>

Include what you found, how to reproduce it, the affected version and the impact you
see. Expect an acknowledgement within a few days. Once a fix is released the advisory is
published and you are credited unless you prefer otherwise.

## Scope

Things Deadlock promises and that count as vulnerabilities when broken:

- **Database passwords** are kept in the OS keychain (through the `keyring` crate), keyed
  by project folder and connection id. They are never written to `.deadlock/project.toml`,
  `state.json` or the query history (history masks secrets).
- **SSH passwords and key passphrases** are kept in memory for the session only. SSH
  private keys are read from the file you point to, or taken from your SSH agent, and are
  never copied. Host keys are checked against `~/.ssh/known_hosts` and Deadlock's own
  known-hosts file; a changed key is refused.
- **Read-only connections** refuse statements that change data, in the client and, where
  the engine allows, on the server.
- Local Redis and database ports in `docker-compose.yml` bind to 127.0.0.1 and use
  throwaway development credentials. That setup is for tests, not production.

Out of scope: weaknesses in the databases you connect to, a compromised user account or
keychain, and the "Prefer" and "Require" TLS modes not verifying certificates (use
"Verify certificate" for that). Known gaps are listed in the README.

Findings in vendored upstream code (`vendor/`) that also exist upstream should be
reported to the upstream project as well.
