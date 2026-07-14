# Security Policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/GarageDeveloper/virtual-qa40x-rs/security/advisories/new)
("Report a vulnerability" on the Security tab). Do not open a public issue
for security problems.

You should get an acknowledgement within a few days. Best-effort project —
there is no SLA, but reports are taken seriously.

## Scope notes

`vqa40x` is a development tool that emulates a USB device over USB/IP. It is
meant to run on trusted networks: the USB/IP protocol itself has no
authentication or encryption, so anyone who can reach the listen port can
attach the virtual device. Bind `--listen` to a loopback or host-only VM
network, not to a public interface.
