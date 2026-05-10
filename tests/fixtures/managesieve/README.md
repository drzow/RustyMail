# ManageSieve integration fixture

Dovecot + Pigeonhole in a one-shot Docker container. Used by the
`integration_sieve` test target to live-test rustymail's sieve client
against a real ManageSieve server (TLS handshake, PLAIN auth, full
PUT/ACTIVATE/LIST/GET/CHECK/DELETE round trip).

## Run

```bash
docker compose -f tests/fixtures/managesieve/docker-compose.yml up -d --build
cargo test --features integration-sieve --test integration_sieve -- --nocapture
docker compose -f tests/fixtures/managesieve/docker-compose.yml down
```

## What's inside

| File              | Role                                                          |
|-------------------|---------------------------------------------------------------|
| `Dockerfile`      | Alpine 3.19 + `dovecot` + `dovecot-pigeonhole-plugin`         |
| `dovecot.conf`    | ManageSieve on :4190, STARTTLS required, PLAIN auth allowed   |
| `docker-compose.yml` | Brings up the container and exposes :4190 on localhost     |

The container generates a fresh self-signed TLS cert at build time
(`CN=localhost`, valid 3650 days). The integration test connects with
`connect_starttls_insecure` so cert validation is bypassed — that
function is feature-gated behind `integration-sieve` and won't compile
in production builds.

## Test credentials

Hardcoded in the image — change the Dockerfile to rotate.

```
user:     testuser
password: testpass
host:     127.0.0.1
port:     4190
```

## Why a self-signed cert?

The alternative is generating a real CA chain (mkcert or a sibling
container with cfssl) and trusting it from the test process. That
adds ~100 lines of fixture and a host-side trust-store mutation that
some CI envs disallow. Self-signed + a feature-gated insecure connect
helper is a smaller surface for the same coverage.
