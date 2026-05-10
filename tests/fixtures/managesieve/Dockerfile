# Minimal Dovecot + Pigeonhole image for ManageSieve integration
# tests. Listens on :4190 with STARTTLS using a fresh self-signed cert
# generated at build time. Single test user "testuser" / "testpass".
#
# Build: docker build -t rustymail-sieve-test tests/fixtures/managesieve
# Run:   docker run --rm -p 4190:4190 rustymail-sieve-test
# Or use docker-compose.yml in this directory.

FROM alpine:3.19

RUN apk add --no-cache \
        dovecot \
        dovecot-pigeonhole-plugin \
        openssl

# Self-signed cert for STARTTLS. The integration test connects with
# danger_accept_invalid_certs(true) so we don't need a real CA chain.
RUN mkdir -p /etc/ssl/dovecot && \
    openssl req -new -x509 -days 3650 -nodes \
        -out /etc/ssl/dovecot/cert.pem \
        -keyout /etc/ssl/dovecot/key.pem \
        -subj "/CN=localhost" 2>/dev/null && \
    chmod 600 /etc/ssl/dovecot/key.pem

# Test user. PLAIN scheme is fine because the connection itself is
# TLS-protected after STARTTLS.
RUN echo 'testuser:{PLAIN}testpass:1000:1000::/var/mail/testuser::' \
        > /etc/dovecot/users && \
    mkdir -p /var/mail/testuser && \
    chown -R 1000:1000 /var/mail/testuser

COPY dovecot.conf /etc/dovecot/dovecot.conf

# Sieve scripts directory, owned by the test user.
RUN mkdir -p /var/mail/testuser/sieve && \
    chown -R 1000:1000 /var/mail/testuser/sieve

EXPOSE 4190

CMD ["dovecot", "-F"]
