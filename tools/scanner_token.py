"""The scanner's token, for the tools that drive its API.

A scanner with a password answers a tool only with a token: its administrator makes one in
Settings, Access ("Tokens for tools"). Put it in the environment as SCANNER_TOKEN, and
`install(host)` has every request this process then makes to that host carry it. With no
SCANNER_TOKEN nothing changes: an open scanner asks for none.
"""

import os
import urllib.parse
import urllib.request


class _Bearer(urllib.request.BaseHandler):
    """Adds the token to requests for the scanner's hosts, and to no other."""

    def __init__(self, token, hosts):
        self.token = token
        self.hosts = hosts

    def http_request(self, req):
        host = (req.host or "").rsplit(":", 1)[0].strip("[]")
        if host in self.hosts:
            req.add_unredirected_header("Authorization", f"Bearer {self.token}")
        return req

    https_request = http_request


def host_of(text):
    """The host of `host`, `host:port` or a URL."""
    return urllib.parse.urlparse(text if "://" in text else f"//{text}").hostname


def install(*hosts):
    """Send SCANNER_TOKEN, when set, with every urllib request to these hosts."""
    token = os.environ.get("SCANNER_TOKEN")
    names = {host_of(h) for h in hosts if h}
    if token and names:
        urllib.request.install_opener(urllib.request.build_opener(_Bearer(token, names)))
