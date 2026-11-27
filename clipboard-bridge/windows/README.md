# Moonlight Windows text clipboard bridge

Dependency-free C# 5 / .NET Framework WinForms bridge. It runs in the logged-in user's interactive desktop, not SSH session 0, not a SYSTEM service. No keyboard injection, foreground/window changes, audio changes, Sunshine changes, or VM restart are involved.

## Files and deployment

- `Core.cs`: authenticated bounded HTTP framing, Unicode validation, fake-backend seam, clipboard busy retry.
- `Native.cs`: STA native WinForms clipboard, TCP listener, hidden child and persistent supervisor (2-second child restart delay, indefinitely). Supervisor logging is bounded and rotates more than once (see `SupervisorLog.cs`).
- `SupervisorLog.cs`: content-free bounded `supervisor.log` with two retained generations. Rotation replaces the oldest generation instead of failing on an existing target, and rotation failure is reported separately from append failure so recovery history is never silently lost.
- `Snapshot.cs`, `Guard.cs`: verification-only in-memory all-format clipboard preservation. Unknown/unmaterializable formats abort before ready; no original values or fingerprints are written to disk/logs. Verification times out after 180 seconds and restores in `finally`.
- `Tests.cs`: fake-backend / fake IDataObject tests; never accesses the system clipboard.
- `install.ps1`, `uninstall.ps1`: scoped, repeatable installation/removal. Installation preflights config/ACL/compiler, builds and fake-tests in a versioned staging directory, snapshots the complete previous state, and only then runs activation as one transaction. Every native side effect goes through a single injectable operation table, so the failure-injection suite can drive all of it without a real scheduled task, firewall, process or clipboard. Uninstall reports leftover processes/files and fails instead of hiding a partial removal. Both preserve an existing config/token unless `-PurgeConfig` is passed.
- `tests/install.Tests.ps1`: deterministic activation/rollback failure-injection suite (22 scenarios) against a simulated Windows machine.
- `tests/mutation-check.py`: mutation controls proving that suite actually detects each rollback defect it claims to cover.

Installation from an elevated PowerShell, after reviewing these sources:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\install.ps1 -DesktopUser 'WORKSTATION\stream-user' -Bind '10.23.45.109' -WebServer '10.23.45.152'
```

Execution-policy bypass applies to that installer process only; no machine/user policy is changed. The binary itself needs no PowerShell execution policy.

Installed path: `C:\ProgramData\MoonlightClipboard` (binary, Core.dll, private config.json, content-free supervisor.log). Directory inheritance is disabled and only the designated user, SYSTEM and Administrators receive access. Config/binaries inherit that restricted ACL. Treat the config as a secret and never print it in diagnostics.

Scheduled task: `MoonlightClipboard`, user `WORKSTATION\stream-user`, InteractiveToken / limited run level, AtLogOn trigger. No execution time limit, IgnoreNew, 999 scheduler-level supervisor failure retries at one minute; the supervisor separately retries bridge child failures indefinitely. No desktop clipboard is available after logout until a user logs in; actual login/reboot was deliberately not exercised.

Firewall: `MoonlightClipboard-WebServer-Only`, incoming TCP, exact program path, local `10.23.45.109:47991`, remote `10.23.45.152` only, edge traversal blocked. The application independently checks the actual TCP peer, not forwarded-IP headers. HTTP is plaintext on this trusted LAN; do not publish/port-forward it. Tokens stay server-side and are never sent to browsers.

Uninstall with `powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\uninstall.ps1`. This stops/removes only the dedicated task, listener/supervisor processes by exact executable path, and its firewall rule. It leaves the web host mapping and config/token intact so a reinstall can reuse the token. Remove the Windows host mapping privately from the web config if retiring the feature permanently; activation needs a separately approved web restart. The uninstaller is supplied and statically reviewed, not executed against the deployed service.

## Installation transaction and rollback

`install.ps1` runs in four phases, and only the last one can endanger the running helper:

1. **Preflight** - port range, compiler presence, desktop-user SID, and an existing `config.json` whose endpoint/token must already match. Any failure aborts with `aborted-before-change` (exit 1) and nothing is touched.
2. **Staging** - compile `Core.dll`, the bridge, the guard and `Tests.exe` into `.staging-<stamp>`, then run the fake-backend regression suite. The live helper keeps serving.
3. **Backup** - while the helper is still running: copy the artifacts and `config.json` into `.rollback-<stamp>`, capture the SDDL of the install and build directories, export the previous task XML, and capture the previous firewall rule's filters. An unreadable ACL/task XML aborts here rather than mid-rollback, and a task that cannot be exported is refused.
4. **Activation** - one transaction over: stop old helper (and confirm it exited), switch binaries (each copy hash-verified against the staged build), create config only if absent, set the restrictive ACL, replace the named firewall rule, register the task, start it, then verify the supervised child, its session, its executable path and the single listener.

Any activation failure triggers a rollback that terminates *only* the helper this run started, restores binaries/config/ACL/firewall/task from the snapshot (each restore verified), then proves the previous helper is healthy again - or, for a first install, that no helper and no task/firewall/config are left. Every layer is attempted even if an earlier one fails.

Exit codes: `0` installed and verified, `1` aborted before any change, `2` activation failed and the previous state was restored and re-verified, `3` activation failed and the rollback itself was incomplete (the report names each failed layer). A task that was already `Running` but had no visible process is reported in `Gaps` rather than claimed restored.

### Testing without touching a real machine

Every native side effect is a named entry in one operation table (`New-DefaultOperations`); `-Operations` replaces that table wholesale. `tests/install.Tests.ps1` supplies an in-memory simulated Windows machine and injects a failure at each step - compile, regression, backup copy, install copy, config write, ACL, firewall, task registration, task start, postcheck, and rollback-during-restore - asserting the full restore contract each time. It never reaches a real scheduled task, firewall, process or the clipboard; one scenario proves that from the installer's own text.

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tests\install.Tests.ps1   # expect exit 0
```

