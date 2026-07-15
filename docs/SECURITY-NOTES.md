# SECURITY NOTES — Attack surface (PurrPause v1.0.0-b.1)

> Companion document to the design constitution. Enumerates every known
> avenue for a child with admin privileges to disable PurrPause on their
> own machine, the mitigation applied (if any), and the v1 decision.

## Threat model

- **Adversary** : child with a local admin account on the Windows
  machine. Motivated, casually technical, comfortable Googling.
- **Not modeled** : professional attacker, kernel-mode techniques,
  physical access to unlock the disk, network attack.
- **Not defended** : rootkit techniques, Protected Process Light (PPL),
  Windows Defender App Control policy, kernel driver enforcement.
- **Design philosophy** : « friction, not fort-knox ». Make the app
  tedious to disable without pretending it's inviolable. If the child
  spends 30 min figuring out the uninstall path, they've learned
  something ; if they can nuke it with one command they've won nothing.

## Naming conventions relevant to attackers

- **Service name** : `WindowsSystemHealth` (registered in SCM).
- **Service display name** : `Windows Session Health Service`.
- **Scheduled Task** : `\Microsoft\Windows\SystemHealth\HealthCheck`.
- **State directory** : `C:\ProgramData\DiagnosticsCache\` (hidden +
  system attributes, restrictive DACL — Users have read-only).
- **State file** : `C:\ProgramData\DiagnosticsCache\state.dat` (DPAPI
  machine-encrypted, contains passcode hash + config).
- **Uninstall marker** (v1) : HKLM DWORD under
  `SOFTWARE\Microsoft\Windows\CurrentVersion\Diagnostics\SessionHealth\Uninstalled`.

All names are bland and telemetry-sounding to avoid triggering
curiosity in a casual services.msc / taskschd.msc browse.

## Difficulty rating (from a child's perspective)

- **Trivial** : one command or one click, no research.
- **Facile** : a few commands, requires knowing the app's presence.
- **Modéré** : requires reading docs / Googling multiple concepts.
- **Difficile** : requires research + tool download (Process Hacker,
  reverse-engineering).

## Known holes

### 1. `del C:\ProgramData\DiagnosticsCache\state.dat`
- **Difficulté** : Trivial (1 admin command).
- **Status v1** : **Mitigated** via HKLM « Uninstalled » marker (see
  `platform/registry.rs::should_watchdog_bail`).
- **Post-mitigation** : `del state.dat` alone → next watchdog tick sees
  state.dat missing + marker absent → treats as tampering → falls
  through to normal SCM classifier → service resurrected with default
  config (passcode reset). Parent notices the passcode reset on their
  next Config UI visit and re-locks.
- **Residual friction** : child must ALSO delete the HKLM marker
  (`reg delete HKLM\SOFTWARE\Microsoft\...\Diagnostics\SessionHealth /v Uninstalled /f`),
  which requires knowing the marker exists and its exact path.

### 2. Task Manager → « End task » on the service process
- **Difficulté** : Trivial (Ctrl+Shift+Esc, Details tab, right-click, End task).
- **Status v1** : **Mitigated** via watchdog resurrection (session 3+7).
- **Post-mitigation** : within 60 s (`<TimeTrigger>` cadence of the
  Scheduled Task) the watchdog detects the SCM state is Stopped and
  calls `svc.start()`.
- **Residual** : brief 0-60 s window where the app is dormant. Child
  cannot benefit meaningfully unless they can also disable the
  Scheduled Task (see hole #5).

### 3. `sc.exe delete WindowsSystemHealth`
- **Difficulté** : Facile (needs to know the service name).
- **Status v1** : **Mitigated** by watchdog Reinstall branch (see
  `modes/watchdog.rs::WatchdogAction::Reinstall`) IF state.dat and
  Scheduled Task both intact.
- **Post-mitigation** : next watchdog tick sees SCM absent → calls
  `install::register_service` → service back within 60 s.
- **Residual** : child needs to disable Scheduled Task AND delete
  state.dat AND set the marker (or delete the exe outright) to
  chain-attack.

### 4. `services.msc` → right-click WindowsSystemHealth → Properties
       → Startup type: Disabled
- **Difficulté** : Facile (services.msc is discoverable).
- **Status v1** : **Not detected**. The watchdog can still call
  `svc.start()` on a Disabled service, but SCM refuses with
  `ERROR_SERVICE_DISABLED` (1058) and the tick logs a warning without
  further action.
- **Post-v1 candidate** : watchdog could inspect StartType via
  `QueryServiceConfigW` and reset it to `AutoStart` if it's been
  toggled to `Disabled`. ~30 lines. Post-v1 because it doesn't defeat
  a child who can also disable the Task (see hole #5) — the fix
  matters only as part of a broader anti-tamper layer.

### 5. `taskschd.msc` → Microsoft\Windows\SystemHealth\HealthCheck
       → Disable
- **Difficulté** : Modéré (Task Scheduler is less familiar than
  services.msc ; the task lives under the Microsoft\Windows tree
  requiring some navigation).
- **Status v1** : **Not detected**. Without the task firing every
  60 s, the watchdog never runs ; the service can then be stopped
  without resurrection.
- **Post-v1 candidate** : add a companion Scheduled Task at a
  different path (e.g. `\Microsoft\Windows\SystemHealth\HealthCheck2`)
  registered by the service itself at startup, so disabling one
  doesn't disable both. Complexity : cross-signaling between two
  tasks is easy to get wrong ; deferred until we see this attack
  actually used.

### 6. Safe Mode boot → disable / delete everything
- **Difficulté** : Facile (Shift+Restart from login screen → WinRE →
  Advanced options → Startup Settings → 4 for Safe Mode, no admin
  needed).
- **Status v1** : **Accepted as known limit**. In Safe Mode, the
  `WindowsSystemHealth` service is NOT started (not in the SafeBoot
  minimal set) ; the Scheduled Task doesn't fire ; every mitigation
  above is inert. Child can `del state.dat` / `sc delete` /
  `schtasks /delete` / `reg delete` unopposed, then reboot into
  normal mode with the app gone.
- **Irrémédiable** at v1 ambition. Boot-start service class requires
  signed kernel driver ; GPO-restricted Safe Mode requires Windows
  Pro/Enterprise ; child has admin so GPO is contournable anyway on
  Home.
- **Cheap partial mitigation post-v1** : add
  `HKLM\SYSTEM\CurrentControlSet\Control\SafeBoot\Minimal\WindowsSystemHealth`
  at install so the service DOES start in Safe Mode. Child then also
  needs to know to delete this SafeBoot key. Doesn't defeat a
  determined child but adds one Google search worth of friction.

### 7. Suspend process via Process Hacker (or `NtSuspendProcess`)
- **Difficulté** : Difficile (requires Process Hacker download +
  knowing what to suspend).
- **Status v1** : **Not mitigated**. Suspending the service process
  freezes the service loop (no popup scheduling) but keeps the SCM
  entry Running so the watchdog Nop's. Currently-running popup
  children continue their lifecycle.
- **Post-v1 candidate** : none obvious without kernel-mode
  co-operation. Accepted.

### 8. Google « WindowsSystemHealth service » → discover PurrPause
- **Difficulté** : Trivial once a curious child searches for the
  service name.
- **Status v1** : **Accepted**. Camouflage is defense-in-depth, not
  absolute. If a child researches the service name they find the
  project's public README with the full uninstall procedure — this is
  fine, we're not selling secrecy.
- **Mitigation applied at design time** : we picked
  `WindowsSystemHealth` (bland telemetry name) over
  `PurrPause` / `Wellness` / `Focus` (product-like names that would
  yield the same Google hit immediately).

### 9. `Reset-Clean.bat` shipped in the release zip
- **Difficulté** : Trivial (double-click the .bat if the child has
  the extract of the zip).
- **Status v1** : **Accepted trade-off**. The script requires admin
  (self-elevates via NET SESSION check + PowerShell RunAs) and is
  the intended recovery path for legitimate uninstall / smoke
  iteration. Any child with access to the zip already has the exe +
  every mitigation-defeating knowledge in the zip anyway ; adding
  Reset-Clean.bat doesn't materially change the threat surface.
- **Note** : Reset-Clean.bat scrubs `PendingFileRenameOperations` so
  a subsequent fresh install doesn't land on the landmine that
  motivated session 8 bug #5. Purpose > risk.

## Non-attack scenarios (not holes, worth documenting)

- **WebView2 Evergreen runtime missing** — the popup mode preflight
  detects this and prompts the user to install (`webview2::ensure_available`).
  Not a security issue but a UX one.
- **State.dat corrupt / DPAPI decrypt fails** — `config::load` returns
  an error, install-flow treats as « no config » and prompts fresh
  wizard. Passcode + preferences lost ; app remains locked to the
  new passcode.
- **Watchdog Scheduled Task disabled by Windows update policy** — some
  enterprise GPO configurations block user-installed tasks under
  `\Microsoft\Windows\`. Not applicable to Home / typical target
  environments.

## Mitigation ledger (v1 additions)

| # | Attack | Mitigation | Delivered in |
|---|---|---|---|
| 1 | del state.dat | HKLM Uninstalled marker (two-signal bail) | 0.9.0 (session 10 A2) |
| 2 | Task Manager kill | Watchdog resurrection | 0.3.0 (session 3+7) |
| 3 | sc.exe delete | Watchdog Reinstall branch | 0.6.0 (session 7) |
| — | SERVICE_MARKED_FOR_DELETE race in path_update | ChangeServiceConfigW direct FFI | 0.9.1 (session 10 A3) |
| — | del state.dat before reboot after --uninstall | PendingFileRenameOperations defuse in fresh_install | 0.7.0 (session 8, bug #5) |
| — | Legacy .bat scripts crashed on FR Windows | UTF-8 chcp preamble | 0.7.0 (session 8, bug #1) |
| — | WebView2 orphan worker processes | proc_kill sweep in service loop | 0.8.0 (session 9.5) |

## Post-v1 candidates (documented, not blocking beta)

- Block plain `VK_LWIN` / `VK_RWIN` in the keyboard hook (Start Menu
  leak over popup).
- `Fullscreen::Exclusive` popup mode (trade-off transparency loss).
- Native `.svg` / `.webm` animation support (drop dotlottie-wc for
  simpler animations) — reduces attack surface by ~2 MB of embedded
  WASM/JS bytes.
- Watchdog StartType inspection (hole #4 mitigation).
- Companion Scheduled Task cross-signaling (hole #5 mitigation).
- SafeBoot registry entry (hole #6 partial mitigation).
