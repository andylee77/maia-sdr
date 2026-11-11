"""How the tools reach a scanner that has a password.

An open scanner (no password set) is reached as always: plain HTTP on port 8080, and nothing here
changes a request. A scanner with a password answers only over HTTPS, and a tool only with a
token: its administrator makes one in Settings, Access ("Tokens for tools"). Then:

- `SCANNER_TOKEN` is the token;
- `SCANNER_CA` is the file of the scanner's CA certificate, which makes its HTTPS trusted: save
  `http://<scanner>:8080/ca.crt` and hold its SHA-256 against the one Settings, Access shows.

`base(host)` is where to reach the scanner at `host`; `install(host)` has every request this
process then makes to that host carry the token and trust that CA.
"""

import os
import ssl
import sys
import urllib.parse
import urllib.request

HTTP_PORT = 8080
HTTPS_PORT = 8443


class _Bearer(urllib.request.BaseHandler):
    """Adds the token to HTTPS requests for the scanner's hosts, and to no other: it never
    crosses the network in the clear, and a request to the plain port is sent on to HTTPS
    without it and gets it there."""

    def __init__(self, token, hosts):
        self.token = token
        self.hosts = hosts

    def https_request(self, req):
        host = (req.host or "").rsplit(":", 1)[0].strip("[]")
        if host in self.hosts:
            req.add_unredirected_header("Authorization", f"Bearer {self.token}")
        return req


def token():
    """The token, when the environment has one."""
    return os.environ.get("SCANNER_TOKEN") or None


def host_of(text):
    """The host of `host`, `host:port` or a URL."""
    return urllib.parse.urlparse(text if "://" in text else f"//{text}").hostname


def base(host):
    """The scanner's origin: a URL as it is; else its plain port, or with a token its HTTPS port."""
    if "://" in host:
        return host.rstrip("/")
    name, _, port = host.partition(":")
    if token():
        return f"https://{name}:{port or HTTPS_PORT}"
    return f"http://{name}:{port or HTTP_PORT}"


def context():
    """What trusts the scanner's HTTPS: its CA from SCANNER_CA; None with none given."""
    ca = os.environ.get("SCANNER_CA")
    return ssl.create_default_context(cafile=ca) if ca else None


def install(*hosts):
    """Send SCANNER_TOKEN, when set, with every HTTPS request urllib makes to these hosts, and
    trust the HTTPS of the scanner whose CA SCANNER_CA is, with a token or without."""
    names = {host_of(h) for h in hosts if h}
    handlers = []
    if token() and names:
        handlers.append(_Bearer(token(), names))
    trust = context()
    if trust:
        handlers.append(urllib.request.HTTPSHandler(context=trust))
    elif handlers:
        print("SCANNER_CA is not set: the scanner's HTTPS will not be trusted. Save http://<scanner>:8080/ca.crt, "
              "hold its SHA-256 against Settings, Access, and point SCANNER_CA at the file.", file=sys.stderr)
    if handlers:
        urllib.request.install_opener(urllib.request.build_opener(*handlers))
