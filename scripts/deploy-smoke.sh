#!/usr/bin/env bash
# P9-T02: bring up deploy/docker-compose.prod.yml with a `*.localhost` test
# domain and check what a client sees through Caddy and coturn. Run by the
# `deploy smoke` CI job. Needs docker (with compose), curl, jq and openssl.
#
# This does NOT prove a public deployment: with a `*.localhost` domain Caddy
# uses its own local CA, so Let's Encrypt issuance, public DNS and
# reachability from the internet are not exercised.

set -euo pipefail
cd "$(dirname "$0")/../deploy"

export LOCAST_DOMAIN=locast.localhost
export LOCAST_CERT_ISSUER_DIR=local
# A throwaway secret for this run only; it is never printed.
LOCAST_TURN_SHARED_SECRET="$(openssl rand -hex 32)"
export LOCAST_TURN_SHARED_SECRET

CADDY_IMAGE=caddy:2.9
COTURN_IMAGE=coturn/coturn:4.6
ca="$(mktemp)"
failed=0

compose() { docker compose -f docker-compose.prod.yml "$@"; }
ok() { echo "ok   $*"; }
bad() {
    echo "FAIL $*"
    failed=1
}
cleanup() {
    if [ "$failed" != 0 ]; then
        compose logs --no-color --tail 80 || true
    fi
    compose down -v --remove-orphans >/dev/null 2>&1 || true
    rm -f "$ca"
}
trap cleanup EXIT

compose config -q && ok "compose file renders"
docker run --rm -e LOCAST_DOMAIN -v "$PWD/caddy/Caddyfile:/etc/caddy/Caddyfile:ro" \
    "$CADDY_IMAGE" caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile \
    && ok "Caddyfile validates"

compose up -d --build --wait --wait-timeout 600
ok "stack is up and every healthcheck passes"

compose cp caddy:/data/caddy/pki/authorities/local/root.crt "$ca"
url="https://$LOCAST_DOMAIN"
c() { curl --silent --show-error --cacert "$ca" --resolve "$LOCAST_DOMAIN:443:127.0.0.1" "$@"; }

body="$(c --fail "$url/health" || true)"
[ "$body" = '{"status":"ok"}' ] && ok "HTTPS /health through Caddy" || bad "HTTPS /health returned '$body'"

c -I "$url/health" | grep -qi '^strict-transport-security: max-age=31536000' \
    && ok "HSTS header is set" || bad "HSTS header missing"

code="$(curl -s -o /dev/null -w '%{http_code}' --resolve "$LOCAST_DOMAIN:80:127.0.0.1" "http://$LOCAST_DOMAIN/health" || true)"
case "$code" in 301 | 308) ok "plain HTTP redirects to HTTPS ($code)" ;; *) bad "plain HTTP returned $code, expected a redirect" ;; esac

# curl stops after the 101 response headers; the timeout ends the open stream.
code="$(c --http1.1 -o /dev/null -w '%{http_code}' --max-time 5 \
    -H 'Connection: Upgrade' -H 'Upgrade: websocket' -H 'Sec-WebSocket-Version: 13' \
    -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' "$url/ws" || true)"
[ "$code" = 101 ] && ok "WebSocket upgrade on /ws through Caddy (101)" || bad "WebSocket upgrade on /ws returned $code"

if c --fail "$url/turn" | jq -e --arg d "$LOCAST_DOMAIN" \
    '.urls | index("turns:\($d):5349?transport=tcp") != null and index("turn:\($d):3478?transport=udp") != null' >/dev/null; then
    ok "/turn lists the domain's TURN endpoints"
else
    bad "/turn does not list the expected TURN endpoints"
fi

code="$(c -o /dev/null -w '%{http_code}' "$url/metrics" || true)"
[ "$code" = 404 ] && ok "/metrics is not exposed publicly" || bad "/metrics returned $code"

if curl -s --max-time 3 http://127.0.0.1:8787/health >/dev/null 2>&1; then
    bad "server port 8787 is reachable on the host; it must only be reachable through Caddy"
else
    ok "server port 8787 is not published on the host"
fi

docker run --rm --network host "$COTURN_IMAGE" turnutils_stunclient -p 3478 127.0.0.1 >/dev/null \
    && ok "coturn answers STUN on 3478" || bad "coturn did not answer STUN on 3478"

if echo | openssl s_client -connect 127.0.0.1:5349 -servername "$LOCAST_DOMAIN" -CAfile "$ca" \
    -verify_return_error -verify_hostname "$LOCAST_DOMAIN" >/dev/null 2>&1; then
    ok "coturn serves turns: on 5349 with Caddy's certificate for $LOCAST_DOMAIN"
else
    bad "coturn TLS on 5349 does not present a valid certificate for $LOCAST_DOMAIN"
fi

if [ "$failed" != 0 ]; then
    echo "deploy smoke: FAILED"
    exit 1
fi
echo "deploy smoke: passed"
