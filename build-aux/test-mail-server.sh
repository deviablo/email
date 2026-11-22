#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Starts a local GreenMail test server with its own certificate authority
# for the integration tests. Needs docker (or podman) and openssl.
# Usage: build-aux/test-mail-server.sh DIR
#        E_MAIL_TEST_CA=DIR/ca.der cargo test -p e-mail-core --test mail_server -- --ignored
set -eu
dir="$1"
mkdir -p "$dir"
cd "$dir"
engine=$(command -v docker || command -v podman)

openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=E-Mail Test CA" \
  -keyout ca.key -out ca.pem 2>/dev/null
openssl req -newkey rsa:2048 -nodes -subj "/CN=localhost" \
  -keyout server.key -out server.csr 2>/dev/null
printf 'subjectAltName=DNS:localhost\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 2 \
  -extfile server.ext -out server.pem 2>/dev/null
openssl pkcs12 -export -in server.pem -inkey server.key -certfile ca.pem \
  -name greenmail -passout pass:changeit -out greenmail.p12
openssl x509 -in ca.pem -outform der -out ca.der
chmod 644 greenmail.p12

"$engine" rm -f e-mail-greenmail >/dev/null 2>&1 || true
"$engine" run -d --name e-mail-greenmail \
  -p 3993:3993 -p 3465:3465 \
  -v "$PWD/greenmail.p12:/certs/greenmail.p12:ro" \
  -e GREENMAIL_OPTS="-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 \
-Dgreenmail.users=alice:secret@example.test,bob:secret@example.test,carol:secret@example.test,max.berger:secret@example.org -Dgreenmail.users.login=email \
-Dgreenmail.tls.keystore.file=/certs/greenmail.p12 \
-Dgreenmail.tls.keystore.password=changeit -Dgreenmail.tls.key.password=changeit" \
  docker.io/greenmail/standalone:2.0.1 >/dev/null

for _ in $(seq 1 60); do
  if openssl s_client -connect localhost:3993 -CAfile ca.pem -verify_return_error \
      </dev/null 2>/dev/null | grep -q "OK"; then
    echo "GreenMail is ready"
    exit 0
  fi
  sleep 1
done
echo "GreenMail did not start" >&2
"$engine" logs e-mail-greenmail >&2
exit 1
