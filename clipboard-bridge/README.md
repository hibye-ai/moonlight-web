# Wayland clipboard bridge

The stream HUD has Controls and Clipboard tabs. Opening Clipboard loads the
remote desktop text clipboard; Reload refreshes it, and Copy to this device
copies it into the browser device clipboard. The separate editable field sends
text to the remote clipboard only when Save is pressed, for later Ctrl+V in
any remote application. Reloading never replaces an unfinished draft. Text is
kept in page memory, not saved to browser storage. No remote keystrokes or
background clipboard polling are used. Images and files are not transferred.

Run `bridge.py` as the desktop user with Python 3 and `wl-clipboard` installed.
Install it at `~/.local/share/moonlight-clipboard/bridge.py`, the example user
unit at `~/.config/systemd/user/moonlight-clipboard.service`, and a private
`~/.config/moonlight-clipboard/config.json` with mode 0600:

```json
{
  "bind": "10.23.45.52",
  "port": 47991,
  "allowed_ips": ["10.23.45.150"],
  "token": "REPLACE_WITH_64_RANDOM_HEXADECIMAL_CHARACTERS"
}
```

Enable with `systemctl --user daemon-reload` and
`systemctl --user enable --now moonlight-clipboard`. The service detects the
current user's Wayland socket. A desktop session must be running. Restrict
TCP 47991 in the host firewall to the Moonlight Web server. This helper uses
authenticated HTTP on the private LAN; do not expose it through a public proxy.

The config file must be mode 0600; the bridge refuses to start if it is readable
by group or others, because it contains the bearer token. Validate a staged
config without binding a port or touching the clipboard:

```sh
python3 bridge.py --config ~/.config/moonlight-clipboard/config.json --check-config
```

### Availability hardening

The server enforces, before and during request handling:

- the allowlist at accept time, so a peer outside `allowed_ips` never has its
  request line parsed;
- an absolute per-request deadline (`request_deadline_seconds`, default 5) that
  covers the whole header and body read, not one `recv`;
- an 8 KiB header budget (431 when exceeded) and a bounded 512 KiB body;
- at most `max_concurrency` requests in flight (default 4, 503 when saturated)
  with one request per connection;
- duplicate `Content-Length` and any `Transfer-Encoding` rejected before the
  clipboard is touched.

The two optional keys have validated ranges; omit them to keep the defaults.
Restart the user unit after editing the config.

Add the matching host ID to the web server's private `server/config.json`:

```json
{
  "clipboard_bridges": {
    "3072409625": {
      "endpoint": "10.23.45.52:47991",
      "token": "THE_SAME_RANDOM_TOKEN"
    }
  }
}
```

Merge this property with the existing configuration, then restart Moonlight
Web. Hosts without an entry show an unavailable message. The token and endpoint
are never returned to browsers. The existing login session and host ownership
checks protect GET and POST `/api/host/clipboard` and POST
`/api/host/clipboard/read`. Both POST endpoints check browser Origin. Reads and
writes accept up to 64 KiB of UTF-8 text and reject null characters. Saving
reports success only after `wl-copy` exits successfully. Clipboard data
is not logged. Unicode and multiline text travel as JSON, outside the short
keyboard-text packet format.

The Dockerfile at the deployment root builds all source normally. When the
already-validated streamer binary must be preserved and only web assets changed,
use `deployment/Dockerfile.preserve-streamer` with an explicit
`--build-arg BASE_IMAGE=<repo>@sha256:<digest>`; it builds only the web server,
records the preserved streamer's SHA-256, and fails if the streamer bytes change.
`deployment/Dockerfile.clipboard` is retired and now fails on purpose.
`deployment/release-manifest.py` records the source/tree/toolchain hashes for a
release; it does not inspect the running container.
