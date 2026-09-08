# Security

Thank you for looking. Flash holds people's study material and their
credentials, so reports are welcome and taken seriously.

## Reporting a vulnerability

Please report privately through GitHub's **Report a vulnerability** button
on this repository's Security tab. It opens a private advisory that only
the maintainers can see, keeps a record of the conversation, and lets us
credit you when the fix ships if you would like that.

Please do not open a public issue for a security problem, and do not test
against the hosted service or against data that is not your own. A local
build (`docker compose up`, or `cargo run -p flash-server`) gives you a
server of your own to probe.

## What to expect

- An acknowledgement within a few days.
- A fix before any public disclosure, with a release note that describes
  the problem once it is fixed.
- Credit in the release note if you want it, under whatever name you give.

## Scope

In scope: everything in this repository, and the behaviour of a server
built from it. That includes authentication and sessions, the OAuth and
MCP surfaces, the JSON API, the importers and exporters, media handling,
and any way one account could reach another's data.

Out of scope: denial of service by volume alone, reports that require a
compromised operator machine or a malicious administrator, and findings
in third-party services a self-hoster may put in front of the server.

## Supported versions

The latest release on `main`. Older releases are not patched separately;
please upgrade.

## What is already in place

The design notes in the source describe the guarantees the test suite
enforces on every push: a user id that cannot be forged, every per-user
query bound to that id, every route probed as a stranger and as a
cross-site post, every mutation probed past its rate budget, every
request field bound to a named size, every heavy operation behind a
permit, rich HTML stored only as the sanitizer's own type, no lock in
the process that a panic can poison, parsers that cap what they build
before they build it, destructive MCP tools that take two calls, and
error messages that never carry a library's words to a client. If you
find a way around one of those, that is exactly the report we want.
