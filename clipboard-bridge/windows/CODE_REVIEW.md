# Windows Moonlight clipboard implementation review

Self-review only; independent parent review is still required. No external-model CLI or additional subagent was used.

Verified by execution:
- C# 5 / Windows .NET Framework csc builds with /warnaserror+.
- Core unit tests use injectable fake clipboard and fake IDataObject only: no OS clipboard mutations.
- Content-Length framing; duplicate headers/Transfer-Encoding rejected; absolute deadline and body/header/text bounds; strict UTF8 and NUL rejection.
- Bearer token constant-work comparison; actual TCP peer allowlist ignores X-Forwarded-For.
- No request/clipboard/token/exception payload logging. No shell execution from native clipboard data, keystroke injection or window focus operations.
- InteractiveToken limited task and actual SessionId=1; hidden WinExe; independent bounded retry and persistent child supervisor.
- File ACL inheritance restricted at private root to designated user + SYSTEM + Administrators.
- Firewall exact executable/local IP/local TCP port/remote web-server IP; external non-allowlisted agent host connection failed (connect_ex=11).
- Repeated installation preserves existing config/token, one listening child; dedicated child recovery exercised three times; interactive duplicate instance exits without replacing listener.
- Materialized all-format in-memory guard restores and verifies both explicit-done and autonomous 180-second timeout paths. Unsupported capture aborts before ready.
- Production activation added only one clipboard_bridges entry; Linux mapping, accounts/roles/hosts/pairing/auth preserved; exactly one web-container restart; image and web/streamer binary hashes unchanged.

Review limitations:
- Uninstaller statically reviewed, not executed against the running deployment.
- AtLogOn trigger/principal/settings read back, but real logout/login or reboot deliberately not tested.
- Browser Clipboard API permissions / existing browser production session not tested. Authenticated API E2E ran in a loopback-isolated copy of the identical production image with only a test snapshot password changed.
- In-memory preservation cannot survive force-killing the guard or crashing/rebooting the VM; guard was not terminated and both restoration paths were verified.
- HTTP bridge is trusted-LAN plaintext, not an Internet endpoint.
