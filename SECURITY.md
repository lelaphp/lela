# Security Policy

Lela is a PHP engine written in Rust. Because Lela implements PHP semantics,
a vulnerability in Lela may also affect PHP itself (php-src), and vice versa.
This policy explains how we handle both cases.

## Reporting a vulnerability

**Please do not report security issues in public issues, pull requests,
discussions or chat channels.**

Report privately through GitHub Private Vulnerability Reporting:
https://github.com/lelaphp/lela/security/advisories/new

Please include:

- affected Lela version(s) or commit (`lela -v` prints the version)
- a description of the issue and its impact
- a minimal reproducer (PHP script, input, or test case)
- whether you believe php-src is also affected, and whether you have
  already reported it to the PHP project

If you are unsure whether something is a security issue, report it privately
anyway. We would rather triage a false alarm than see a real issue disclosed
by accident.

## What to expect

| Step | Target |
|---|---|
| Acknowledgement of your report | within 3 working days |
| Initial triage and classification | within 10 working days |
| Fix and coordinated release | depends on classification (see below) |

You will be kept informed throughout and credited in the advisory unless you
prefer to remain anonymous.

## How we classify and handle issues

Every suspected security issue starts in a **private security advisory**.
Nothing is discussed, committed or merged publicly until it has been
classified and the relevant release has shipped.

### 1. Affects Lela only

The issue is specific to Lela (for example, in our Rust implementation, our
runtime, or behaviour that differs from PHP).

- We develop and test the fix in a private fork.
- We ship a patch release, then publish the advisory and merge the fix.
- Target: as fast as safely possible, typically within 14 days for high and
  critical severity.

### 2. Affects php-src only

During triage we sometimes find issues in php-src that do not affect Lela
(for example, memory-safety bugs in C code that Rust's guarantees rule out).

- We report the issue privately to the PHP Security Team via
  https://github.com/php/php-src/security/advisories/new and follow
  [PHP's vulnerability disclosure policy](https://github.com/php/policies/blob/main/security-classification.rst).
- We do not publish details until PHP has released a fix.
- After PHP's disclosure, we may add a regression test to Lela.

### 3. Affects both Lela and php-src

- We report the issue privately to the PHP Security Team first.
- Where helpful, we share a reference patch or analysis to support the PHP
  team. This is offered as an example only; the PHP team decides on their own
  fix.
- We prepare our own fix privately and **coordinate the public release date
  with the PHP Security Team**, aiming for simultaneous disclosure.
- Until that date, the fix is shared only with participants in our
  pre-notification programme (see below), under embargo.
- If no coordinated date can be agreed, we follow an industry-standard
  maximum embargo of 90 days from the initial report to PHP. If an issue is
  being actively exploited, we will work with the PHP team towards a much
  shorter timeline.

## Pre-notification programme

Organisations running Lela in production can apply to receive security fixes
and mitigation guidance before public disclosure.

Participants agree to:

- keep embargoed information confidential and share it only with staff who
  need it to deploy the fix
- not publish patches, diffs, binaries or details before the public
  disclosure date
- notify us immediately if they suspect a leak

If embargoed information leaks or an issue is exploited in the wild, we may
release fixes immediately and will notify the PHP Security Team at the same
time.

To apply, open a private report at
https://github.com/lelaphp/lela/security/advisories/new titled
"Pre-notification application", with your organisation, how you use Lela,
and a security contact.

## Memory safety, and where it ends

Lela's engine is written in Rust, which rules out whole classes of
memory-safety bugs rather than patching them one by one. `unsafe` code is
allowed only in a few named modules (the heap, the process layer, the
compiled tier's entry, and the bindings to C libraries), and each `unsafe`
block is one operation with a written justification.

That guarantee stops at C. Lela compiles some C from php-src (timelib,
libmbfl, lexbor, uriparser and libmagic among it) and links system libraries
such as OpenSSL, PCRE2, libxml2, ICU, curl and libsodium. A bug in any of
them can affect Lela; report it here, and we will route it as above or to
the library's own maintainers.

Lela's behaviour, its defaults included, follows the PHP release it is
tested against. Every deliberate difference from PHP is listed in
[docs/divergence.md](docs/divergence.md).

## Supported versions

| Version | Supported |
|---|---|
| 0.1.x | ✅ |
| older | ❌ |

Security fixes are released for the latest minor version only during the
pre-1.0 phase.

## Advisories and CVEs

Published advisories are listed at
https://github.com/lelaphp/lela/security/advisories.
CVE identifiers are requested through GitHub for Lela-specific issues. Issues
that also affect PHP use the CVE assigned by the PHP project.

## Safe harbour

We will not pursue or support legal action against researchers who make a
good-faith effort to follow this policy, avoid privacy violations and service
disruption, and give us reasonable time to respond before disclosure.
