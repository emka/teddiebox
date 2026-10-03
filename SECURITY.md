# Security policy

## Reporting a vulnerability

Report privately. Do not open a public issue.

Use GitHub private vulnerability reporting: the **Security** tab of the
repository, then **Report a vulnerability**.

Include the firmware version, what you observed and how to reproduce it.

## Scope

In scope:

- the firmware running on the box
- the setup-mode portal
- the over-the-air update path
- TLS handling, including the CA the box trusts

Out of scope:

- vulnerabilities in teddyCloud, ESP-IDF or other upstream projects; report
  those upstream
- attacks that need physical access to the box or its card

## Known limits

- The update path has no code signing. The only boundary is that the update
  host comes from the card, never from the manifest.
- Setup mode serves plain HTTP on a public default passphrase.

The setup-mode risks are listed under Security in the
[README](README.md#setup-mode).