`tests/mutation-check.py` then reintroduces each original defect (missing firewall/task/config/ACL restore, no snapshot, unverified helper, ignored postcheck, killed pre-existing helper) and requires the suite to go red, so the passing suite is not merely a tautology. Requires `pwsh` or Windows PowerShell plus Python 3.

## HTTP contract

`Authorization: Bearer <64 lowercase hex random token>` and actual TCP peer allowlist required. Bind address is explicit, not 0.0.0.0. All responses contain Content-Length and Cache-Control: no-store; connection closes after one request.

- GET `/clipboard` -> 200 `{ "has_text": boolean, "text": string }`
- POST `/clipboard`, application/json `{ "text": string }` -> 204 after native write succeeds.
- At most 64 KiB UTF-8 text, 512 KiB body, 8192-byte headers, 5-second absolute read deadline, 1-second send timeout. NUL, malformed UTF-8, ambiguous duplicate headers, transfer encoding and malformed framing are rejected. Five bounded 40ms-delayed attempts for clipboard-busy errors; other native errors return sanitized 503.

## Test commands

From this source directory on Windows, using the already-installed .NET Framework compiler:

```powershell
$csc='C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe'
& $csc /nologo /warnaserror+ /target:library /out:Core.dll /r:System.Web.Extensions.dll /r:System.Windows.Forms.dll /r:System.Drawing.dll Core.cs Snapshot.cs SupervisorLog.cs
& $csc /nologo /warnaserror+ /out:Tests.exe /r:Core.dll /r:System.Web.Extensions.dll /r:System.Windows.Forms.dll /r:System.Drawing.dll Tests.cs
.\Tests.exe
```

Run tests/build in a separate directory, never overwrite a loaded deployed DLL. For live E2E, first run ClipboardGuard.exe via a limited InteractiveToken scheduled task in the same session. It stores only ready/done/status/restored metadata beneath `%LOCALAPPDATA%\MoonlightClipboard-build\guard`.

The guard now publishes a lease, and `verify-moonlight.py` refuses to write without one:

- set `MOONLIGHT_CLIPBOARD_GUARD_RUN` to a per-run nonce (the guard generates one if unset) and optionally `MOONLIGHT_CLIPBOARD_GUARD_LEASE_SECONDS` (default 180);
- `ready` contains `ready session=<id> run=<nonce> deadline=<unix-seconds>`;
- the verifier requires `--authorize-destructive-test --guard-dir <staged copy> [--run-nonce <nonce>] [--session-id <id>]`, re-checks the lease immediately before **every** write, and returns exit code 4 when the lease is unusable;
- in every cleanup path create `done`, then require `restored` to be `true`; the verifier returns exit code 5 when restoration is not proven. Remove the temporary guard task afterwards.

Never start test writes if the guard does not become ready, and never run the verifier against a guard directory whose nonce does not match. The guard clears stale `ready`/`done`/`restored` files when it arms a new lease. The 180-second timeout is the final restoration backstop, not permission to terminate the guard. Forced termination or a VM crash cannot be recovered by an in-memory guard.

Run `verify-moonlight.py` without `--authorize-destructive-test` to execute only the read-only checks (auth, Origin, ownership, framing limits, bridge read path). The write round trips are skipped and the process exits 0 on success.

The included integration harness `verify-moonlight.py` is run on the Moonlight Web host using a private config file; it creates a loopback-only, disposable copy of the exact production image and a stripped snapshot with only test-only password authentication. Production passwords, sessions, pairing and auth policy are not changed. This tests real server authentication/origin/ownership, bridge HTTP and — only in authorized mode — the real Windows clipboard in both directions; it does not validate browser Clipboard API permissions or the user's existing production browser session.
