#!/bin/sh
# Regenerates the test PKI of tests/tls.rs:
#
#   root CA ── server, server2 (localhost), alice, dave (revoked by root.crl)
#     └─ intermediate CA ── bob, carol (revoked by intermediate.crl)
#   other CA ── mallory (not trusted by the broker)
#
# Every certificate and CRL is valid for a hundred years.
set -eu
cd "$(dirname "$0")"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
days=36500

key() {
    openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$1" 2>/dev/null
}

ca_ext="basicConstraints=critical,CA:TRUE
keyUsage=critical,keyCertSign,cRLSign
subjectKeyIdentifier=hash"

# CA name, issuer name (empty for self-signed), subject
ca() {
    key "$work/$1.key"
    printf '%s\n' "$ca_ext" >"$work/$1.ext"
    if [ -z "$2" ]; then
        openssl req -x509 -new -key "$work/$1.key" -subj "$3" -days $days \
            -addext "basicConstraints=critical,CA:TRUE" \
            -addext "keyUsage=critical,keyCertSign,cRLSign" -out "$1.pem"
    else
        openssl req -new -key "$work/$1.key" -subj "$3" -out "$work/$1.csr"
        printf 'authorityKeyIdentifier=keyid\n' >>"$work/$1.ext"
        openssl x509 -req -in "$work/$1.csr" -CA "$2.pem" -CAkey "$work/$2.key" \
            -CAcreateserial -days $days -extfile "$work/$1.ext" -out "$1.pem" 2>/dev/null
    fi
    mkdir -p "$work/$1.db"
    : >"$work/$1.db/index.txt"
    echo 1000 >"$work/$1.db/crlnumber"
    cat >"$work/$1.cnf" <<EOF
[ca]
default_ca = issuer
[issuer]
database = $work/$1.db/index.txt
crlnumber = $work/$1.db/crlnumber
certificate = $1.pem
private_key = $work/$1.key
default_md = sha256
default_crl_days = $days
crl_extensions = crl_ext
[crl_ext]
authorityKeyIdentifier = keyid
EOF
}

# Leaf name, issuer, subject, extended key usage, extra extensions
leaf() {
    key "$1.key"
    openssl req -new -key "$1.key" -subj "$3" -out "$work/$1.csr"
    printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=%s\nauthorityKeyIdentifier=keyid\n%s\n' \
        "$4" "$5" >"$work/$1.ext"
    openssl x509 -req -in "$work/$1.csr" -CA "$2.pem" -CAkey "$work/$2.key" \
        -CAcreateserial -days $days -extfile "$work/$1.ext" -out "$1.pem" 2>/dev/null
}

ca root "" "/CN=Mercurio Test Root CA"
ca intermediate root "/CN=Mercurio Test Intermediate CA"
ca other "" "/CN=Mercurio Untrusted CA"

leaf server root "/CN=localhost" serverAuth "subjectAltName=DNS:localhost"
leaf server2 root "/CN=localhost again" serverAuth "subjectAltName=DNS:localhost"
leaf alice root "/CN=alice/O=Mercurio" clientAuth ""
leaf dave root "/CN=dave" clientAuth ""
leaf bob intermediate "/CN=bob" clientAuth ""
leaf carol intermediate "/CN=carol" clientAuth ""
leaf mallory other "/CN=mallory" clientAuth ""

# Clients issued by the intermediate present it after their own certificate
cat intermediate.pem >>bob.pem
cat intermediate.pem >>carol.pem

openssl ca -config "$work/root.cnf" -revoke dave.pem 2>/dev/null
openssl ca -config "$work/root.cnf" -gencrl -out root.crl 2>/dev/null
openssl ca -config "$work/intermediate.cnf" -revoke carol.pem 2>/dev/null
openssl ca -config "$work/intermediate.cnf" -gencrl -out intermediate.crl 2>/dev/null
rm -f ./*.srl
