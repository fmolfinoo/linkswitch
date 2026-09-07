# LinkSwitch Blueprint — Completeness Critique (gaps only, prioritised)

Verified during review (first-party): **`wlan_connection_mode_auto` is not a legal input to `WlanConnect`** — learn.microsoft.com/en-us/windows/win32/api/wlanapi/nf-wlanapi-wlanconnect lists "The **wlanConnectionMode** member … is set to **wlan_connection_mode_invalid** or **wlan_connection_mode_auto**" under **ERROR_INVALID_PARAMETER**. Also: "**WlanConnect** returns immediately … a client must register for notifications by calling `WlanRegisterNotification`", and an all-user profile requires *execute* access or the call fails with ERROR_ACCESS_DENIED. This breaks §4.6, which is the primary mitigation for R1 (the blueprint's own "product-breaking, certain" risk). Everything in P0 below flows from that class of gap: the *happy path* is specified, the *degraded paths* mostly are not.

---

## P0 — Blocks the product working at all / silently strands the user

**G1. §4.6 primary Wi-Fi connect path is not a valid API call.** `wlan_connection_mode_auto` returns ERROR_INVALID_PARAMETER by spec, so `connect_auto()` never connects and every switch-to-Wi-Fi aborts at step 2.
→ Enumerate with `WlanGetProfileList` (`WLAN_PROFILE_INFO_LIST`, returned in **preference order**), pick the first profile whose SSID is in the latest `WlanGetNetworkBssList`/`WlanGetAvailableNetworkList`, and call `WlanConnect` with `wlan_connection_mode_profile` + `strProfile = <profile name>` + `dot11_BSS_type_infrastructure`; keep `netsh wlan connect name=…` as fallback.

**G2. No completion/failure signal from `WlanConnect`.** It is asynchronous and the blueprint only polls `wlan_intf_opcode_interface_state` to a 12 s timeout, so "wrong key", "AP out of range", "profile is per-user and worker can't read it", and "still associating" all collapse into one timeout.
→ Register `WlanRegisterNotification(WLAN_NOTIFICATION_SOURCE_ACM, …)` before connecting and classify on `wlan_notification_acm_connection_complete` / `disconnected` (`WLAN_CONNECTION_NOTIFICATION_DATA.wlanReasonCode` → `WlanReasonCodeToString`) so the UI can say *why*.

**G3. Wi-Fi radio off / airplane mode is never checked.** The most common "Wi-Fi won't connect" cause is not WCM; it's the radio.
→ Read `WlanQueryInterface(wlan_intf_opcode_radio_state)` → `WLAN_RADIO_STATE`; if `dot11SoftwareRadioState == dot11_radio_state_off` offer to turn it on via `WlanSetInterface`, and if the **hardware** radio is off say so and disable the Wi-Fi button (nothing software can do).

**G4. No saved Wi-Fi profile / zero WLAN interfaces / `wlan_interface_state_not_ready` are unhandled.** `WlanEnumInterfaces` can legitimately return 0 items, or an interface that is present but not ready.
→ Treat each as a distinct, named pre-flight failure with actionable copy ("Connect to your Wi-Fi once manually first — LinkSwitch reuses your saved network").

**G5. No pre-flight guard on the *Ethernet* button symmetric to the Wi-Fi one.** Clicking "Ethernet" with the cable out parks Wi-Fi at 9000 while Ethernet has no default route — you keep connectivity by luck (Wi-Fi is the only route), but the state is nonsense and the readout will say Ethernet lost.
→ Mirror step 2: if the Ethernet candidate's `MediaConnectState != Connected` or it has no default route, refuse the flip and explain, unless the user confirms.

**G6. Step 4 ("confirm the intended candidate wins") has no defined failure action.** The sequence verifies and then… writes state.json and exits 0/non-zero. There is no rollback, so a failed verification leaves a half-applied machine.
→ Snapshot every touched `(luid, family)` row *before* mutating, and on verification failure restore all of them in-process (scope guard / explicit `rollback()`), then report `VerifyFailed`.

**G7. Crash / hard-terminate mid-apply leaves a permanently parked interface with no torn-state marker.** `AllowHardTerminate=true` + `ExecutionTimeLimit=PT1M` against a 12 s association wait makes this reachable, and `state.json` is only written *after* success.
→ Write `pending.json` **before** the first `SetIpInterfaceEntry` (intent journal: mode, touched LUIDs, prior values, pid, timestamp); the `Restore` task and widget startup both detect a `pending.json` with no matching `state.json` and roll back to the recorded prior values.

**G8. No original-metric backup — "restore" means "set automatic", which is not the same as "put it back".** A user who had manually pinned Ethernet to metric 10 before installing loses that permanently, and R5/T7/uninstall all inherit the bug. `wcm_backup` exists; `metric_backup` does not.
→ On first touch of each `(luid, family)`, persist `{use_automatic, metric}` into `MachineConfig::metric_backup` and restore *that* verbatim on `auto`/uninstall; keep a `touched_luids` set so uninstall also cleans NICs that are no longer candidates.

**G9. `state.json` / `config.json` writes are not atomic and have no schema version.** A crash mid-write, or a v0.2 field addition, silently bricks the readout.
→ Write to `*.tmp` + `MoveFileExW(MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`; add `"schema": 1` to all three files now, with an explicit unknown-version policy.

**G10. Only two exit codes are defined ("2 = AdapterGone, 3 = WifiUnavailable") though §5.4 promises "a distinct code per failure class".** `LastTaskResult` is the *only* channel when the worker dies before it can write state.json.
→ Publish a fixed exit-code table (e.g. 0 ok / 2 adapter gone / 3 wifi unavailable / 4 radio off / 5 no profile / 6 access denied / 7 verify failed / 8 config missing / 9 rollback performed) in the README and in `--status --json`.

**G11. `panic = "abort"` in release + no panic hook = a worker that dies with `0xC0000409` and zero log lines.** The file log is the stated diagnostics channel and it will be empty exactly when it matters.
→ `std::panic::set_hook` that appends payload + location + backtrace to the log (the hook still runs before abort), installed as the first statement in `main`.

---

## P1 — Real machines will hit these

**G12. Hyper-V external vSwitch / teaming inverts the "hardware only" filter.** When Ethernet is bound to an external vSwitch, the physical NIC holds no IP stack and `vEthernet (…)` — a *non*-hardware interface — carries the address and default route. §4.2's `physical` predicate rejects exactly the interface that must be steered.
→ Candidate set = hardware NICs **∪** any interface that currently holds a default route or a gateway; rank hardware first, and always provide "show all adapters" in the picker.

**G13. Multiple Ethernet (dock + onboard + USB NIC) or multiple Wi-Fi adapters have no policy.** §4.2 returns `Vec<Nic>` per kind but §5.4 says "resolve both LUIDs" singular; parking only the *configured* Ethernet lets the second Ethernet win the flip.
→ Park **every** non-selected candidate of the losing kind (and every non-selected candidate of *both* kinds that holds a default route), and specify tie-break/selection in the picker.

**G14. Dock/USB-NIC hot-plug and driver reinstall change the LUID, so the saved config goes stale in normal use** (docked vs undocked is a daily event, not an R9 edge case).
→ Store a candidate *set* keyed by LUID with a `{PnP instance id, MAC, description}` fingerprint for re-matching, plus a "no saved adapter present → auto-pick best current candidate of this kind" fallback instead of hard-failing with code 2.

**G15. "Adapter disabled in Device Manager" is indistinguishable from "adapter removed"** — both vanish from IP Helper and both return `ERROR_FILE_NOT_FOUND`.
→ Say "not present (removed or disabled)" rather than "gone — reconfigure", and don't discard the saved config on a single miss.

**G16. Sleep/resume, DHCP renew, cable replug and VPN transitions have no runtime re-apply path** — T8 tests the behaviour but nothing in the design reacts to it; the `Restore` task only fires at logon.
→ The widget already gets `Notify*Change`; add "intended mode ≠ current verdict" detection with a debounced auto-re-apply (opt-in, max N retries in M minutes to avoid a flap loop), and add a resume/`SessionUnlock` trigger to the Restore task.

**G17. WCM soft-disconnecting Wi-Fi *after* a successful switch is the #1 daily annoyance and is unhandled.** §1.2 documents the 30 s traffic-threshold drop, then §5.4 only connects Wi-Fi *before* the flip.
→ While in wifi mode, watch for Wi-Fi disassociation and either re-trigger `ApplyWifi` (reconnect) or auto-fall-back to Ethernet with a toast; make the choice a setting.

**G18. IPv6 failure is scored as success.** "Overall success == v4 is Applied" means a v6 leg that is *bound* and *fails* still reports OK — and since Windows prefers IPv6 (RFC 6724), traffic can keep egressing the old NIC while the UI says the switch worked.
→ Distinguish `FamilyAbsent` (fine) from `Failed` on a bound family; the latter is a hard failure with rollback.

**G19. Park value 9000 is a hardcoded guess, not a computed guarantee.** With a `route -p add … metric 256`, or a Wi-Fi total that is itself high, 9000 may not be enough — and R15 already admits route metric offsets exist.
→ Compute `park = max(total metric of all other default routes) + margin`, clamp to `[9000, 60000]`, and log the computed value; keep the allowlist as a *floor*, not the only permitted value (which today contradicts `park_metric` being a config field at all).

**G20. Same-subnet LAN consequences are noted for routing but not for *the user's stuff*.** Both NICs on `192.168.1.0/24` means the source address flips: NAS/printer/license-server ACLs bound to the Ethernet IP, and SMB sessions, break on flip.
→ One line of UI copy naming the source-IP change, and state the reassuring converse — inbound RDP/SMB to the Ethernet IP keeps working because Ethernet stays up.

**G21. No network-profile / firewall-profile awareness.** Flipping to a Wi-Fi network classified Public silently changes which firewall rules apply (file sharing, discovery).
→ Show the target network's category and warn once when it differs from Ethernet's.

**G22. No metered-connection awareness.** Switching to a phone hotspot can trigger multi-GB background downloads.
→ Read the cost/metered flag (`INetworkCostManager::GetCost`, or WinRT `ConnectionProfile.GetConnectionCost`) and warn before switching to a metered link.

**G23. Task XML is built by string interpolation with no escaping.** `{USER}`, `{EXE}`, `{EXEDIR}` can contain `&`, `<`, `'` (domain names, `Program Files (x86)`, an OEM path); a malformed doc makes `RegisterTask` fail with an opaque HRESULT.
→ XML-escape all five substitutions; resolve the program folder with `SHGetKnownFolderPath(FOLDERID_ProgramFiles)` rather than `%ProgramFiles%`.

**G24. `ensure_dacl` is named but never specified — and it is the entire R11 mitigation.**
→ Ship the exact SDDL (`D:PAI(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)(A;OICI;0x1200a9;;;BU)`) applied at directory creation via `SECURITY_ATTRIBUTES` from `ConvertStringSecurityDescriptorToSecurityDescriptorW` — which needs the **`Win32_Security_Authorization`** Cargo feature, currently missing from §2's list.

**G25. The unelevated widget cannot write the log it is told to write.** `%ProgramData%\LinkSwitch\logs` is `Users: read` by G24's own DACL.
→ Two logs: worker → `%ProgramData%\LinkSwitch\logs\worker.log`; widget → `%LOCALAPPDATA%\LinkSwitch\logs\widget.log`; `--diagnose` merges them.

**G26. `MultipleInstancesPolicy=Queue` + fast clicking executes stale intents in order.** Click wifi → ethernet → wifi leaves whatever the queue drains last, with no ordering guarantee against the widget's own optimistic UI.
→ Debounce in the UI (ignore clicks while in flight), and stamp each request with a monotonic id the worker writes into state.json so the widget can discard stale results.

**G27. `state.json` is read by the widget while the worker writes it — Windows share-mode violations are likely.**
→ Open for read with `FILE_SHARE_READ|FILE_SHARE_WRITE|FILE_SHARE_DELETE`, and treat a parse error as "unknown", never as an error state.

**G28. Read-modify-write side effects have no detection, only a note.** `NlMtu` re-assert is called out as "no mitigation needed" but a jumbo-frame or VPN-tuned MTU being pinned to 1500 is a serious silent regression.
→ Diff the full row before/after every `Set` in the worker and log any field other than `Metric`/`UseAutomaticMetric` that changed; surface it once in the UI.

**G29. No detection of LinkSwitch residue left by a previous install/crash.** Nothing knows that "metric == 9000, automatic == false, no state.json" is *our* mess.
→ On widget/worker startup, scan for the marker pattern and offer one-click "Reset all interfaces to automatic" (this is `restore.ps1`'s job, done in-app).

---

## P2 — Multi-user, install lifecycle, and the OSS-release surface

**G30. Multi-user is undefined.** Tasks carry the installing user's SID in `<UserId>` and the logon trigger; a second user's widget can trigger nothing, yet the metric state is machine-global so both users fight over it.
→ Ship `--register-user` (self-elevating, per-user task registration), or set a task SDDL granting `TASK_EXECUTE` to a chosen group; document that the mode is machine-wide and show "changed by another session" when state.json's writer ≠ current user.

**G31. Standard (non-admin) users have no story at all.** They cannot install, and "the creator can run the task" only covers the installing account.
→ Document the admin-installs-once flow and set an explicit task security descriptor (`IRegisteredTask::SetSecurityDescriptor`) granting run rights to `BU` or a named group.

**G32. First-run experience is a blank.** After install, nothing specifies how the user chooses which NIC is "Ethernet" and which is "Wi-Fi", what defaults are pre-selected, or what the widget shows when *not installed at all* (someone double-clicks the exe from Downloads).
→ Define three explicit first-run states — not installed (offer Install), installed but unconfigured (picker with auto-detected defaults + preview of the exact changes), configured — and make the picker the landing screen after `--install`.

**G33. No "safety net" on the first switch.** Any mis-click on a machine where Wi-Fi can't actually reach the internet costs the user their connection with no obvious way back.
→ Display-resolution-style confirm: apply, then auto-revert after 15 s unless the user clicks "Keep it" (drive the revert from the worker's own timer so it survives a widget crash).

**G34. Widget autostart is never specified.** A tray widget that doesn't come back after reboot is not a widget.
→ Add a per-user `Run`-key entry (or a non-elevated logon task) toggled by a settings checkbox, written by the *unelevated* widget to HKCU.

**G35. No Apps & Features entry, no upgrade/migration path, no "app is running" handling on uninstall.**
→ Write `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\LinkSwitch` on install; make `--install` detect and update existing tasks + migrate config in place; have `--uninstall` signal the running widget to exit before deleting `%ProgramFiles%`.

**G36. No code-signing / SmartScreen story for an unsigned OSS exe that asks for UAC.** This is the single biggest adoption blocker for this category of tool.
→ Decide and document (self-signed + published thumbprint, sigstore, or a signed release via a CI signing service), plus a winget/scoop manifest so users install through a trusted channel.

**G37. No `SECURITY.md` / threat model, despite R11 being a self-declared severe risk.** Note that *any* medium-IL process running as that user can call `IRegisteredTask::Run("ApplyWifi")` — acceptable, but it must be stated, not discovered.
→ Ship `SECURITY.md` covering the elevated-task design, what an attacker gains (a metric flip, nothing else), and the fixed-verb rationale.

**G38. No supply-chain or CI gates.**
→ GitHub Actions on `windows-latest`: `cargo fmt --check`, `clippy -D warnings`, `cargo deny`/`audit`, MSRV build, release artifact + SHA256.

**G39. No unit tests — the plan is 12 manual gates on one machine.** Every pure function here (verdict computation from a route+interface snapshot, park-value computation, XML escaping, config migration, exit-code mapping, LUID round-trip) is testable with zero hardware.
→ Put the snapshot behind a `NetSnapshot` struct produced by a trait, add a fake provider, and unit-test `winner()` against fixtures (VPN-hijacked, no-default-route, tie, route-metric-offset, IPv6-only).

**G40. `restore.ps1` lives "beside the repo".** The panic button must be in the repo, referenced from the README, and shipped in `%ProgramFiles%\LinkSwitch\`.
→ Ship it as `tools/restore.ps1` and copy it on install.

**G41. No `--diagnose` / `--status --json`.** OSS bug reports will be screenshots.
→ `--diagnose` emits one shareable text file (adapters, per-family metrics, default routes with totals, verdict, WCM policy, task states + LastTaskResult + action paths, exe path/version, last 200 log lines); `--status --json` for scripting; both with an opt-in SSID/MAC/IP redaction switch (the log contains SSIDs, MACs, IPs and a username by default — say so).

**G42. Untested-machine classes missing from §8.** No Wi-Fi adapter at all (desktop), no Ethernet at all, two Ethernet NICs, docked/undocked, VM (where T3/T10 are unrunnable because VMs have no WLAN), and Windows 10.
→ Add T12 (degraded hardware matrix) and T13 (dock plug/unplug while a mode is applied); note explicitly which gates require physical hardware.

**G43. No test for "the winner's link dies while a mode is applied"** — the reassuring case (Ethernet@9000 still wins when it's the only route) is never verified.
→ Add T14: in wifi mode, disable Wi-Fi → confirm traffic falls back to the parked Ethernet within seconds and the UI says so.

---

## P3 — UX polish that decides whether a daily user keeps it

**G44. The tray icon never reflects state.** Static icon, static tooltip "LinkSwitch" — the entire value of a tray app is glanceability.
→ Two/three icon variants (Ethernet / Wi-Fi / error-or-hijacked) plus a tooltip that reads "LinkSwitch — routing via Wi-Fi (MyNet)".

**G45. Right-click on the tray does nothing in v1.** Users read that as broken within ten seconds.
→ At minimum handle right-click as show/hide; better, enable `tray-icon`'s `common-controls-v6` + `muda` menu with Ethernet / Wi-Fi / Reset / Show / Quit.

**G46. ✕ hides to tray with no first-time hint** — the classic "I closed it and now it's gone" complaint.
→ One-time tray balloon/notification: "LinkSwitch is still running in your tray."

**G47. No global hotkey.** The single most-requested feature for a two-state toggle.
→ Optional `RegisterHotKey` (default e.g. Ctrl+Alt+L) cycling Ethernet ↔ Wi-Fi, configurable, off by default.

**G48. Saved window position is not clamped to the current monitor set.** Unplug the second monitor and the always-on-top widget is invisible forever, with no way to recover but deleting prefs.json.
→ Clamp restored position against `SM_XVIRTUALSCREEN…`/`MonitorFromPoint` on startup and re-center if off-screen; also state whether the persisted coords are logical or physical pixels (PerMonitorV2 makes this ambiguous).

**G49. No progress feedback during the up-to-12 s Wi-Fi association.** The blueprint's own budget is 3 s of polling against a 12 s worker wait, so the UI gives up before the worker does.
→ Extend the poll budget past the worker's association timeout and show live stage text ("Connecting to MyNet… 4s") driven by the worker writing progress into state.json.

**G50. "Auto" is a misleading label.** It means "restore Windows' automatic metrics", but every user will read it as "automatically pick the best link" — and then file a bug when it doesn't.
→ Rename to **"Windows default"** (or "Reset"), keep `--apply auto` as the CLI verb.

**G51. The UI speaks in raw metrics to a non-technical audience.** "metric 9000" is meaningless; the target user asked for "switch my traffic".
→ Lead with plain language ("Ethernet: connected, deprioritised by LinkSwitch") and put metric numbers behind an "Advanced / details" disclosure.

**G52. Status is encoded in a filled/hollow dot — color/shape only.** Fails colour-blind users and egui's weak screen-reader support compounds it.
→ Always pair the indicator with a text label ("← active"), and never rely on hue alone.

**G53. No success/failure toast when the app is hidden to tray.** A flip triggered by hotkey or tray click gives zero feedback.
→ Windows toast (or tray balloon) on completion and on failure, suppressible.

**G54. All strings are hardcoded inline with no localisation seam.**
→ Centralise every user-facing string in one module now; i18n later is then a swap, not a rewrite.

**G55. README omits the honest alternatives and the manual undo.** Trust for a tool that edits network config and installs elevated tasks depends on showing the two-line PowerShell equivalent.
→ README sections: "What this actually does" (`Set-NetIPInterface -InterfaceIndex N -AddressFamily IPv4 -InterfaceMetric 9000`), "How to undo it without the app", "Why not just disable the adapter", plus the DNS / existing-connection / STIG disclosures already drafted.