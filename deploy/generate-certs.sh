#!/usr/bin/env bash
# PiCast TLS Certificate Generator
#
# Generates a private CA + server certificate for HTTPS/WSS on the
# PiCast receiver. Using a CA chain (instead of a bare self-signed
# server cert) allows the CA to be imported into the browser's trust
# store ONCE — after that, every server cert signed by that CA is
# trusted automatically, regardless of hostname or port.
#
# This avoids the per-host:port exception dance that Firefox requires
# for bare self-signed certs (you'd need separate exceptions for
# picast.local:8585, picast.local:8586, 192.168.x.x:8585, etc.).
#
# Usage:
#   sudo ./generate-certs.sh                    # auto-detect IP
#   sudo ./generate-certs.sh 192.168.1.100      # specify IP
#
# Generated files in /etc/picast/tls/:
#   ca.pem           — PiCast CA certificate (import this into browsers)
#   ca-key.pem       — PiCast CA private key (keep secret!)
#   picast.pem       — Server certificate (PEM, used by picast-server)
#   picast-key.pem   — Server private key (PEM, used by picast-server)
#
# After generating:
#   1. sudo systemctl restart picast
#   2. Import ca.pem into your browser's trusted root CAs
#   3. The PiCast extension can now connect via HTTPS/WSS

set -euo pipefail

CERT_DIR="/etc/picast/tls"
CA_CERT="${CERT_DIR}/ca.pem"
CA_KEY="${CERT_DIR}/ca-key.pem"
SRV_CERT="${CERT_DIR}/picast.pem"
SRV_KEY="${CERT_DIR}/picast-key.pem"
VALIDITY_DAYS=3650   # 10 years

# Determine the Pi's LAN IP
if [ $# -ge 1 ]; then
    PI_IP="$1"
else
    PI_IP=$(hostname -I | awk '{print $1}')
    if [ -z "$PI_IP" ]; then
        echo "ERROR: Cannot determine Pi IP address. Pass it as argument:" >&2
        echo "  sudo $0 192.168.1.100" >&2
        exit 1
    fi
fi

echo "PiCast TLS Certificate Generator"
echo "================================"
echo "  mDNS name : picast.local"
echo "  LAN IP    : ${PI_IP}"
echo "  Output dir: ${CERT_DIR}"
echo "  Validity  : ${VALIDITY_DAYS} days"
echo

# Create output directory
mkdir -p "${CERT_DIR}"

# ── Step 1: Generate the PiCast CA ──────────────────────────────────
#
# This CA certificate will be imported into browsers as a trusted root.
# Once trusted, any server cert it signs is automatically accepted.

if [ -f "${CA_CERT}" ] && [ -f "${CA_KEY}" ]; then
    echo "CA certificate already exists — reusing ${CA_CERT}"
    echo "  (delete ${CA_CERT} and ${CA_KEY} to regenerate)"
else
    echo "Generating PiCast CA certificate..."
    openssl req -x509 -newkey rsa:4096 \
        -keyout "${CA_KEY}" \
        -out "${CA_CERT}" \
        -days "${VALIDITY_DAYS}" \
        -nodes \
        -subj "/O=PiCast/CN=PiCast Local CA" \
        -addext "basicConstraints = critical, CA:TRUE" \
        -addext "keyUsage = critical, keyCertSign, cRLSign" \
        -addext "subjectKeyIdentifier = hash"

    echo "  CA certificate: ${CA_CERT}"
    echo "  CA private key: ${CA_KEY}"
fi
echo

# ── Step 2: Generate the server CSR ─────────────────────────────────
#
# The server cert is signed by the PiCast CA (not self-signed).
# It includes SANs for all hostnames and IPs that clients will use.

echo "Generating server certificate signing request..."

# Create a temporary OpenSSL config for the CSR extensions
CSR_CONFIG=$(mktemp)
cat > "${CSR_CONFIG}" <<EOF
[v3_req]
subjectAltName      = DNS:picast.local, DNS:localhost, IP:${PI_IP}, IP:127.0.0.1
basicConstraints    = CA:FALSE
keyUsage            = critical, digitalSignature, keyEncipherment
extendedKeyUsage    = serverAuth
subjectKeyIdentifier = hash
authorityKeyIdentifier = keyid,issuer
EOF

openssl req -new -newkey rsa:2048 \
    -keyout "${SRV_KEY}" \
    -out "${CERT_DIR}/picast.csr" \
    -nodes \
    -subj "/O=PiCast/CN=picast.local"

# ── Step 3: Sign the server cert with the CA ────────────────────────

echo "Signing server certificate with PiCast CA..."
openssl x509 -req \
    -in "${CERT_DIR}/picast.csr" \
    -CA "${CA_CERT}" \
    -CAkey "${CA_KEY}" \
    -CAcreateserial \
    -out "${SRV_CERT}" \
    -days "${VALIDITY_DAYS}" \
    -extfile "${CSR_CONFIG}" \
    -extensions v3_req

# Clean up temp files
rm -f "${CSR_CONFIG}" "${CERT_DIR}/picast.csr" "${CERT_DIR}/ca.srl"

# ── Set permissions ─────────────────────────────────────────────────

chown root:picast "${CA_CERT}" "${SRV_CERT}" "${SRV_KEY}" 2>/dev/null || \
    chown root:root "${CA_CERT}" "${SRV_CERT}" "${SRV_KEY}"
chown root:picast "${CA_KEY}" 2>/dev/null || chown root:root "${CA_KEY}"

chmod 644 "${CA_CERT}" "${SRV_CERT}"
chmod 640 "${CA_KEY}" "${SRV_KEY}"

echo
echo "Certificates generated successfully:"
echo "  Server cert : ${SRV_CERT}"
echo "  Server key  : ${SRV_KEY}"
echo "  CA cert     : ${CA_CERT}  (import this into browsers)"
echo "  CA key      : ${CA_KEY}  (keep secret!)"
echo
echo "Server certificate details:"
openssl x509 -in "${SRV_CERT}" -noout -subject -issuer
echo "SANs:"
openssl x509 -in "${SRV_CERT}" -noout -ext subjectAltName 2>/dev/null | tail -1
echo
echo "=========================================="
echo "NEXT STEPS — IMPORTANT"
echo "=========================================="
echo
echo "1. Restart PiCast:"
echo "   sudo systemctl restart picast"
echo
echo "2. Import the CA into Firefox on your Mac/PC:"
echo "   a) Copy ca.pem to your computer:"
echo "      scp pi@${PI_IP}:/etc/picast/tls/ca.pem /tmp/picast-ca.pem"
echo "   b) In Firefox: Settings > Privacy & Security > Certificates > View Certificates"
echo "   c) Click 'Import...' and select /tmp/picast-ca.pem"
echo "   d) Check 'Trust this CA to identify websites' and confirm"
echo
echo "3. Import the CA into Chrome on your Mac/PC:"
echo "   macOS: Double-click ca.pem > Keychain Access > add to System keychain"
echo "          Then set trust to 'Always Trust' for SSL"
echo "   Linux: Settings > Privacy & Security > Security > Manage certificates"
echo "          > Authorities > Import ca.pem"
echo
echo "4. Test: visit https://picast.local:8585/api/health"
echo "   — should show {\"status\":\"ok\"} without any certificate warning"
echo
echo "5. The PiCast browser extension can now connect via HTTPS/WSS"
